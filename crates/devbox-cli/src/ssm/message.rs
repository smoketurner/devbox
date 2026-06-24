//! Binary wire format and JSON payloads for the SSM Session Manager data channel.
//!
//! A message is a fixed 120-byte big-endian header ("ClientMessage" /
//! "AgentMessage") followed by a payload. The layout mirrors the AWS
//! `session-manager-plugin` (`clientmessage.go`): the `HeaderLength` field holds
//! 116 (the offset of the payload-length field), but the full header is 120
//! bytes and the payload follows it. The 16-byte `MessageId` stores a UUID with
//! its two 8-byte halves swapped (Java `UUID` convention).

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Value written into the 4-byte `HeaderLength` field (offset of the
/// payload-length field, not the full header size).
const HL_VALUE: u32 = 116;
/// Total header size in bytes; the payload starts here.
const HEADER_LEN: usize = 120;
/// Schema version every message carries.
const SCHEMA_VERSION: u32 = 1;
/// Width of the space-padded `MessageType` field.
const MESSAGE_TYPE_LEN: usize = 32;
/// `Flags` value on acknowledge messages (SYN | FIN).
const FLAGS_ACK: u64 = 3;

const OFF_MESSAGE_TYPE: usize = 4;
const OFF_SCHEMA_VERSION: usize = 36;
const OFF_SEQUENCE: usize = 48;
const OFF_FLAGS: usize = 56;
const OFF_MESSAGE_ID: usize = 64;
const OFF_DIGEST: usize = 80;
const OFF_PAYLOAD_TYPE: usize = 112;
const OFF_PAYLOAD_LEN: usize = 116;

/// Client version advertised in the OpenDataChannel and handshake payloads.
pub(crate) const CLIENT_VERSION: &str = "1.3.0.0";

const ACTION_SUCCESS: u32 = 1;
const ACTION_FAILED: u32 = 2;

/// The `MessageType` header field, modeled so dispatch is exhaustive. Unknown
/// values round-trip via [`MessageType::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessageType {
    InputStreamData,
    OutputStreamData,
    Acknowledge,
    ChannelClosed,
    StartPublication,
    PausePublication,
    Other(String),
}

impl MessageType {
    /// The on-wire string for this type.
    fn as_wire(&self) -> &str {
        match self {
            Self::InputStreamData => "input_stream_data",
            Self::OutputStreamData => "output_stream_data",
            Self::Acknowledge => "acknowledge",
            Self::ChannelClosed => "channel_closed",
            Self::StartPublication => "start_publication",
            Self::PausePublication => "pause_publication",
            Self::Other(raw) => raw.as_str(),
        }
    }

    /// Parse an on-wire string (also used for the text flow-control frames).
    pub(crate) fn from_wire(raw: &str) -> Self {
        match raw {
            "input_stream_data" => Self::InputStreamData,
            "output_stream_data" => Self::OutputStreamData,
            "acknowledge" => Self::Acknowledge,
            "channel_closed" => Self::ChannelClosed,
            "start_publication" => Self::StartPublication,
            "pause_publication" => Self::PausePublication,
            other => Self::Other(other.to_string()),
        }
    }
}

/// The `PayloadType` header field. Unknown values round-trip via
/// [`PayloadType::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadType {
    Output,
    HandshakeRequest,
    HandshakeResponse,
    HandshakeComplete,
    Flag,
    StdErr,
    Other(u32),
}

impl PayloadType {
    fn as_u32(self) -> u32 {
        match self {
            Self::Output => 1,
            Self::HandshakeRequest => 5,
            Self::HandshakeResponse => 6,
            Self::HandshakeComplete => 7,
            Self::Flag => 10,
            Self::StdErr => 11,
            Self::Other(raw) => raw,
        }
    }

    fn from_u32(raw: u32) -> Self {
        match raw {
            1 => Self::Output,
            5 => Self::HandshakeRequest,
            6 => Self::HandshakeResponse,
            7 => Self::HandshakeComplete,
            10 => Self::Flag,
            11 => Self::StdErr,
            other => Self::Other(other),
        }
    }
}

/// A parsed or to-be-serialized data-channel message.
#[derive(Debug, Clone)]
pub(crate) struct ClientMessage {
    pub(crate) message_type: MessageType,
    pub(crate) sequence_number: i64,
    pub(crate) flags: u64,
    pub(crate) message_id: Uuid,
    pub(crate) payload_type: PayloadType,
    pub(crate) payload: Bytes,
}

