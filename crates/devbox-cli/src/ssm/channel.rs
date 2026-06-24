//! The SSM data-channel state machine: WebSocket framing, the session-type
//! handshake, reliable transport (sequencing, per-message acknowledgement,
//! out-of-order reordering, retransmission), and the stdin/stdout pump.

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
/// How long an unacknowledged message waits before being resent.
const RETRANSMISSION_TIMEOUT: Duration = Duration::from_millis(200);
/// Give up (fatal) after this many resend attempts on a single message.
const MAX_RESEND_ATTEMPTS: u32 = 3000;
/// stdin read-buffer size; a heap buffer keeps the event-loop future small.
const STDIN_BUF: usize = 32_768;
/// Cap on buffered out-of-order incoming messages.
const INCOMING_BUFFER_CAP: usize = 10_000;

/// An unacknowledged outgoing message held for possible retransmission.
struct Outgoing {
    sequence_number: i64,
    bytes: Vec<u8>,
    last_sent: Instant,
    attempts: u32,
}

/// Owns the WebSocket sink, the local output sink, and all data-channel state.
struct Channel<S, W> {
    sink: SplitSink<WebSocketStream<S>, Message>,
    output: W,
    expected_seq: i64,
    out_seq: i64,
    outgoing: Vec<Outgoing>,
    incoming: BTreeMap<i64, ClientMessage>,
    can_send: bool,
    handshake_done: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin, W: AsyncWrite + Unpin> Channel<S, W> {
    /// Send an `input_stream_data` message, assigning the next sequence number
    /// and buffering it for retransmission.
    async fn send_input(&mut self, payload_type: u32, payload: Bytes) -> Result<()> {
        let bytes = message::input_data(self.out_seq, payload_type, payload).serialize()?;
        self.sink
            .send(Message::Binary(Bytes::from(bytes.clone())))
            .await
            .context("send input_stream_data")?;
        self.outgoing.push(Outgoing {
            sequence_number: self.out_seq,
            bytes,
            last_sent: Instant::now(),
            attempts: 0,
        });
        self.out_seq = self
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

    /// Drop the outgoing message the agent acknowledged.
    fn handle_ack(&mut self, msg: &ClientMessage) -> Result<()> {
        let content: message::AcknowledgeContent =
            serde_json::from_slice(&msg.payload).context("parse acknowledge")?;
        let acked = content.acknowledged_message_sequence_number;
        self.outgoing.retain(|out| out.sequence_number != acked);
        Ok(())
    }

    /// Act on a received payload. Returns `true` when the session should close.
    async fn process_payload(&mut self, msg: &ClientMessage) -> Result<bool> {
        match msg.payload_type {
            payload_type::OUTPUT => {
                self.output
                    .write_all(&msg.payload)
                    .await
                    .context("write output stream")?;
                self.output.flush().await.context("flush output stream")?;
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
                let request: message::HandshakeRequestPayload =
                    serde_json::from_slice(&msg.payload).context("parse handshake request")?;
                let payload = message::handshake_response_payload(&request)?;
                self.send_input(payload_type::HANDSHAKE_RESPONSE, payload)
                    .await?;
                Ok(false)
            }
            payload_type::HANDSHAKE_COMPLETE => {
                if let Ok(complete) =
                    serde_json::from_slice::<message::HandshakeComplete>(&msg.payload)
                    && !complete.customer_message.is_empty()
                {
                    eprintln!("devbox ssm-proxy: {}", complete.customer_message);
                }
                self.handshake_done = true;
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
        match msg.sequence_number.cmp(&self.expected_seq) {
            Ordering::Equal => {
                self.send_ack(&msg, true).await?;
                let mut close = self.process_payload(&msg).await?;
                self.advance_expected()?;
                while let Some(buffered) = self.incoming.remove(&self.expected_seq) {
                    if self.process_payload(&buffered).await? {
                        close = true;
                    }
                    self.advance_expected()?;
                }
                Ok(close)
            }
            Ordering::Greater => {
                self.send_ack(&msg, false).await?;
                if self.incoming.len() < INCOMING_BUFFER_CAP {
                    self.incoming.insert(msg.sequence_number, msg);
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
        self.expected_seq = self
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
                self.handle_ack(&msg)?;
                Ok(false)
            }
            message_type::CHANNEL_CLOSED => Ok(true),
            message_type::START_PUBLICATION => {
                self.can_send = true;
                Ok(false)
            }
            message_type::PAUSE_PUBLICATION => {
                self.can_send = false;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Handle a text frame (flow-control / close signals). Returns `true` to close.
    fn handle_text(&mut self, text: &str) -> bool {
        match text.trim() {
            message_type::START_PUBLICATION => {
                self.can_send = true;
                false
            }
            message_type::PAUSE_PUBLICATION => {
                self.can_send = false;
                false
            }
            message_type::CHANNEL_CLOSED => true,
            _ => false,
        }
    }

    /// Resend unacknowledged messages whose retransmission timeout elapsed.
    async fn resend_timed_out(&mut self) -> Result<()> {
        let mut resend: Vec<Vec<u8>> = Vec::new();
        for out in &mut self.outgoing {
            if out.last_sent.elapsed() > RETRANSMISSION_TIMEOUT {
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

/// Run the data channel over `ws`: open it, complete the handshake, and pipe
/// `input`/`output` until either side closes. In production `input`/`output` are
/// the process's stdin/stdout; tests inject in-memory pipes.
pub(crate) async fn run<S, R, W>(
    ws: WebSocketStream<S>,
    token_value: &str,
    mut input: R,
    output: W,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::text(message::open_data_channel_json(token_value)?))
        .await
        .context("send OpenDataChannel")?;

    let mut channel = Channel {
        sink,
        output,
        expected_seq: 0,
        out_seq: 0,
        outgoing: Vec::new(),
        incoming: BTreeMap::new(),
        can_send: false,
        handshake_done: false,
    };

    let mut input_buf = vec![0u8; STDIN_BUF];
    let mut input_eof = false;
    let mut resend = tokio::time::interval(RESEND_INTERVAL);
    resend.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            item = stream.next() => {
                let Some(item) = item else { break };
                match item.context("websocket receive")? {
                    Message::Binary(data) => {
                        let parsed = ClientMessage::deserialize(data.as_ref())?;
                        if channel.handle_incoming(parsed).await? {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        if channel.handle_text(text.as_str()) {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        channel.sink.send(Message::Pong(payload)).await.context("send pong")?;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            read = input.read(&mut input_buf),
                if channel.can_send && channel.handshake_done && !input_eof =>
            {
                let n = read.context("read input stream")?;
                if n == 0 {
                    input_eof = true;
                } else if let Some(chunk) = input_buf.get(..n) {
                    channel
                        .send_input(payload_type::OUTPUT, Bytes::copy_from_slice(chunk))
                        .await?;
                }
            }
            _ = resend.tick() => {
                channel.resend_timed_out().await?;
            }
        }
    }
    Ok(())
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

    async fn send(server: &mut WebSocketStream<DuplexStream>, bytes: Vec<u8>) {
        server
            .send(Message::Binary(Bytes::from(bytes)))
            .await
            .expect("server send");
    }

    struct Harness {
        server: WebSocketStream<DuplexStream>,
        feed: DuplexStream,
        capture: DuplexStream,
        handle: tokio::task::JoinHandle<Result<()>>,
    }

    async fn setup() -> Harness {
        let (client_io, server_io) = tokio::io::duplex(BUF);
        let (client_res, server_res) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/", client_io),
            tokio_tungstenite::accept_async(server_io),
        );
        let (client_ws, _) = client_res.expect("client handshake");
        let server = server_res.expect("server handshake");

        let (feed, input_side) = tokio::io::duplex(BUF);
        let (output_side, capture) = tokio::io::duplex(BUF);
        let handle =
            tokio::spawn(
                async move { run(client_ws, "test-token", input_side, output_side).await },
            );
        Harness {
            server,
            feed,
            capture,
            handle,
        }
    }

    /// Drive OpenDataChannel + the session-type handshake to completion, leaving
    /// the channel ready to pipe data (`can_send` and `handshake_done` set).
    async fn complete_handshake(h: &mut Harness) {
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

        let ack = next_binary(&mut h.server).await;
        assert_eq!(ack.message_type, message_type::ACKNOWLEDGE);
        let response = next_binary(&mut h.server).await;
        assert_eq!(response.message_type, message_type::INPUT_STREAM_DATA);
        assert_eq!(response.payload_type, payload_type::HANDSHAKE_RESPONSE);
        send(&mut h.server, agent_ack(response.sequence_number)).await;

        send(
            &mut h.server,
            output_json(
                1,
                payload_type::HANDSHAKE_COMPLETE,
                serde_json::json!({ "HandshakeTimeToComplete": 0, "CustomerMessage": "" }),
            ),
        )
        .await;
        let ack = next_binary(&mut h.server).await;
        assert_eq!(ack.message_type, message_type::ACKNOWLEDGE);

        h.server
            .send(Message::text(message_type::START_PUBLICATION))
            .await
            .expect("send start_publication");
    }

    #[tokio::test]
    async fn handshake_reorder_and_pipe() {
        let mut h = setup().await;
        complete_handshake(&mut h).await;

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
        h.handle.await.expect("join").expect("run ok");
    }

    #[tokio::test]
    async fn retransmits_unacknowledged_input() {
        let mut h = setup().await;
        complete_handshake(&mut h).await;

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
        h.handle.await.expect("join").expect("run ok");
    }
}
