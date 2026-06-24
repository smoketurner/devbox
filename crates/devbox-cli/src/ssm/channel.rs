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

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::ssm::message::{self, ClientMessage, message_type, payload_type};

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
    bytes: Vec<u8>,
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
    async fn open_and_resync(&mut self, token_value: &str) -> Result<()> {
        self.sink
            .send(Message::text(message::open_data_channel_json(token_value)?))
            .await
            .context("send OpenDataChannel")?;
        let pending: Vec<Vec<u8>> = self
            .state
            .outgoing
            .iter()
            .map(|o| o.bytes.clone())
            .collect();
        for bytes in pending {
            self.sink
                .send(Message::Binary(Bytes::from(bytes)))
                .await
                .context("resync outgoing")?;
        }
        let now = Instant::now();
        for out in &mut self.state.outgoing {
            out.last_sent = now;
        }
        Ok(())
    }

    /// Send an `input_stream_data` message, assigning the next sequence number
    /// and buffering it for retransmission.
    async fn send_input(&mut self, payload_type: u32, payload: Bytes) -> Result<()> {
        let bytes = message::input_data(self.state.out_seq, payload_type, payload).serialize()?;
        self.sink
            .send(Message::Binary(Bytes::from(bytes.clone())))
            .await
            .context("send input_stream_data")?;
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
            .context("outgoing sequence overflow")?;
        Ok(())
    }

    /// Acknowledge a received message.
    async fn send_ack(&mut self, incoming: &ClientMessage, is_sequential: bool) -> Result<()> {
        let bytes = message::acknowledge(incoming, is_sequential)?.serialize()?;
        self.sink
            .send(Message::Binary(Bytes::from(bytes)))
            .await
            .context("send acknowledge")
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
    async fn process_payload(&mut self, msg: &ClientMessage) -> Result<bool> {
        match msg.payload_type {
            payload_type::OUTPUT => {
                self.state
                    .output
                    .write_all(&msg.payload)
                    .await
                    .context("write output stream")?;
                self.state
                    .output
                    .flush()
                    .await
                    .context("flush output stream")?;
                Ok(false)
            }
            payload_type::STDERR => {
                let mut stderr = tokio::io::stderr();
                stderr
                    .write_all(&msg.payload)
                    .await
                    .context("write stderr")?;
                stderr.flush().await.context("flush stderr")?;
                Ok(false)
            }
            payload_type::HANDSHAKE_REQUEST => {
                match serde_json::from_slice::<message::HandshakeRequestPayload>(&msg.payload) {
                    Ok(request) => {
                        let payload = message::handshake_response_payload(&request)?;
                        self.send_input(payload_type::HANDSHAKE_RESPONSE, payload)
                            .await?;
                    }
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: ignoring malformed handshake request: {e}");
                    }
                }
                Ok(false)
            }
            payload_type::HANDSHAKE_COMPLETE => {
                if let Ok(complete) =
                    serde_json::from_slice::<message::HandshakeComplete>(&msg.payload)
                    && !complete.customer_message.is_empty()
                {
                    eprintln!("devbox ssm-proxy: {}", complete.customer_message);
                }
                self.state.handshake_done = true;
                Ok(false)
            }
            payload_type::FLAG => {
                eprintln!("devbox ssm-proxy: session disconnect flag received");
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Handle an `output_stream_data` message with in-order delivery, acking and
    /// draining any buffered successors. Returns `true` to close the session.
    async fn handle_output_stream(&mut self, msg: ClientMessage) -> Result<bool> {
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

    fn advance_expected(&mut self) -> Result<()> {
        self.state.expected_seq = self
            .state
            .expected_seq
            .checked_add(1)
            .context("expected sequence overflow")?;
        Ok(())
    }

    /// Dispatch a parsed binary message. Returns `true` to close the session.
    async fn handle_incoming(&mut self, msg: ClientMessage) -> Result<bool> {
        match msg.message_type.as_str() {
            message_type::OUTPUT_STREAM_DATA => self.handle_output_stream(msg).await,
            message_type::ACKNOWLEDGE => {
                self.handle_ack(&msg);
                Ok(false)
            }
            message_type::CHANNEL_CLOSED => Ok(true),
            message_type::START_PUBLICATION => {
                self.state.can_send = true;
                Ok(false)
            }
            message_type::PAUSE_PUBLICATION => {
                self.state.can_send = false;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Handle a text frame (flow-control / close signals). Returns `true` to close.
    fn handle_text(&mut self, text: &str) -> bool {
        match text.trim() {
            message_type::START_PUBLICATION => {
                self.state.can_send = true;
                false
            }
            message_type::PAUSE_PUBLICATION => {
                self.state.can_send = false;
                false
            }
            message_type::CHANNEL_CLOSED => true,
            _ => false,
        }
    }

    /// Resend unacknowledged messages whose adaptive timeout elapsed.
    async fn resend_timed_out(&mut self) -> Result<()> {
        let rto = self.state.rtt.rto;
        let mut resend: Vec<Vec<u8>> = Vec::new();
        for out in &mut self.state.outgoing {
            if out.last_sent.elapsed() > rto {
                if out.attempts >= MAX_RESEND_ATTEMPTS {
                    bail!(
                        "retransmission limit reached for sequence {}",
                        out.sequence_number
                    );
                }
                resend.push(out.bytes.clone());
                out.last_sent = Instant::now();
                out.attempts = out.attempts.saturating_add(1);
            }
        }
        for bytes in resend {
            self.sink
                .send(Message::Binary(Bytes::from(bytes)))
                .await
                .context("resend input_stream_data")?;
        }
        Ok(())
    }
}

/// Run one WebSocket connection over `state`: open it (replaying any unacked
/// data), then pipe `input`/`output` until the peer closes ([`Outcome::Closed`])
/// or the connection drops ([`Outcome::Dropped`], the caller should resume).
pub(crate) async fn run_connection<S, R, W>(
    ws: WebSocketStream<S>,
    token_value: &str,
    state: &mut SessionState<W>,
    input: &mut R,
) -> Outcome
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (sink, mut stream) = ws.split();
    let mut channel = Channel { sink, state };
    if let Err(e) = channel.open_and_resync(token_value).await {
        eprintln!("devbox ssm-proxy: failed to open data channel: {e}");
        return Outcome::Dropped;
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
                let Some(item) = item else { return Outcome::Dropped };
                let message = match item {
                    Ok(message) => message,
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: websocket receive error: {e}");
                        return Outcome::Dropped;
                    }
                };
                // Any inbound frame (data, pong, ping) proves the peer is alive.
                last_inbound = Instant::now();
                match message {
                    // A malformed frame is skipped, not fatal. A handler error is
                    // a connection problem → drop so the caller can resume.
                    Message::Binary(data) => match ClientMessage::deserialize(data.as_ref()) {
                        Ok(parsed) => match channel.handle_incoming(parsed).await {
                            Ok(true) => return Outcome::Closed,
                            Ok(false) => {}
                            Err(e) => {
                                eprintln!("devbox ssm-proxy: data channel error: {e}");
                                return Outcome::Dropped;
                            }
                        },
                        Err(e) => eprintln!("devbox ssm-proxy: ignoring malformed frame: {e}"),
                    },
                    Message::Text(text) => {
                        if channel.handle_text(text.as_str()) {
                            return Outcome::Closed;
                        }
                    }
                    Message::Ping(payload) => {
                        if channel.sink.send(Message::Pong(payload)).await.is_err() {
                            return Outcome::Dropped;
                        }
                    }
                    // A transport-level close is a reconnect trigger, not session
                    // end — only the `channel_closed` message ends the session.
                    // The caller's ResumeSession decides if it is truly over
                    // (plugin issues #135 / #47).
                    Message::Close(_) => return Outcome::Dropped,
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
                            && channel
                                .send_input(payload_type::OUTPUT, Bytes::copy_from_slice(chunk))
                                .await
                                .is_err()
                        {
                            return Outcome::Dropped;
                        }
                    }
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: stdin read error: {e}");
                        return Outcome::Closed;
                    }
                }
            }
            _ = resend.tick() => {
                if channel.resend_timed_out().await.is_err() {
                    return Outcome::Dropped;
                }
            }
            _ = keepalive.tick() => {
                if last_inbound.elapsed() > LIVENESS_TIMEOUT {
                    eprintln!(
                        "devbox ssm-proxy: no data received in {LIVENESS_TIMEOUT:?}; reconnecting"
                    );
                    return Outcome::Dropped;
                }
                if channel.sink.send(Message::Ping(Bytes::new())).await.is_err() {
                    return Outcome::Dropped;
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

    /// Build an agent->client message of `message_type` with a raw payload.
    fn frame(
        message_type: &str,
        sequence_number: i64,
        payload_type: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        ClientMessage {
            message_type: message_type.to_string(),
            sequence_number,
            flags: 0,
            message_id: Uuid::new_v4(),
            payload_type,
            payload: Bytes::copy_from_slice(payload),
        }
        .serialize()
        .expect("serialize agent frame")
    }

    fn output_json(seq: i64, payload_type: u32, value: serde_json::Value) -> Vec<u8> {
        frame(
            message_type::OUTPUT_STREAM_DATA,
            seq,
            payload_type,
            &serde_json::to_vec(&value).expect("json"),
        )
    }

    fn output_bytes(seq: i64, payload: &[u8]) -> Vec<u8> {
        frame(
            message_type::OUTPUT_STREAM_DATA,
            seq,
            payload_type::OUTPUT,
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
            message_type::ACKNOWLEDGE,
            0,
            0,
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

    /// Establish a server-side WebSocket over an in-memory pipe and spawn a
    /// `run_connection` client driving fresh session state.
    async fn connect_pair(
        token: &'static str,
    ) -> (
        WebSocketStream<DuplexStream>,
        DuplexStream,
        DuplexStream,
        tokio::task::JoinHandle<Outcome>,
    ) {
        let (client_io, server_io) = tokio::io::duplex(BUF);
        let (client_res, server_res) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/", client_io),
            tokio_tungstenite::accept_async(server_io),
        );
        let (client_ws, _) = client_res.expect("client handshake");
        let server = server_res.expect("server handshake");
        let (feed, input_side) = tokio::io::duplex(BUF);
        let (output_side, capture) = tokio::io::duplex(BUF);
        let handle = tokio::spawn(async move {
            let mut state = SessionState::new(output_side);
            let mut input = input_side;
            run_connection(client_ws, token, &mut state, &mut input).await
        });
        (server, feed, capture, handle)
    }

    struct Harness {
        server: WebSocketStream<DuplexStream>,
        feed: DuplexStream,
        capture: DuplexStream,
        handle: tokio::task::JoinHandle<Outcome>,
    }

    async fn setup() -> Harness {
        let (server, feed, capture, handle) = connect_pair("test-token").await;
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
                payload_type::HANDSHAKE_REQUEST,
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
        assert_eq!(ack.message_type, message_type::ACKNOWLEDGE);
        let response = next_binary(server).await;
        assert_eq!(response.message_type, message_type::INPUT_STREAM_DATA);
        assert_eq!(response.payload_type, payload_type::HANDSHAKE_RESPONSE);
        send(server, agent_ack(response.sequence_number)).await;

        send(
            server,
            output_json(
                1,
                payload_type::HANDSHAKE_COMPLETE,
                serde_json::json!({ "HandshakeTimeToComplete": 0, "CustomerMessage": "" }),
            ),
        )
        .await;
        let ack = next_binary(server).await;
        assert_eq!(ack.message_type, message_type::ACKNOWLEDGE);

        server
            .send(Message::text(message_type::START_PUBLICATION))
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
        assert_eq!(input.message_type, message_type::INPUT_STREAM_DATA);
        assert_eq!(input.payload_type, payload_type::OUTPUT);
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
            .send(Message::text(message_type::CHANNEL_CLOSED))
            .await
            .expect("send channel_closed");
        assert_eq!(h.handle.await.expect("join"), Outcome::Closed);
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
            .send(Message::text(message_type::CHANNEL_CLOSED))
            .await
            .expect("send channel_closed");
        assert_eq!(h.handle.await.expect("join"), Outcome::Closed);
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
            .send(Message::text(message_type::CHANNEL_CLOSED))
            .await
            .expect("send channel_closed");
        assert_eq!(h.handle.await.expect("join"), Outcome::Closed);
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
        assert_eq!(resent.message_type, message_type::INPUT_STREAM_DATA);
        assert_eq!(resent.sequence_number, first.sequence_number);
        assert_eq!(resent.payload.as_ref(), b"retry-me".as_slice());

        h.server
            .send(Message::text(message_type::CHANNEL_CLOSED))
            .await
            .expect("send channel_closed");
        assert_eq!(h.handle.await.expect("join"), Outcome::Closed);
    }

    #[tokio::test]
    async fn dropped_connection_reports_outcome_dropped() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        // Tear the connection down without a clean close.
        drop(h.server);
        assert_eq!(h.handle.await.expect("join"), Outcome::Dropped);
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
        assert_eq!(h.handle.await.expect("join"), Outcome::Dropped);
    }

    #[tokio::test]
    async fn resume_preserves_state_and_resends_unacked() {
        // One persistent state + input across two connections.
        let mut state = SessionState::new(tokio::io::sink());
        let (mut feed, mut input) = tokio::io::duplex(BUF);

        // --- Connection 1: handshake, send an unacked input, then drop ---
        let (c1, s1) = tokio::io::duplex(BUF);
        let (c1_res, s1_res) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/", c1),
            tokio_tungstenite::accept_async(s1),
        );
        let client1 = c1_res.expect("client1").0;
        let mut server1 = s1_res.expect("server1");

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
        assert_eq!(outcome1, Outcome::Dropped);
        assert_eq!(
            state.outgoing.len(),
            1,
            "unacked input must survive the drop"
        );

        // --- Connection 2: same state; expect re-open + retransmit of the unacked ---
        let (c2, s2) = tokio::io::duplex(BUF);
        let (c2_res, s2_res) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/", c2),
            tokio_tungstenite::accept_async(s2),
        );
        let client2 = c2_res.expect("client2").0;
        let mut server2 = s2_res.expect("server2");

        let conn2 = run_connection(client2, "token-2", &mut state, &mut input);
        let drive2 = async {
            let open = next_text(&mut server2).await;
            let value: serde_json::Value = serde_json::from_str(&open).expect("open json");
            assert_eq!(
                value.get("TokenValue").and_then(serde_json::Value::as_str),
                Some("token-2"),
            );
            let resent = next_binary(&mut server2).await;
            assert_eq!(resent.message_type, message_type::INPUT_STREAM_DATA);
            assert_eq!(resent.sequence_number, unacked_seq);
            assert_eq!(resent.payload.as_ref(), b"x".as_slice());
            server2
                .send(Message::text(message_type::CHANNEL_CLOSED))
                .await
                .expect("close");
        };
        let (outcome2, ()) = tokio::join!(conn2, drive2);
        assert_eq!(outcome2, Outcome::Closed);
    }
}