impl ClientMessage {
    /// Serialize to the wire format, computing the SHA-256 payload digest.
    pub(crate) fn serialize(&self) -> Result<Vec<u8>> {
        let digest = sha256(&self.payload)?;
        let payload_len: u32 = self
            .payload
            .len()
            .try_into()
            .context("payload exceeds u32 length")?;

        let mut buf = Vec::with_capacity(HEADER_LEN.saturating_add(self.payload.len()));
        buf.extend_from_slice(&HL_VALUE.to_be_bytes());

        let mut type_field = [b' '; MESSAGE_TYPE_LEN];
        for (slot, byte) in type_field
            .iter_mut()
            .zip(self.message_type.as_wire().bytes())
        {
            *slot = byte;
        }
        buf.extend_from_slice(&type_field);

        buf.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
        buf.extend_from_slice(&now_millis()?.to_be_bytes());
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.flags.to_be_bytes());
        buf.extend_from_slice(&encode_uuid(&self.message_id));
        buf.extend_from_slice(&digest);
        buf.extend_from_slice(&self.payload_type.as_u32().to_be_bytes());
        buf.extend_from_slice(&payload_len.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        Ok(buf)
    }

    /// Parse a message from the wire format. The payload digest is not
    /// re-validated; we trust the underlying TLS transport.
    pub(crate) fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            bail!("data-channel message too short: {} bytes", buf.len());
        }
        let message_type = read_message_type(
            buf.get(OFF_MESSAGE_TYPE..OFF_SCHEMA_VERSION)
                .context("message_type field")?,
        );
        let sequence_number = i64::from_be_bytes(read_array(buf, OFF_SEQUENCE)?);
        let flags = u64::from_be_bytes(read_array(buf, OFF_FLAGS)?);
        let message_id = decode_uuid(
            buf.get(OFF_MESSAGE_ID..OFF_DIGEST)
                .context("message_id field")?,
        )?;
        let payload_type =
            PayloadType::from_u32(u32::from_be_bytes(read_array(buf, OFF_PAYLOAD_TYPE)?));
        let payload_len = usize::try_from(u32::from_be_bytes(read_array(buf, OFF_PAYLOAD_LEN)?))
            .context("payload length")?;
        let end = HEADER_LEN
            .checked_add(payload_len)
            .context("payload length overflow")?;
        let payload = buf.get(HEADER_LEN..end).context("payload truncated")?;
        Ok(Self {
            message_type,
            sequence_number,
            flags,
            message_id,
            payload_type,
            payload: Bytes::copy_from_slice(payload),
        })
    }
}

/// Build an `input_stream_data` message for the given sequence number.
pub(crate) fn input_data(
    sequence_number: i64,
    payload_type: PayloadType,
    payload: Bytes,
) -> ClientMessage {
    ClientMessage {
        message_type: MessageType::InputStreamData,
        sequence_number,
        flags: 0,
        message_id: Uuid::now_v7(),
        payload_type,
        payload,
    }
}

/// Build an `acknowledge` message for a received message.
pub(crate) fn acknowledge(incoming: &ClientMessage, is_sequential: bool) -> Result<ClientMessage> {
    let content = AcknowledgeContent {
        acknowledged_message_type: incoming.message_type.as_wire().to_string(),
        acknowledged_message_id: incoming.message_id.to_string(),
        acknowledged_message_sequence_number: incoming.sequence_number,
        is_sequential_message: is_sequential,
    };
    Ok(ClientMessage {
        message_type: MessageType::Acknowledge,
        sequence_number: 0,
        flags: FLAGS_ACK,
        message_id: Uuid::now_v7(),
        payload_type: PayloadType::Other(0),
        payload: Bytes::from(serde_json::to_vec(&content).context("serialize acknowledge")?),
    })
}

/// A client identifier for a session, generated once and reused across
/// reconnects so a resumed data channel presents the same `ClientId`.
pub(crate) fn new_client_id() -> String {
    Uuid::now_v7().to_string()
}

/// The JSON for the initial OpenDataChannel WebSocket text frame.
pub(crate) fn open_data_channel_json(token_value: &str, client_id: &str) -> Result<String> {
    let open = OpenDataChannelInput {
        message_schema_version: "1.0".to_string(),
        request_id: Uuid::now_v7().to_string(),
        token_value: token_value.to_string(),
        client_id: client_id.to_string(),
        client_version: CLIENT_VERSION.to_string(),
    };
    serde_json::to_string(&open).context("serialize OpenDataChannel")
}

/// Build the handshake-response payload: accept every requested action except
/// `KMSEncryption`, which we do not support and explicitly mark failed.
pub(crate) fn handshake_response_payload(request: &HandshakeRequestPayload) -> Result<Bytes> {
    let actions = request
        .requested_client_actions
        .iter()
        .map(|action| {
            if action.action_type == "KMSEncryption" {
                ProcessedClientAction {
                    action_type: action.action_type.clone(),
                    action_status: ACTION_FAILED,
                    action_result: None,
                    error: "KMS session encryption is not supported by devbox".to_string(),
                }
            } else {
                ProcessedClientAction {
                    action_type: action.action_type.clone(),
                    action_status: ACTION_SUCCESS,
                    action_result: None,
                    error: String::new(),
                }
            }
        })
        .collect();
    let response = HandshakeResponsePayload {
        client_version: CLIENT_VERSION.to_string(),
        processed_client_actions: actions,
        errors: Vec::new(),
    };
    Ok(Bytes::from(
        serde_json::to_vec(&response).context("serialize handshake response")?,
    ))
}

