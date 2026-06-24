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

/// `MessageType` header strings.
pub(crate) mod message_type {
    pub(crate) const INPUT_STREAM_DATA: &str = "input_stream_data";
    pub(crate) const ACKNOWLEDGE: &str = "acknowledge";
    pub(crate) const OUTPUT_STREAM_DATA: &str = "output_stream_data";
    pub(crate) const CHANNEL_CLOSED: &str = "channel_closed";
    pub(crate) const START_PUBLICATION: &str = "start_publication";
    pub(crate) const PAUSE_PUBLICATION: &str = "pause_publication";
}

/// `PayloadType` header values.
pub(crate) mod payload_type {
    pub(crate) const OUTPUT: u32 = 1;
    pub(crate) const HANDSHAKE_REQUEST: u32 = 5;
    pub(crate) const HANDSHAKE_RESPONSE: u32 = 6;
    pub(crate) const HANDSHAKE_COMPLETE: u32 = 7;
    pub(crate) const FLAG: u32 = 10;
    pub(crate) const STDERR: u32 = 11;
}

/// A parsed or to-be-serialized data-channel message.
#[derive(Debug, Clone)]
pub(crate) struct ClientMessage {
    pub(crate) message_type: String,
    pub(crate) sequence_number: i64,
    pub(crate) flags: u64,
    pub(crate) message_id: Uuid,
    pub(crate) payload_type: u32,
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
        for (slot, byte) in type_field.iter_mut().zip(self.message_type.bytes()) {
            *slot = byte;
        }
        buf.extend_from_slice(&type_field);

        buf.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
        buf.extend_from_slice(&now_millis()?.to_be_bytes());
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.flags.to_be_bytes());
        buf.extend_from_slice(&encode_uuid(&self.message_id));
        buf.extend_from_slice(&digest);
        buf.extend_from_slice(&self.payload_type.to_be_bytes());
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
        let message_type = read_string(
            buf.get(OFF_MESSAGE_TYPE..OFF_SCHEMA_VERSION)
                .context("message_type field")?,
        );
        let sequence_number = i64::from_be_bytes(read_array(buf, OFF_SEQUENCE)?);
        let flags = u64::from_be_bytes(read_array(buf, OFF_FLAGS)?);
        let message_id = decode_uuid(
            buf.get(OFF_MESSAGE_ID..OFF_DIGEST)
                .context("message_id field")?,
        )?;
        let payload_type = u32::from_be_bytes(read_array(buf, OFF_PAYLOAD_TYPE)?);
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
pub(crate) fn input_data(sequence_number: i64, payload_type: u32, payload: Bytes) -> ClientMessage {
    ClientMessage {
        message_type: message_type::INPUT_STREAM_DATA.to_string(),
        sequence_number,
        flags: 0,
        message_id: Uuid::new_v4(),
        payload_type,
        payload,
    }
}

/// Build an `acknowledge` message for a received message.
pub(crate) fn acknowledge(incoming: &ClientMessage, is_sequential: bool) -> Result<ClientMessage> {
    let content = AcknowledgeContent {
        acknowledged_message_type: incoming.message_type.clone(),
        acknowledged_message_id: incoming.message_id.to_string(),
        acknowledged_message_sequence_number: incoming.sequence_number,
        is_sequential_message: is_sequential,
    };
    Ok(ClientMessage {
        message_type: message_type::ACKNOWLEDGE.to_string(),
        sequence_number: 0,
        flags: FLAGS_ACK,
        message_id: Uuid::new_v4(),
        payload_type: 0,
        payload: Bytes::from(serde_json::to_vec(&content).context("serialize acknowledge")?),
    })
}

/// The JSON for the initial OpenDataChannel WebSocket text frame.
pub(crate) fn open_data_channel_json(token_value: &str) -> Result<String> {
    let open = OpenDataChannelInput {
        message_schema_version: "1.0".to_string(),
        request_id: Uuid::new_v4().to_string(),
        token_value: token_value.to_string(),
        client_id: Uuid::new_v4().to_string(),
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

/// Read a fixed 4-byte field at `offset`.
fn read_array<const N: usize>(buf: &[u8], offset: usize) -> Result<[u8; N]> {
    let end = offset.checked_add(N).context("field offset overflow")?;
    buf.get(offset..end)
        .context("field truncated")?
        .try_into()
        .context("field size mismatch")
}

/// Read the space/null-padded `MessageType` field as a trimmed string.
fn read_string(field: &[u8]) -> String {
    String::from_utf8_lossy(field)
        .trim_matches(|c| c == ' ' || c == '\0')
        .to_string()
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
        input_data(7, payload_type::OUTPUT, Bytes::copy_from_slice(payload))
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let msg = sample(b"hello ssh");
        let wire = msg.serialize().expect("serialize");
        let back = ClientMessage::deserialize(&wire).expect("deserialize");
        assert_eq!(back.message_type, message_type::INPUT_STREAM_DATA);
        assert_eq!(back.sequence_number, 7);
        assert_eq!(back.flags, 0);
        assert_eq!(back.payload_type, payload_type::OUTPUT);
        assert_eq!(back.message_id, msg.message_id);
        assert_eq!(back.payload.as_ref(), b"hello ssh");
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
            message_type: message_type::OUTPUT_STREAM_DATA.to_string(),
            sequence_number: 42,
            flags: 0,
            message_id: Uuid::nil(),
            payload_type: payload_type::OUTPUT,
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
}
