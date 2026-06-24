//! The SSM data-channel state machine: WebSocket framing, the session-type
//! handshake, reliable transport (sequencing, per-message acknowledgement,
//! out-of-order reordering, adaptive-RTO retransmission), and the stdin/stdout
//! pump.
//!
//! [`SessionState`] holds everything that must survive a reconnect (sequence
//! numbers, the unacked send buffer, the reorder buffer, the RTT estimator);
//! [`run_connection`] drives one WebSocket connection over that state and
//! reports whether the session closed cleanly or the connection dropped (so the
//! caller can resume — see `crate::ssm::run_proxy`).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::ssm::message::{self, ClientMessage, MessageType, PayloadType};

/// How often the retransmission scheduler scans the unacknowledged buffer.
const RESEND_INTERVAL: Duration = Duration::from_millis(100);
/// Retransmission timeout before any RTT samples (RFC 6298 initial value).
const INITIAL_RTO: Duration = Duration::from_millis(200);
/// Lower clamp on the adaptive retransmission timeout.
const MIN_RTO: Duration = Duration::from_millis(50);
/// Upper clamp on the adaptive retransmission timeout.
const MAX_RTO: Duration = Duration::from_secs(1);
/// Clock-granularity floor for the RTO variance term.
const CLOCK_GRANULARITY: Duration = Duration::from_millis(10);
/// Give up (fatal) after this many resend attempts on a single message.
const MAX_RESEND_ATTEMPTS: u32 = 3000;
/// stdin read-buffer size; a heap buffer keeps the event-loop future small.
const STDIN_BUF: usize = 32_768;
/// Cap on buffered out-of-order incoming messages.
#[cfg(not(test))]
const INCOMING_BUFFER_CAP: usize = 10_000;
/// A small cap under test so the buffer-full path is cheap to exercise.
#[cfg(test)]
const INCOMING_BUFFER_CAP: usize = 2;

/// How often to send a WebSocket keepalive ping, so an idle session survives
/// NAT / load-balancer idle timeouts instead of being silently dropped.
#[cfg(not(test))]
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
/// A short keepalive interval under test so the ping path is cheap to exercise.
#[cfg(test)]
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(50);

/// Treat the connection as dead if nothing is received for this long (~3
/// keepalive intervals with no pong or data), so a black-holed connection is
/// recovered by a reconnect instead of hanging forever (plugin issue #47).
#[cfg(not(test))]
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(180);
/// A short liveness window under test, matching the test keepalive interval.
#[cfg(test)]
const LIVENESS_TIMEOUT: Duration = Duration::from_millis(150);

/// Why a single connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The peer closed the channel or local stdin reached EOF — do not reconnect.
    Closed,
    /// The connection dropped — the caller should resume and reconnect.
    Dropped,
}

/// A data-channel failure, classified for the reconnect loop: transport errors
/// are recoverable by a resume; protocol errors are deterministic and fail fast
/// rather than burning the reconnect budget on a retry that cannot succeed.
enum ChannelError {
    Transport(anyhow::Error),
    Protocol(anyhow::Error),
}

type ChannelResult<T> = std::result::Result<T, ChannelError>;

/// Jacobson/Karels RTT estimator producing an adaptive retransmission timeout.
struct RttEstimator {
    srtt: Duration,
    rttvar: Duration,
    rto: Duration,
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            srtt: Duration::ZERO,
            rttvar: Duration::ZERO,
            rto: INITIAL_RTO,
        }
    }

    /// Fold a new RTT sample in and recompute the timeout (RFC 6298).
    fn update(&mut self, sample: Duration) {
        // Clamp the sample so the `mul_f64` math below cannot overflow.
        let sample = sample.min(MAX_RTO);
        if self.srtt.is_zero() {
            self.srtt = sample;
            self.rttvar = sample.mul_f64(0.5);
        } else {
            let diff = self
                .srtt
                .saturating_sub(sample)
                .max(sample.saturating_sub(self.srtt));
            self.rttvar = self.rttvar.mul_f64(0.75).saturating_add(diff.mul_f64(0.25));
            self.srtt = self
                .srtt
                .mul_f64(0.875)
                .saturating_add(sample.mul_f64(0.125));
        }
        let variance = self.rttvar.mul_f64(4.0).max(CLOCK_GRANULARITY);
        self.rto = self.srtt.saturating_add(variance).clamp(MIN_RTO, MAX_RTO);
    }
}

