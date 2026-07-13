//! Session archive document type.
//!
//! A `SessionDoc` records one archived devbox session: the S3 object a released
//! box uploaded (git work-in-progress + agent context) and the metadata needed
//! to restore it onto a fresh box with `claim --resume`. Created pending by
//! `release --keep`; marked complete/failed by the agent's archive-done report
//! (or failed by the reconciler when the archive deadline passes). Expired
//! sessions are swept by the store's TTL cleanup — the S3 object itself is
//! expired independently by the bucket's lifecycle rule.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::db::document_type::{DocumentType, IndexEntry};
use devbox_common::{SessionResponse, SessionState};

/// Bound on a stored failure summary, in characters.
const MAX_SESSION_ERROR_CHARS: usize = 256;

/// An archived (or archiving) devbox session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDoc {
    /// The name the source box had when it was released — the friendly
    /// selector for `claim --resume`. Not unique: releasing a box named
    /// `calm-quilt` twice yields two sessions; resolution picks the newest
    /// complete one.
    pub name: String,
    /// The principal who owned the released box. Sessions are only listable
    /// and resumable by their owner.
    pub owner: String,
    pub state: SessionState,
    /// Instance the archive was produced on.
    pub source_instance_id: String,
    /// S3 object key of the archive (`sessions/<doc-id>.tar.gz`).
    pub s3_key: String,
    /// Uploaded archive size, once reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Timestamp>,
    /// When the session ages out (created_at + the server's session TTL).
    pub expires_at: Timestamp,
    /// Truncated failure summary when `state` is `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SessionDoc {
    /// Bound an agent-reported failure summary for storage.
    pub(crate) fn truncate_error(error: &str) -> String {
        error.chars().take(MAX_SESSION_ERROR_CHARS).collect()
    }
}

impl DocumentType for SessionDoc {
    const DOC_TYPE: &'static str = "session";

    fn index_entries(&self) -> Vec<IndexEntry> {
        vec![
            IndexEntry {
                field: "state",
                value: self.state.to_string(),
            },
            IndexEntry {
                field: "owner",
                value: self.owner.clone(),
            },
            IndexEntry {
                field: "name",
                value: self.name.clone(),
            },
        ]
    }

    fn expires_at(&self) -> Option<Timestamp> {
        Some(self.expires_at)
    }
}

impl From<crate::db::document_type::Document<SessionDoc>> for SessionResponse {
    fn from(doc: crate::db::document_type::Document<SessionDoc>) -> Self {
        SessionResponse {
            id: doc.id,
            name: doc.data.name,
            state: doc.data.state,
            source_devbox: doc.data.source_instance_id,
            created_at: doc.data.created_at.to_string(),
            expires_at: Some(doc.data.expires_at.to_string()),
            size_bytes: doc.data.size_bytes,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn sample_session() -> SessionDoc {
        SessionDoc {
            name: "calm-quilt".to_string(),
            owner: "jdoe".to_string(),
            state: SessionState::Pending,
            source_instance_id: "i-1234567890abcdef0".to_string(),
            s3_key: "sessions/0197-abc.tar.gz".to_string(),
            size_bytes: None,
            created_at: Timestamp::now(),
            completed_at: None,
            expires_at: Timestamp::now(),
            error: None,
        }
    }

    #[test]
    fn session_doc_serde_roundtrip() {
        let doc = sample_session();
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: SessionDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.state, SessionState::Pending);
        assert_eq!(parsed.name, "calm-quilt");
        assert_eq!(parsed.s3_key, "sessions/0197-abc.tar.gz");
    }

    #[test]
    fn session_doc_indexes_state_owner_and_name() {
        let entries = sample_session().index_entries();
        assert!(
            entries
                .iter()
                .any(|e| e.field == "state" && e.value == "pending")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.field == "owner" && e.value == "jdoe")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.field == "name" && e.value == "calm-quilt")
        );
    }

    #[test]
    fn session_doc_expires() {
        let doc = sample_session();
        assert_eq!(doc.expires_at(), Some(doc.expires_at));
    }

    #[test]
    fn truncate_error_bounds_length() {
        let long = "é".repeat(500);
        assert_eq!(SessionDoc::truncate_error(&long).chars().count(), 256);
    }
}