/// Milliseconds since the Unix epoch.
fn now_millis() -> Result<u64> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    u64::try_from(since.as_millis()).context("timestamp overflow")
}

/// SHA-256 over `data`, via aws-lc-rs.
fn sha256(data: &[u8]) -> Result<[u8; 32]> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data)
        .as_ref()
        .try_into()
        .context("unexpected SHA-256 digest length")
}

/// Encode a UUID into the 16-byte wire field: least-significant 8 bytes first,
/// most-significant 8 bytes second.
fn encode_uuid(id: &Uuid) -> [u8; 16] {
    let bytes = id.as_bytes();
    let mut out = [0u8; 16];
    if let (Some(dst), Some(src)) = (out.get_mut(0..8), bytes.get(8..16)) {
        dst.copy_from_slice(src);
    }
    if let (Some(dst), Some(src)) = (out.get_mut(8..16), bytes.get(0..8)) {
        dst.copy_from_slice(src);
    }
    out
}

/// Decode a UUID from the 16-byte wire field (reverse of [`encode_uuid`]).
fn decode_uuid(wire: &[u8]) -> Result<Uuid> {
    let low = wire.get(0..8).context("uuid low half")?;
    let high = wire.get(8..16).context("uuid high half")?;
    let mut bytes = [0u8; 16];
    if let Some(dst) = bytes.get_mut(0..8) {
        dst.copy_from_slice(high);
    }
    if let Some(dst) = bytes.get_mut(8..16) {
        dst.copy_from_slice(low);
    }
    Ok(Uuid::from_bytes(bytes))
}

/// Read a fixed `N`-byte field at `offset`.
fn read_array<const N: usize>(buf: &[u8], offset: usize) -> Result<[u8; N]> {
    let end = offset.checked_add(N).context("field offset overflow")?;
    buf.get(offset..end)
        .context("field truncated")?
        .try_into()
        .context("field size mismatch")
}