/// An unacknowledged outgoing message held for possible retransmission.
struct Outgoing {
    sequence_number: i64,
    bytes: Bytes,
    sent_at: Instant,
    last_sent: Instant,
    attempts: u32,
}

/// All data-channel state that must persist across reconnects.
pub(crate) struct SessionState<W> {
    output: W,
    expected_seq: i64,
    out_seq: i64,
    outgoing: Vec<Outgoing>,
    incoming: BTreeMap<i64, ClientMessage>,
    // `can_send` / `handshake_done` persist across a resume on purpose: the
    // session is already established, so we keep publishing rather than risk a
    // stall if the agent does not re-emit `start_publication` on resume. Sending
    // before the agent re-gates is safe — the reliable layer retransmits until it
    // acks. (The exact resume re-gating behavior is pending live verification.)
    can_send: bool,
    handshake_done: bool,
    rtt: RttEstimator,
}

impl<W> SessionState<W> {
    pub(crate) fn new(output: W) -> Self {
        Self {
            output,
            expected_seq: 0,
            out_seq: 0,
            outgoing: Vec::new(),
            incoming: BTreeMap::new(),
            can_send: false,
            handshake_done: false,
            rtt: RttEstimator::new(),
        }
    }
}

/// One WebSocket connection's sink, bound to the persistent session state.
struct Channel<'a, S, W> {
    sink: SplitSink<WebSocketStream<S>, Message>,
    state: &'a mut SessionState<W>,
}

