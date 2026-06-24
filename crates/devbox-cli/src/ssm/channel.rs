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
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{FutureExt, SinkExt, StreamExt};
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
/// Give up retransmitting a single message after this many attempts (a backstop;
/// the send-stall detector below usually fires first).
const MAX_RESEND_ATTEMPTS: u32 = 3000;
/// Per-message payload size for outgoing input, matching the AWS plugin's 1 KB
/// stream-data framing; larger stdin reads are split into this many bytes.
const STREAM_PAYLOAD_SIZE: usize = 1024;
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

/// If the oldest unacked message stays unanswered this long while inbound is
/// otherwise flowing, the egress path is dead (half-open) — reconnect. This is
/// the send-side analogue of liveness, which only watches the inbound path.
#[cfg(not(test))]
const SEND_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A test send-stall window comfortably above the retransmit tests' lifetime.
#[cfg(test)]
const SEND_STALL_TIMEOUT: Duration = Duration::from_secs(1);

/// Why a single connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The peer closed the channel or local stdin reached EOF — do not reconnect.
    Closed,
    /// The connection dropped — the caller should resume and reconnect.
    Dropped,
}

/// What to do with the connection after handling one inbound frame.
enum Flow {
    Continue,
    Close,
    Drop,
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
    /// Stable per-session client id, reused across reconnects.
    client_id: String,
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
            client_id: message::new_client_id(),
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
    /// messages (the resume path). The retransmit counters are reset so the new
    /// connection gets a fresh retransmit/stall budget.
    async fn open_and_resync(&mut self, token_value: &str) -> ChannelResult<()> {
        let json = message::open_data_channel_json(token_value, &self.state.client_id)
            .map_err(ChannelError::Protocol)?;
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
            out.sent_at = now;
            out.last_sent = now;
            out.attempts = 0;
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
        // Linear scan; the unacked buffer stays tiny at interactive SSH volumes.
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
    /// Output is written but not flushed here — the caller coalesces flushes.
    async fn process_payload(&mut self, msg: &ClientMessage) -> ChannelResult<bool> {
        match msg.payload_type {
            PayloadType::Output => {
                self.state
                    .output
                    .write_all(&msg.payload)
                    .await
                    .context("write output stream")
                    .map_err(ChannelError::Transport)?;
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
                // A malformed handshake is deterministic (a reconnect gets the same
                // bytes) and would leave the session ungated, so fail fast rather
                // than silently stalling stdin forever.
                let request =
                    serde_json::from_slice::<message::HandshakeRequestPayload>(&msg.payload)
                        .map_err(|e| {
                            ChannelError::Protocol(anyhow!(
                                "agent sent a malformed handshake request: {e}"
                            ))
                        })?;
                let payload = message::handshake_response_payload(&request)
                    .map_err(ChannelError::Protocol)?;
                self.send_input(PayloadType::HandshakeResponse, payload)
                    .await?;
                Ok(false)
            }
            PayloadType::HandshakeComplete => {
                if let Ok(complete) =
                    serde_json::from_slice::<message::HandshakeComplete>(&msg.payload)
                    && !complete.customer_message.is_empty()
                {
                    // Agent-controlled text: strip control characters so it cannot
                    // inject terminal escape sequences into the operator's terminal.
                    eprintln!(
                        "devbox ssm-proxy: {}",
                        sanitize_for_terminal(&complete.customer_message)
                    );
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

    /// Flush buffered output to the local sink.
    async fn flush_output(&mut self) -> ChannelResult<()> {
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

    /// Handle one received WebSocket message, returning the loop control flow.
    async fn handle_ws_message(&mut self, message: Message) -> ChannelResult<Flow> {
        match message {
            // A malformed frame is skipped, not fatal.
            Message::Binary(data) => match ClientMessage::deserialize(data.as_ref()) {
                Ok(parsed) => {
                    if self.handle_incoming(parsed).await? {
                        Ok(Flow::Close)
                    } else {
                        Ok(Flow::Continue)
                    }
                }
                Err(e) => {
                    eprintln!("devbox ssm-proxy: ignoring malformed frame: {e}");
                    Ok(Flow::Continue)
                }
            },
            Message::Text(text) => {
                if self.handle_text(text.as_str()) {
                    Ok(Flow::Close)
                } else {
                    Ok(Flow::Continue)
                }
            }
            Message::Ping(payload) => {
                self.send(Message::Pong(payload), "send pong").await?;
                Ok(Flow::Continue)
            }
            // A transport-level close is a reconnect trigger, not session end —
            // only the `channel_closed` message ends the session; the caller's
            // ResumeSession decides if it is truly over (plugin issues #135/#47).
            Message::Close(_) => Ok(Flow::Drop),
            _ => Ok(Flow::Continue),
        }
    }

    /// Resend unacknowledged messages whose adaptive timeout elapsed. Returns
    /// `true` when the egress path looks stalled and the caller should reconnect.
    async fn resend_timed_out(&mut self) -> ChannelResult<bool> {
        let Some(oldest) = self.state.outgoing.first() else {
            return Ok(false);
        };
        if oldest.sent_at.elapsed() > SEND_STALL_TIMEOUT {
            return Ok(true);
        }
        let rto = self.state.rtt.rto;
        let mut resend: Vec<Bytes> = Vec::with_capacity(self.state.outgoing.len());
        for out in &mut self.state.outgoing {
            if out.last_sent.elapsed() > rto {
                if out.attempts >= MAX_RESEND_ATTEMPTS {
                    return Ok(true);
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
        Ok(false)
    }
}

/// Strip control characters (except tab/newline) from agent-controlled text so
/// it cannot inject terminal escape sequences into the operator's terminal.
fn sanitize_for_terminal(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c == '\t' || *c == '\n' || !c.is_control())
        .collect()
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
    let outcome = connection_loop(&mut channel, token_value, &mut stream, input).await;
    // Output frames are acked to the agent before this writer is flushed, so the
    // agent will not resend them after ResumeSession. Flush on every exit path
    // (dropped, closed, or error) so acked-but-buffered bytes reach stdout
    // instead of being discarded when the connection ends.
    let _flushed = channel.flush_output().await;
    outcome
}

/// Open the connection (replaying unacked data) and pump stdin/stdout until the
/// peer closes ([`Outcome::Closed`]) or the connection drops
/// ([`Outcome::Dropped`]). Buffered output is flushed by the caller on every
/// exit path, so the early returns here may leave bytes in the writer.
async fn connection_loop<S, R, W>(
    channel: &mut Channel<'_, S, W>,
    token_value: &str,
    stream: &mut SplitStream<WebSocketStream<S>>,
    input: &mut R,
) -> Result<Outcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
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
                // Process this frame plus any others immediately ready, then flush
                // once — coalescing per-frame syscalls under heavy output.
                let mut pending = item;
                loop {
                    let Some(item) = pending else { return Ok(Outcome::Dropped) };
                    last_inbound = Instant::now();
                    let message = match item {
                        Ok(message) => message,
                        Err(e) => {
                            eprintln!("devbox ssm-proxy: websocket receive error: {e}");
                            return Ok(Outcome::Dropped);
                        }
                    };
                    match channel.handle_ws_message(message).await {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Close) => return Ok(Outcome::Closed),
                        Ok(Flow::Drop) => return Ok(Outcome::Dropped),
                        Err(e) => return on_channel_error(e, "data channel"),
                    }
                    match stream.next().now_or_never() {
                        Some(ready) => pending = ready,
                        None => break,
                    }
                }
                if let Err(e) = channel.flush_output().await {
                    return on_channel_error(e, "flush output");
                }
            }
            // `AsyncReadExt::read` is cancellation-safe: if another branch wins,
            // no bytes are consumed from stdin, so nothing is lost.
            read = input.read(&mut input_buf),
                if channel.state.can_send && channel.state.handshake_done && !input_eof =>
            {
                match read {
                    // stdin EOF: the local side is done sending, but keep draining
                    // output until the peer closes (supports `ssh host cmd`).
                    Ok(0) => input_eof = true,
                    Ok(n) => {
                        let Some(data) = input_buf.get(..n) else { continue };
                        // Split into <=1 KB frames to match the agent's framing.
                        for chunk in data.chunks(STREAM_PAYLOAD_SIZE) {
                            if let Err(e) = channel
                                .send_input(PayloadType::Output, Bytes::copy_from_slice(chunk))
                                .await
                            {
                                return on_channel_error(e, "send stdin");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("devbox ssm-proxy: stdin read error: {e}");
                        return Ok(Outcome::Closed);
                    }
                }
            }
            _ = resend.tick() => {
                match channel.resend_timed_out().await {
                    Ok(false) => {}
                    Ok(true) => return Ok(Outcome::Dropped),
                    Err(e) => return on_channel_error(e, "retransmit"),
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
            message_id: Uuid::now_v7(),
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

        h.feed.write_all(b"to-remote").await.expect("write feed");
        let input = next_binary(&mut h.server).await;
        assert_eq!(input.message_type, MessageType::InputStreamData);
        assert_eq!(input.payload_type, PayloadType::Output);
        assert_eq!(input.payload.as_ref(), b"to-remote".as_slice());
        send(&mut h.server, agent_ack(input.sequence_number)).await;

        send(&mut h.server, output_bytes(3, b"world")).await;
        send(&mut h.server, output_bytes(2, b"hello")).await;

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
    async fn large_stdin_is_split_into_payload_frames() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        // 2500 bytes -> three frames of 1024 / 1024 / 452.
        let big = vec![b'z'; 2500];
        h.feed.write_all(&big).await.expect("write feed");

        let mut total = 0usize;
        for _ in 0..3 {
            let frame = next_binary(&mut h.server).await;
            assert_eq!(frame.payload_type, PayloadType::Output);
            assert!(frame.payload.len() <= STREAM_PAYLOAD_SIZE);
            total = total.saturating_add(frame.payload.len());
            send(&mut h.server, agent_ack(frame.sequence_number)).await;
        }
        assert_eq!(total, big.len());

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
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        send(&mut h.server, output_bytes(3, b"c")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 3);
        send(&mut h.server, output_bytes(4, b"d")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 4);

        send(&mut h.server, output_bytes(5, b"e")).await;
        send(&mut h.server, output_bytes(2, b"b")).await;

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
    async fn duplicate_sequence_is_reacked_without_double_write() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        send(&mut h.server, output_bytes(2, b"b")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 2);
        let mut buf = [0u8; 1];
        h.capture.read_exact(&mut buf).await.expect("read output");
        assert_eq!(buf.as_slice(), b"b".as_slice());

        send(&mut h.server, output_bytes(2, b"b")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 2);

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
    async fn flag_payload_ends_session() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        send(
            &mut h.server,
            frame(MessageType::OutputStreamData, 2, PayloadType::Flag, b""),
        )
        .await;
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Closed
        );
    }

    #[tokio::test]
    async fn malformed_frame_is_skipped() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        h.server
            .send(Message::Binary(Bytes::from_static(b"short")))
            .await
            .expect("send short");
        send(&mut h.server, output_bytes(2, b"ok")).await;
        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 2);
        let mut buf = [0u8; 2];
        h.capture.read_exact(&mut buf).await.expect("read output");
        assert_eq!(buf.as_slice(), b"ok".as_slice());

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
    async fn malformed_ack_keeps_message_buffered() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        h.feed.write_all(b"keep").await.expect("write feed");
        let input = next_binary(&mut h.server).await;
        assert_eq!(input.payload.as_ref(), b"keep".as_slice());

        send(
            &mut h.server,
            frame(
                MessageType::Acknowledge,
                0,
                PayloadType::Other(0),
                b"{not json",
            ),
        )
        .await;

        // The unacked message is therefore retransmitted.
        let resent = next_binary(&mut h.server).await;
        assert_eq!(resent.sequence_number, input.sequence_number);
        assert_eq!(resent.payload.as_ref(), b"keep".as_slice());

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
    async fn pause_publication_gates_input_until_resume() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        h.server
            .send(Message::text("pause_publication"))
            .await
            .expect("pause");
        h.feed.write_all(b"q").await.expect("write feed");

        let paused =
            tokio::time::timeout(Duration::from_millis(300), next_binary(&mut h.server)).await;
        assert!(paused.is_err(), "input was sent while paused");

        h.server
            .send(Message::text("start_publication"))
            .await
            .expect("resume");
        let input = next_binary(&mut h.server).await;
        assert_eq!(input.payload.as_ref(), b"q".as_slice());

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
    async fn kms_only_handshake_is_refused() {
        let mut h = setup().await;
        let open = next_text(&mut h.server).await;
        let value: serde_json::Value = serde_json::from_str(&open).expect("open json");
        assert_eq!(
            value.get("TokenValue").and_then(serde_json::Value::as_str),
            Some("test-token")
        );

        send(
            &mut h.server,
            output_json(
                0,
                PayloadType::HandshakeRequest,
                serde_json::json!({
                    "AgentVersion": "1.0",
                    "RequestedClientActions": [{ "ActionType": "KMSEncryption" }],
                }),
            ),
        )
        .await;

        assert_eq!(acked_seq(&next_binary(&mut h.server).await), 0);
        let response = next_binary(&mut h.server).await;
        assert_eq!(response.payload_type, PayloadType::HandshakeResponse);
        let body: serde_json::Value = serde_json::from_slice(&response.payload).expect("json");
        let action = body
            .get("ProcessedClientActions")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| a.first())
            .expect("action");
        assert_eq!(
            action.get("ActionType").and_then(serde_json::Value::as_str),
            Some("KMSEncryption")
        );
        assert_eq!(
            action
                .get("ActionStatus")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );

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
    async fn liveness_timeout_triggers_reconnect() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;
        // Stop reading the server: no pongs are sent, so the client's liveness
        // window elapses and it reconnects.
        let outcome = tokio::time::timeout(Duration::from_secs(3), h.handle)
            .await
            .expect("run_connection hung")
            .expect("join");
        assert_eq!(outcome.expect("connection"), Outcome::Dropped);
        drop(h.server);
    }

    #[tokio::test]
    async fn retransmits_unacknowledged_input() {
        let mut h = setup().await;
        complete_handshake(&mut h.server, "test-token").await;

        h.feed.write_all(b"retry-me").await.expect("write feed");
        let first = next_binary(&mut h.server).await;
        assert_eq!(first.payload.as_ref(), b"retry-me".as_slice());

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
        drop(h.server);
        assert_eq!(
            h.handle.await.expect("join").expect("connection"),
            Outcome::Dropped
        );
    }

    #[tokio::test]
    async fn dropped_connection_flushes_buffered_output() {
        // Output is buffered in production (BufWriter over stdout). A frame that
        // is acked but only written into the buffer must still reach the sink
        // when the connection drops in the same batch as a transport close — the
        // agent will not resend acked bytes after ResumeSession.
        let (output_side, mut capture) = tokio::io::duplex(BUF);
        let mut state = SessionState::new(tokio::io::BufWriter::new(output_side));
        let (_feed, mut input) = tokio::io::duplex(BUF);

        let (client, mut server) = ws_pair().await;
        let conn = run_connection(client, "token", &mut state, &mut input);
        let drive = async {
            complete_handshake(&mut server, "token").await;
            // Send the output frame and a transport close back-to-back so they
            // coalesce into one inbound batch; the drain loop returns Dropped on
            // the close, leaving the acked payload unflushed in the buffer.
            send(&mut server, output_bytes(2, b"buffered")).await;
            server.send(Message::Close(None)).await.expect("send close");
        };
        let (outcome, ()) = tokio::join!(conn, drive);
        assert_eq!(outcome.expect("conn"), Outcome::Dropped);

        let mut buf = [0u8; 8];
        capture
            .read_exact(&mut buf)
            .await
            .expect("buffered output reaches the sink on drop");
        assert_eq!(buf.as_slice(), b"buffered".as_slice());
    }

    #[tokio::test]
    async fn server_close_frame_triggers_reconnect() {
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
        let mut state = SessionState::new(tokio::io::sink());
        let (mut feed, mut input) = tokio::io::duplex(BUF);

        let (client1, mut server1) = ws_pair().await;
        let conn1 = run_connection(client1, "token-1", &mut state, &mut input);
        let drive1 = async {
            complete_handshake(&mut server1, "token-1").await;
            feed.write_all(b"x").await.expect("feed");
            let input_msg = next_binary(&mut server1).await;
            assert_eq!(input_msg.payload.as_ref(), b"x".as_slice());
            let seq = input_msg.sequence_number;
            drop(server1);
            seq
        };
        let (outcome1, unacked_seq) = tokio::join!(conn1, drive1);
        assert_eq!(outcome1.expect("conn1"), Outcome::Dropped);
        assert_eq!(
            state.outgoing.len(),
            1,
            "unacked input must survive the drop"
        );

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
        let (output_side, mut capture) = tokio::io::duplex(BUF);
        let mut state = SessionState::new(output_side);
        let (_feed, mut input) = tokio::io::duplex(BUF);

        let (client1, mut server1) = ws_pair().await;
        let conn1 = run_connection(client1, "token-1", &mut state, &mut input);
        let drive1 = async {
            complete_handshake(&mut server1, "token-1").await;
            send(&mut server1, output_bytes(3, b"Y")).await;
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

        let (client2, mut server2) = ws_pair().await;
        let conn2 = run_connection(client2, "token-2", &mut state, &mut input);
        let drive2 = async {
            let _open = next_text(&mut server2).await;
            send(&mut server2, output_bytes(2, b"X")).await;
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

    #[test]
    fn sanitize_strips_control_sequences() {
        let raw = "ok\x1b[31mred\x07\tmsg\n";
        assert_eq!(sanitize_for_terminal(raw), "ok[31mred\tmsg\n");
    }
}