/// Parse the space/null-padded `MessageType` field without allocating for the
/// known types (only an unknown type allocates, via `MessageType::Other`).
fn read_message_type(field: &[u8]) -> MessageType {
    let text = std::str::from_utf8(field)
        .unwrap_or_default()
        .trim_matches([' ', '\0']);
    MessageType::from_wire(text)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OpenDataChannelInput {
    message_schema_version: String,
    request_id: String,
    token_value: String,
    client_id: String,
    client_version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AcknowledgeContent {
    pub(crate) acknowledged_message_type: String,
    pub(crate) acknowledged_message_id: String,
    pub(crate) acknowledged_message_sequence_number: i64,
    pub(crate) is_sequential_message: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct HandshakeRequestPayload {
    pub(crate) requested_client_actions: Vec<RequestedClientAction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct RequestedClientAction {
    pub(crate) action_type: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct HandshakeResponsePayload {
    client_version: String,
    processed_client_actions: Vec<ProcessedClientAction>,
    errors: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessedClientAction {
    action_type: String,
    action_status: u32,
    action_result: Option<serde_json::Value>,
    error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct HandshakeComplete {
    #[serde(default)]
    pub(crate) customer_message: String,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn sample(payload: &[u8]) -> ClientMessage {
        input_data(7, PayloadType::Output, Bytes::copy_from_slice(payload))
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let msg = sample(b"hello ssh");
        let wire = msg.serialize().expect("serialize");
        let back = ClientMessage::deserialize(&wire).expect("deserialize");
        assert_eq!(back.message_type, MessageType::InputStreamData);
        assert_eq!(back.sequence_number, 7);
        assert_eq!(back.flags, 0);
        assert_eq!(back.payload_type, PayloadType::Output);
        assert_eq!(back.message_id, msg.message_id);
        assert_eq!(back.payload.as_ref(), b"hello ssh");
    }

    #[test]
    fn unknown_types_round_trip() {
        assert_eq!(
            MessageType::from_wire("bogus"),
            MessageType::Other("bogus".to_string())
        );
        assert_eq!(PayloadType::from_u32(42), PayloadType::Other(42));
        assert_eq!(PayloadType::Other(42).as_u32(), 42);
    }

    #[test]
    fn header_layout_and_digest() {
        let msg = sample(b"abc");
        let wire = msg.serialize().expect("serialize");
        // HeaderLength field = 116, payload begins at byte 120.
        assert_eq!(wire.get(0..4), Some([0, 0, 0, 116].as_slice()));
        assert_eq!(wire.len(), HEADER_LEN + 3);
        assert_eq!(wire.get(HEADER_LEN..), Some(b"abc".as_slice()));
        // Digest field equals an independent SHA-256 of the payload.
        let expect = sha256(b"abc").expect("digest");
        assert_eq!(
            wire.get(OFF_DIGEST..OFF_PAYLOAD_TYPE),
            Some(expect.as_slice())
        );
    }

    #[test]
    fn message_type_is_space_padded() {
        let wire = acknowledge(&sample(b""), true)
            .expect("ack")
            .serialize()
            .expect("serialize");
        let field = wire
            .get(OFF_MESSAGE_TYPE..OFF_SCHEMA_VERSION)
            .expect("field");
        assert_eq!(field.len(), MESSAGE_TYPE_LEN);
        assert!(field.starts_with(b"acknowledge"));
        assert_eq!(field.get(11..), Some([b' '; 21].as_slice()));
    }

    #[test]
    fn uuid_halves_are_swapped_on_the_wire() {
        let id = Uuid::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let wire = encode_uuid(&id);
        // Low half of the wire = canonical bytes [8..16].
        assert_eq!(
            wire.get(0..8),
            Some([8, 9, 10, 11, 12, 13, 14, 15].as_slice())
        );
        assert_eq!(wire.get(8..16), Some([0, 1, 2, 3, 4, 5, 6, 7].as_slice()));
        assert_eq!(decode_uuid(&wire).expect("decode"), id);
    }

    #[test]
    fn acknowledge_payload_has_expected_json() {
        let incoming = ClientMessage {
            message_type: MessageType::OutputStreamData,
            sequence_number: 42,
            flags: 0,
            message_id: Uuid::nil(),
            payload_type: PayloadType::Output,
            payload: Bytes::new(),
        };
        let ack = acknowledge(&incoming, true).expect("ack");
        assert_eq!(ack.flags, FLAGS_ACK);
        assert_eq!(ack.sequence_number, 0);
        let value: serde_json::Value = serde_json::from_slice(&ack.payload).expect("json");
        assert_eq!(
            value
                .get("AcknowledgedMessageType")
                .and_then(serde_json::Value::as_str),
            Some("output_stream_data")
        );
        assert_eq!(
            value
                .get("AcknowledgedMessageSequenceNumber")
                .and_then(serde_json::Value::as_i64),
            Some(42)
        );
        assert_eq!(
            value
                .get("IsSequentialMessage")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn handshake_response_accepts_session_type_and_refuses_kms() {
        let request = HandshakeRequestPayload {
            requested_client_actions: vec![
                RequestedClientAction {
                    action_type: "SessionType".to_string(),
                },
                RequestedClientAction {
                    action_type: "KMSEncryption".to_string(),
                },
            ],
        };
        let payload = handshake_response_payload(&request).expect("response");
        let value: serde_json::Value = serde_json::from_slice(&payload).expect("json");
        let actions = value
            .get("ProcessedClientActions")
            .and_then(serde_json::Value::as_array)
            .expect("actions");
        assert_eq!(actions.len(), 2);
        let first = actions.first().expect("first action");
        assert_eq!(
            first.get("ActionType").and_then(serde_json::Value::as_str),
            Some("SessionType")
        );
        assert_eq!(
            first
                .get("ActionStatus")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(ACTION_SUCCESS))
        );
        let second = actions.get(1).expect("second action");
        assert_eq!(
            second.get("ActionType").and_then(serde_json::Value::as_str),
            Some("KMSEncryption")
        );
        assert_eq!(
            second
                .get("ActionStatus")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(ACTION_FAILED))
        );
    }

    #[test]
    fn deserialize_rejects_short_buffer() {
        assert!(ClientMessage::deserialize(&[0u8; 10]).is_err());
    }

    proptest::proptest! {
        #[test]
        fn codec_roundtrips_arbitrary_messages(
            seq in proptest::prelude::any::<i64>(),
            payload in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..2048),
        ) {
            let msg = input_data(seq, PayloadType::Output, Bytes::copy_from_slice(&payload));
            let wire = msg.serialize().expect("serialize");
            let back = ClientMessage::deserialize(&wire).expect("deserialize");
            proptest::prop_assert_eq!(back.sequence_number, seq);
            proptest::prop_assert_eq!(back.payload.as_ref(), payload.as_slice());
            proptest::prop_assert_eq!(back.payload_type, PayloadType::Output);
        }

        #[test]
        fn deserialize_never_panics_on_arbitrary_bytes(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512),
        ) {
            // Must return Ok or Err on any input, but never panic.
            ClientMessage::deserialize(&bytes).ok();
        }
    }
}