impl<S: AsyncRead + AsyncWrite + Unpin, W: AsyncWrite + Unpin> Channel<'_, S, W> {
    /// Send the OpenDataChannel frame and replay any still-unacked outgoing
    /// messages (the resume path), resetting their timers.
    async fn open_and_resync(&mut self, token_value: &str) -> ChannelResult<()> {
        let json = message::open_data_channel_json(token_value).map_err(ChannelError::Protocol)?;
        self.send(Message::text(json), "send OpenDataChannel")
            .await?;
        let pending: Vec<Bytes> = self
            .state
            .outgoing
            .iter()
            .map(|o| o.bytes.clone())
            .collect();
        for bytes in pending {
            self.send(Message::Binary(bytes), "resync outgoing").await?;
        }
        let now = Instant::now();
        for out in &mut self.state.outgoing {
            out.last_sent = now;
        }
        Ok(())
    }

    /// Send a frame, classifying a failure as a (recoverable) transport error.
    async fn send(&mut self, message: Message, context: &'static str) -> ChannelResult<()> {
        self.sink
            .send(message)
            .await
            .context(context)
            .map_err(ChannelError::Transport)
    }

    /// Send an `input_stream_data` message, assigning the next sequence number
    /// and buffering it for retransmission.
    async fn send_input(&mut self, payload_type: PayloadType, payload: Bytes) -> ChannelResult<()> {
        let serialized = message::input_data(self.state.out_seq, payload_type, payload)
            .serialize()
            .map_err(ChannelError::Protocol)?;
        let bytes = Bytes::from(serialized);
        self.send(Message::Binary(bytes.clone()), "send input_stream_data")
            .await?;
        let now = Instant::now();
        self.state.outgoing.push(Outgoing {
            sequence_number: self.state.out_seq,
            bytes,
            sent_at: now,
            last_sent: now,
            attempts: 0,
        });
        self.state.out_seq = self
            .state
            .out_seq
            .checked_add(1)
            .context("outgoing sequence overflow")
            .map_err(ChannelError::Protocol)?;
        Ok(())
    }

    /// Acknowledge a received message.
    async fn send_ack(
        &mut self,
        incoming: &ClientMessage,
        is_sequential: bool,
    ) -> ChannelResult<()> {
        let serialized = message::acknowledge(incoming, is_sequential)
            .map_err(ChannelError::Protocol)?
            .serialize()
            .map_err(ChannelError::Protocol)?;
        self.send(Message::Binary(Bytes::from(serialized)), "send acknowledge")
            .await
    }

    /// Drop the outgoing message the agent acknowledged, sampling its RTT. A
    /// malformed ack is logged and ignored — the message stays buffered and will
    /// retransmit, which is safer than tearing the session down over one frame.
    fn handle_ack(&mut self, msg: &ClientMessage) {
        let content = match serde_json::from_slice::<message::AcknowledgeContent>(&msg.payload) {
            Ok(content) => content,
            Err(e) => {
                eprintln!("devbox ssm-proxy: ignoring malformed acknowledge: {e}");
                return;
            }
        };
        let acked = content.acknowledged_message_sequence_number;
        if let Some(pos) = self
            .state
            .outgoing
            .iter()
            .position(|out| out.sequence_number == acked)
        {
            // Karn's algorithm: only sample RTT from messages never retransmitted.
            if let Some(out) = self.state.outgoing.get(pos)
                && out.attempts == 0
            {
                self.state.rtt.update(out.sent_at.elapsed());
            }
            self.state.outgoing.remove(pos);
        }
    }

    /// Act on a received payload. Returns `true` when the session should close.
    async fn process_payload(&mut self, msg: &ClientMessage) -> ChannelResult<bool> {
        match msg.payload_type {
            PayloadType::Output => {
                self.write_output(&msg.payload).await?;
                Ok(false)
            }
            PayloadType::StdErr => {
                let mut stderr = tokio::io::stderr();
                stderr
                    .write_all(&msg.payload)
                    .await
                    .context("write stderr")
                    .map_err(ChannelError::Transport)?;
                stderr
                    .flush()
                    .await
                    .context("flush stderr")
                    .map_err(ChannelError::Transport)?;
                Ok(false)
            }
            PayloadType::HandshakeRequest => {
                match serde_json::from_slice::<message::HandshakeRequestPayload>(&msg.payload) {
                    Ok(request) => {
                        let payload = message::handshake_response_payload(&request)
                            .map_err(ChannelError::Protocol)?;
                        self.send_input(PayloadType::HandshakeResponse, payload)
                            .await?;
                    }
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: ignoring malformed handshake request: {e}");
                    }
                }
                Ok(false)
            }
            PayloadType::HandshakeComplete => {
                if let Ok(complete) =
                    serde_json::from_slice::<message::HandshakeComplete>(&msg.payload)
                    && !complete.customer_message.is_empty()
                {
                    eprintln!("devbox ssm-proxy: {}", complete.customer_message);
                }
                self.state.handshake_done = true;
                Ok(false)
            }
            PayloadType::Flag => {
                eprintln!("devbox ssm-proxy: session disconnect flag received");
                Ok(true)
            }
            PayloadType::HandshakeResponse | PayloadType::Other(_) => Ok(false),
        }
    }

    async fn write_output(&mut self, data: &[u8]) -> ChannelResult<()> {
        self.state
            .output
            .write_all(data)
            .await
            .context("write output stream")
            .map_err(ChannelError::Transport)?;
        self.state
            .output
            .flush()
            .await
            .context("flush output stream")
            .map_err(ChannelError::Transport)
    }

    /// Handle an `output_stream_data` message with in-order delivery, acking and
    /// draining any buffered successors. Returns `true` to close the session.
    async fn handle_output_stream(&mut self, msg: ClientMessage) -> ChannelResult<bool> {
        match msg.sequence_number.cmp(&self.state.expected_seq) {
            Ordering::Equal => {
                self.send_ack(&msg, true).await?;
                let mut close = self.process_payload(&msg).await?;
                self.advance_expected()?;
                while let Some(buffered) = self.state.incoming.remove(&self.state.expected_seq) {
                    if self.process_payload(&buffered).await? {
                        close = true;
                    }
                    self.advance_expected()?;
                }
                Ok(close)
            }
            // Future sequence: buffer and ack only if there is room. When the
            // buffer is full we neither store nor ack, so the agent retransmits
            // once we catch up and free space. Acking then dropping would make
            // the agent treat it as received and never resend, stalling the stream.
            Ordering::Greater => {
                if self.state.incoming.len() < INCOMING_BUFFER_CAP {
                    self.send_ack(&msg, false).await?;
                    self.state.incoming.insert(msg.sequence_number, msg);
                }
                Ok(false)
            }
            // Duplicate of an already-processed message: re-ack, ignore payload.
            Ordering::Less => {
                self.send_ack(&msg, true).await?;
                Ok(false)
            }
        }
    }

    fn advance_expected(&mut self) -> ChannelResult<()> {
        self.state.expected_seq = self
            .state
            .expected_seq
            .checked_add(1)
            .context("expected sequence overflow")
            .map_err(ChannelError::Protocol)?;
        Ok(())
    }

    /// Dispatch a parsed binary message. Returns `true` to close the session.
    async fn handle_incoming(&mut self, msg: ClientMessage) -> ChannelResult<bool> {
        match msg.message_type {
            MessageType::OutputStreamData => return self.handle_output_stream(msg).await,
            MessageType::Acknowledge => self.handle_ack(&msg),
            MessageType::ChannelClosed => return Ok(true),
            MessageType::StartPublication => self.state.can_send = true,
            MessageType::PausePublication => self.state.can_send = false,
            MessageType::InputStreamData | MessageType::Other(_) => {}
        }
        Ok(false)
    }

    /// Handle a text frame (flow-control / close signals). Returns `true` to close.
    fn handle_text(&mut self, text: &str) -> bool {
        match MessageType::from_wire(text.trim()) {
            MessageType::StartPublication => {
                self.state.can_send = true;
                false
            }
            MessageType::PausePublication => {
                self.state.can_send = false;
                false
            }
            MessageType::ChannelClosed => true,
            MessageType::InputStreamData
            | MessageType::OutputStreamData
            | MessageType::Acknowledge
            | MessageType::Other(_) => false,
        }
    }

    /// Resend unacknowledged messages whose adaptive timeout elapsed.
    async fn resend_timed_out(&mut self) -> ChannelResult<()> {
        let rto = self.state.rtt.rto;
        let mut resend: Vec<Bytes> = Vec::new();
        for out in &mut self.state.outgoing {
            if out.last_sent.elapsed() > rto {
                if out.attempts >= MAX_RESEND_ATTEMPTS {
                    return Err(ChannelError::Transport(anyhow!(
                        "retransmission limit reached for sequence {}",
                        out.sequence_number
                    )));
                }
                resend.push(out.bytes.clone());
                out.last_sent = Instant::now();
                out.attempts = out.attempts.saturating_add(1);
            }
        }
        for bytes in resend {
            self.send(Message::Binary(bytes), "resend input_stream_data")
                .await?;
        }
        Ok(())
    }
}

