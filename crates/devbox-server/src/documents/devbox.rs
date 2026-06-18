//! Devbox document type.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::db::document_type::{DocumentType, IndexEntry};
use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};

/// A devbox instance document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxDoc {
    /// EC2 instance ID (set after launch).
    pub instance_id: Option<String>,
    /// Current state in the lifecycle.
    pub state: DevboxState,
    /// EC2 instance type (e.g., "m5.large").
    pub instance_type: InstanceType,
    /// AMI ID used to launch the instance.
    pub ami_id: AmiId,
    /// Subnet ID where the instance is launched.
    pub subnet_id: SubnetId,
    /// EBS volume ID (if attached).
    pub ebs_volume_id: Option<String>,
    /// Owner (user who claimed the devbox).
    pub owner: Option<String>,
    /// When the devbox was claimed.
    pub claimed_at: Option<Timestamp>,
    /// When the devbox record was created.
    pub created_at: Timestamp,
    /// Whether the EC2 "devbox:owner" tag has been applied after claiming.
    /// Tagging is deferred to the reconciler tick; this flag enables retry.
    #[serde(default)]
    pub owner_tag_applied: bool,
}

impl DocumentType for DevboxDoc {
    const DOC_TYPE: &'static str = "devbox";

    fn index_entries(&self) -> Vec<IndexEntry> {
        let mut entries = vec![IndexEntry {
            field: "state",
            value: self.state.to_string(),
        }];

        if let Some(ref owner) = self.owner {
            entries.push(IndexEntry {
                field: "owner",
                value: owner.clone(),
            });
        }

        if let Some(ref instance_id) = self.instance_id {
            entries.push(IndexEntry {
                field: "instance_id",
                value: instance_id.clone(),
            });
        }

        entries
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::get_unwrap,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn sample_devbox() -> DevboxDoc {
        DevboxDoc {
            instance_id: Some("i-1234567890abcdef0".to_string()),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            ebs_volume_id: Some("vol-12345678".to_string()),
            owner: None,
            claimed_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
        }
    }

    #[test]
    fn test_devbox_doc_serde_roundtrip() {
        let doc = sample_devbox();
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: DevboxDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.state, DevboxState::Ready);
        assert_eq!(parsed.instance_type, InstanceType("m5.large".to_string()));
    }

    #[test]
    fn test_devbox_doc_index_entries_no_owner() {
        let doc = sample_devbox();
        let entries = doc.index_entries();
        assert_eq!(entries.len(), 2); // state + instance_id
        assert_eq!(entries.first().unwrap().field, "state");
        assert_eq!(entries.first().unwrap().value, "ready");
        assert_eq!(entries.get(1).unwrap().field, "instance_id");
    }

    #[test]
    fn test_devbox_doc_index_entries_with_owner() {
        let mut doc = sample_devbox();
        doc.owner = Some("user@example.com".to_string());
        let entries = doc.index_entries();
        assert_eq!(entries.len(), 3); // state + owner + instance_id
    }

    #[test]
    fn test_devbox_doc_type() {
        assert_eq!(DevboxDoc::DOC_TYPE, "devbox");
    }
}
