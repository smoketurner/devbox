//! Leader lock document type for distributed reconciler coordination.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::db::document_type::{DocumentType, IndexEntry};

/// Document type for the distributed leader lock.
///
/// A singleton document (well-known ID: "reconciler-leader-lock") that
/// implements distributed mutual exclusion via TTL-based expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderLockDoc {
    /// Identity of the server holding the lock.
    pub holder_id: String,
    /// When the lock expires (UTC timestamp).
    pub expires_at: Timestamp,
}

impl DocumentType for LeaderLockDoc {
    const DOC_TYPE: &'static str = "leader_lock";

    fn index_entries(&self) -> Vec<IndexEntry> {
        vec![IndexEntry {
            field: "holder_id",
            value: self.holder_id.clone(),
        }]
    }

    fn expires_at(&self) -> Option<Timestamp> {
        Some(self.expires_at)
    }
}