/// Map a channel error to a loop outcome: transport errors resume, protocol
/// errors fail fast.
fn on_channel_error(error: ChannelError, context: &str) -> Result<Outcome> {
    match error {
        ChannelError::Transport(e) => {
            eprintln!("devbox ssm-proxy: {context}: {e}");
            Ok(Outcome::Dropped)
        }
        ChannelError::Protocol(e) => Err(e.context(context.to_string())),
    }
}

/// Run one WebSocket connection over `state`: open it (replaying any unacked
/// data), then pipe `input`/`output` until the peer closes ([`Outcome::Closed`])
/// or the connection drops ([`Outcome::Dropped`], the caller should resume). A
/// protocol-invariant violation returns `Err` so the caller fails fast.
pub(crate) async fn run_connection<S, R, W>(
    ws: WebSocketStream<S>,
    token_value: &str,
    state: &mut SessionState<W>,
    input: &mut R,
) -> Result<Outcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (sink, mut stream) = ws.split();
    let mut channel = Channel { sink, state };
    if let Err(e) = channel.open_and_resync(token_value).await {
        return on_channel_error(e, "open data channel");
    }

    let mut input_buf = vec![0u8; STDIN_BUF];
    let mut input_eof = false;
    let mut last_inbound = Instant::now();
    let mut resend = tokio::time::interval(RESEND_INTERVAL);
    resend.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            item = stream.next() => {
                let Some(item) = item else { return Ok(Outcome::Dropped) };
                let message = match item {
                    Ok(message) => message,
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: websocket receive error: {e}");
                        return Ok(Outcome::Dropped);
                    }
                };
                // Any inbound frame (data, pong, ping) proves the peer is alive.
                last_inbound = Instant::now();
                match message {
                    // A malformed frame is skipped, not fatal.
                    Message::Binary(data) => match ClientMessage::deserialize(data.as_ref()) {
                        Ok(parsed) => match channel.handle_incoming(parsed).await {
                            Ok(true) => return Ok(Outcome::Closed),
                            Ok(false) => {}
                            Err(e) => return on_channel_error(e, "data channel"),
                        },
                        Err(e) => eprintln!("devbox ssm-proxy: ignoring malformed frame: {e}"),
                    },
                    Message::Text(text) => {
                        if channel.handle_text(text.as_str()) {
                            return Ok(Outcome::Closed);
                        }
                    }
                    Message::Ping(payload) => {
                        if channel.sink.send(Message::Pong(payload)).await.is_err() {
                            return Ok(Outcome::Dropped);
                        }
                    }
                    // A transport-level close is a reconnect trigger, not session
                    // end — only the `channel_closed` message ends the session.
                    // The caller's ResumeSession decides if it is truly over
                    // (plugin issues #135 / #47).
                    Message::Close(_) => return Ok(Outcome::Dropped),
                    _ => {}
                }
            }
            read = input.read(&mut input_buf),
                if channel.state.can_send && channel.state.handshake_done && !input_eof =>
            {
                match read {
                    // stdin EOF: the local side is done sending, but keep draining
                    // output until the peer closes (supports `ssh host cmd`).
                    Ok(0) => input_eof = true,
                    Ok(n) => {
                        if let Some(chunk) = input_buf.get(..n)
                            && let Err(e) = channel
                                .send_input(PayloadType::Output, Bytes::copy_from_slice(chunk))
                                .await
                        {
                            return on_channel_error(e, "send stdin");
                        }
                    }
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: stdin read error: {e}");
                        return Ok(Outcome::Closed);
                    }
                }
            }
            _ = resend.tick() => {
                if let Err(e) = channel.resend_timed_out().await {
                    return on_channel_error(e, "retransmit");
                }
            }
            _ = keepalive.tick() => {
                if last_inbound.elapsed() > LIVENESS_TIMEOUT {
                    eprintln!(
                        "devbox ssm-proxy: no data received in {LIVENESS_TIMEOUT:?}; reconnecting"
                    );
                    return Ok(Outcome::Dropped);
                }
                if channel.sink.send(Message::Ping(Bytes::new())).await.is_err() {
                    return Ok(Outcome::Dropped);
                }
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use crate::ssm::message::AcknowledgeContent;
    use tokio::io::DuplexStream;
    use uuid::Uuid;

    const BUF: usize = 64 * 1024;

    /// Build an agent->client message with a raw payload.
    fn frame(
        message_type: MessageType,
        sequence_number: i64,
        payload_type: PayloadType,
        payload: &[u8],
    ) -> Vec<u8> {
        ClientMessage {
            message_type,
            sequence_number,
            flags: 0,
            message_id: Uuid::new_v4(),
            payload_type,
            payload: Bytes::copy_from_slice(payload),
        }
        .serialize()
        .expect("serialize agent frame")
    }

    fn output_json(seq: i64, payload_type: PayloadType, value: serde_json::Value) -> Vec<u8> {
        frame(
            MessageType::OutputStreamData,
            seq,
            payload_type,
            &serde_json::to_vec(&value).expect("json"),
        )
    }

    fn output_bytes(seq: i64, payload: &[u8]) -> Vec<u8> {
        frame(
            MessageType::OutputStreamData,
            seq,
            PayloadType::Output,
            payload,
        )
    }

    /// An agent acknowledgement of a client message at `seq`.
    fn agent_ack(seq: i64) -> Vec<u8> {
        let content = serde_json::json!({
            "AcknowledgedMessageType": "input_stream_data",
            "AcknowledgedMessageId": Uuid::nil().to_string(),
            "AcknowledgedMessageSequenceNumber": seq,
            "IsSequentialMessage": true,
        });
        frame(
            MessageType::Acknowledge,
            0,
            PayloadType::Other(0),
            &serde_json::to_vec(&content).expect("json"),
        )
    }

    fn acked_seq(msg: &ClientMessage) -> i64 {
        let content: AcknowledgeContent = serde_json::from_slice(&msg.payload).expect("parse ack");
        content.acknowledged_message_sequence_number
    }

    async fn next_binary(server: &mut WebSocketStream<DuplexStream>) -> ClientMessage {
        loop {
            let msg = server
                .next()
                .await
                .expect("server stream ended")
                .expect("ws error");
            if let Message::Binary(bytes) = msg {
                return ClientMessage::deserialize(&bytes).expect("parse client message");
            }
        }
    }

    async fn next_text(server: &mut WebSocketStream<DuplexStream>) -> String {
        loop {
            let msg = server
                .next()
                .await
                .expect("server stream ended")
                .expect("ws error");
            if let Message::Text(text) = msg {
                return text.as_str().to_string();
            }
        }
    }

    /// Wait for the next WebSocket Ping frame; false if the stream ends first.
    async fn next_ping(server: &mut WebSocketStream<DuplexStream>) -> bool {
        loop {
            match server.next().await {
                Some(Ok(Message::Ping(_))) => return true,
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    }

    async fn send(server: &mut WebSocketStream<DuplexStream>, bytes: Vec<u8>) {
        server
            .send(Message::Binary(Bytes::from(bytes)))
            .await
            .expect("server send");
    }

    /// Establish a client/server WebSocket pair over an in-memory pipe.
    async fn ws_pair() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client_io, server_io) = tokio::io::duplex(BUF);
        let (client_res, server_res) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/", client_io),
            tokio_tungstenite::accept_async(server_io),
        );
        (
            client_res.expect("client handshake").0,
            server_res.expect("server handshake"),
        )
    }

    struct Harness {
        server: WebSocketStream<DuplexStream>,
        feed: DuplexStream,
        capture: DuplexStream,
        handle: tokio::task::JoinHandle<Result<Outcome>>,
    }

    async fn setup() -> Harness {
        let (client_ws, server) = ws_pair().await;
        let (feed, input_side) = tokio::io::duplex(BUF);
        let (output_side, capture) = tokio::io::duplex(BUF);
        let handle = tokio::spawn(async move {
            let mut state = SessionState::new(output_side);
            let mut input = input_side;
            run_connection(client_ws, "test-token", &mut state, &mut input).await
        });
        Harness {
            server,
            feed,
            capture,
            handle,
        }
    }

    /// Drive OpenDataChannel + the session-type handshake to completion, leaving
    /// the channel ready to pipe data (`can_send` and `handshake_done` set).
    async fn complete_handshake(server: &mut WebSocketStream<DuplexStream>, expect_token: &str) {
        let open = next_text(server).await;
        let value: serde_json::Value = serde_json::from_str(&open).expect("open json");
        assert_eq!(
            value.get("TokenValue").and_then(serde_json::Value::as_str),
            Some(expect_token)
        );

        send(
            server,
            output_json(
                0,
                PayloadType::HandshakeRequest,
                serde_json::json!({
                    "AgentVersion": "1.0",
                    "RequestedClientActions": [
                        { "ActionType": "SessionType", "ActionParameters": { "SessionType": "Port" } }
                    ],
                }),
            ),
        )
        .await;

        let ack = next_binary(server).await;
        assert_eq!(ack.message_type, MessageType::Acknowledge);
        let response = next_binary(server).await;
        assert_eq!(response.message_type, MessageType::InputStreamData);
        assert_eq!(response.payload_type, PayloadType::HandshakeResponse);
        send(server, agent_ack(response.sequence_number)).await;

        send(
            server,
            output_json(
                1,
                PayloadType::HandshakeComplete,
                serde_json::json!({ "HandshakeTimeToComplete": 0, "CustomerMessage": "" }),
            ),
        )
        .await;
        let ack = next_binary(server).await;
        assert_eq!(ack.message_type, MessageType::Acknowledge);

        server
            .send(Message::text("start_publication"))
            .await
            .expect("send start_publication");
    }

    #[tokio::test]
    async fn handshake_reorder_and_pipe() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        // stdin bytes become input_stream_data (seq 1; the handshake response was seq 0).
        h.feed.write_all(b"to-remote").await.expect("write feed");
        let input = next_binary(&mut h.server).await;
        assert_eq!(input.message_type, MessageType::InputStreamData);
        assert_eq!(input.payload_type, PayloadType::Output);
        assert_eq!(input.payload.as_ref(), b"to-remote".as_slice());
        send(&mut h.server, agent_ack(input.sequence_number)).await;

        // Out-of-order delivery: seq 3 before seq 2 (expected is 2 post-handshake).
        send(&mut h.server, output_bytes(3, b"world")).await;
        send(&mut h.server, output_bytes(2, b"hello")).await;

        // Both are acked; payloads are written to stdout in order.
        let first_ack = next_binary(&mut h.server).await;
        let second_ack = next_binary(&mut h.server).await;
        assert_eq!(acked_seq(&first_ack), 3);
        assert_eq!(acked_seq(&second_ack), 2);

        let mut buf = [0u8; 10];
        h.capture.read_exact(&mut buf).await.expect("read output");
        assert_eq!(buf.as_slice(), b"helloworld".as_slice());

        h.server
            .send(Message::text("channel_closed"))
            .await
            .expect("send channel_closed");
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Closed
        );
    }

    #[tokio::test]
    async fn full_reorder_buffer_drops_without_acking() {
        // INCOMING_BUFFER_CAP is 2 under cfg(test). Fill it with two future
        // frames, then an overflow frame must be neither buffered nor acked, so
        // the agent will retransmit it rather than treat it as delivered.
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        // expected_seq == 2 after the handshake. Buffer seq 3 and 4 (fills cap).
        send(&mut h.server, output_bytes(3, b"c")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 3);
        send(&mut h.server, output_bytes(4, b"d")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 4);

        // Overflow frame (seq 5) must be dropped silently, then deliver seq 2.
        send(&mut h.server, output_bytes(5, b"e")).await;
        send(&mut h.server, output_bytes(2, b"b")).await;

        // The next ack must be for seq 2 — proving seq 5 was never acknowledged.
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 2);

        let mut buf = [0u8; 3];
        h.capture.read_exact(&mut buf).await.expect("read output");
        assert_eq!(buf.as_slice(), b"bcd".as_slice());

        h.server
            .send(Message::text("channel_closed"))
            .await
            .expect("send channel_closed");
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Closed
        );
    }

    #[tokio::test]
    async fn sends_keepalive_pings() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        // On an otherwise idle channel a keepalive ping must arrive.
        let pinged = tokio::time::timeout(Duration::from_secs(5), next_ping(&mut h.server))
            .await
            .expect("keepalive ping timed out");
        assert!(pinged, "expected a keepalive ping");
        h.server
            .send(Message::text("channel_closed"))
            .await
            .expect("send channel_closed");
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Closed
        );
    }

    #[tokio::test]
    async fn retransmits_unacknowledged_input() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        h.feed.write_all(b"retry-me").await.expect("write feed");
        let first = next_binary(&mut h.server).await;
        assert_eq!(first.payload.as_ref(), b"retry-me".as_slice());

        // Without an ack, the same message is retransmitted after the timeout.
        let resent = next_binary(&mut h.server).await;
        assert_eq!(resent.message_type, MessageType::InputStreamData);
        assert_eq!(resent.sequence_number, first.sequence_number);
        assert_eq!(resent.payload.as_ref(), b"retry-me".as_slice());

        h.server
            .send(Message::text("channel_closed"))
            .await
            .expect("send channel_closed");
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Closed
        );
    }

    #[tokio::test]
    async fn dropped_connection_reports_outcome_dropped() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        // Tear the connection down without a clean close.
        drop(h.server);
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Dropped
        );
    }

    #[tokio::test]
    async fn server_close_frame_triggers_reconnect() {
        // A transport-level WebSocket Close is a reconnect signal, not session
        // end (the session ends only via the `channel_closed` message).
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        h.server
            .send(Message::Close(None))
            .await
            .expect("send close");
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Dropped
        );
    }

    #[tokio::test]
    async fn resume_preserves_state_and_resends_unacked() {
        // One persistent state + input across two connections.
        let mut state = SessionState::new(tokio::io::sink());
        let (mut feed, mut input) = tokio::io::duplex(BUF);

        // --- Connection 1: handshake, send an unacked input, then drop ---
        let (client1, mut server1) = ws_pair().await;
        let conn1 = run_connection(client1, "token-1", &mut state, &mut input);
        let drive1 = async {
            complete_handshake(&mut server1, "token-1").await;
            feed.write_all(b"x").await.expect("feed");
            let input_msg = next_binary(&mut server1).await;
            assert_eq!(input_msg.payload.as_ref(), b"x".as_slice());
            let seq = input_msg.sequence_number;
            drop(server1); // abrupt drop, no close frame
            seq
        };
        let (outcome1, unacked_seq) = tokio::join!(conn1, drive1);
        assert_eq!(outcome1.expect("conn1"), Outcome::Dropped);
        assert_eq!(
            state.outgoing.len(),
            1,
            "unacked input must survive the drop"
        );

        // --- Connection 2: same state; expect re-open + retransmit of the unacked ---
        let (client2, mut server2) = ws_pair().await;
        let conn2 = run_connection(client2, "token-2", &mut state, &mut input);
        let drive2 = async {
            let open = next_text(&mut server2).await;
            let value: serde_json::Value = serde_json::from_str(&open).expect("open json");
            assert_eq!(
                value.get("TokenValue").and_then(serde_json::Value::as_str),
                Some("token-2"),
            );
            let resent = next_binary(&mut server2).await;
            assert_eq!(resent.message_type, MessageType::InputStreamData);
            assert_eq!(resent.sequence_number, unacked_seq);
            assert_eq!(resent.payload.as_ref(), b"x".as_slice());
            server2
                .send(Message::text("channel_closed"))
                .await
                .expect("close");
        };
        let (outcome2, ()) = tokio::join!(conn2, drive2);
        assert_eq!(outcome2.expect("conn2"), Outcome::Closed);
    }

    #[tokio::test]
    async fn resume_redelivers_buffered_gap_in_order() {
        // Connection 1 buffers an out-of-order frame (a gap), then drops.
        // Connection 2 delivers the missing frame; the buffered successor must be
        // drained in order to stdout and not lost or double-written.
        let (output_side, mut capture) = tokio::io::duplex(BUF);
        let mut state = SessionState::new(output_side);
        let (_feed, mut input) = tokio::io::duplex(BUF);

        // --- Connection 1: handshake (expected_seq -> 2), buffer seq 3, drop ---
        let (client1, mut server1) = ws_pair().await;
        let conn1 = run_connection(client1, "token-1", &mut state, &mut input);
        let drive1 = async {
            complete_handshake(&mut server1, "token-1").await;
            send(&mut server1, output_bytes(3, b"Y")).await; // future gap (expected 2)
            assert_eq!(acked_seq(&next_binary(&mut server1).await), 3);
            drop(server1);
        };
        let (outcome1, ()) = tokio::join!(conn1, drive1);
        assert_eq!(outcome1.expect("conn1"), Outcome::Dropped);
        assert_eq!(
            state.incoming.len(),
            1,
            "buffered gap must survive the drop"
        );

        // --- Connection 2: deliver seq 2; client writes "X" then drains "Y" ---
        let (client2, mut server2) = ws_pair().await;
        let conn2 = run_connection(client2, "token-2", &mut state, &mut input);
        let drive2 = async {
            let _open = next_text(&mut server2).await;
            send(&mut server2, output_bytes(2, b"X")).await;
            // Acks seq 2; the buffered seq 3 is drained without a fresh ack.
            assert_eq!(acked_seq(&next_binary(&mut server2).await), 2);
            server2
                .send(Message::text("channel_closed"))
                .await
                .expect("close");
        };
        let (outcome2, ()) = tokio::join!(conn2, drive2);
        assert_eq!(outcome2.expect("conn2"), Outcome::Closed);

        let mut buf = [0u8; 2];
        capture.read_exact(&mut buf).await.expect("read output");
        assert_eq!(buf.as_slice(), b"XY".as_slice());
    }
}
