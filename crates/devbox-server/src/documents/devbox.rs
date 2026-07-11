//! Devbox document type.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::db::document_type::{DocumentType, IndexEntry};
use devbox_common::{AmiId, DevboxState, InstanceType, RepoFreshenReport, SubnetId};

/// Bound on stored per-repo entries; a report exceeding it is truncated.
const MAX_REPORT_REPOS: usize = 64;
/// Bound on a stored per-repo error string, in characters.
const MAX_REPORT_ERROR_CHARS: usize = 256;

/// A devbox instance document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxDoc {
    /// EC2 instance ID. The reconciler is adopt-only — a doc is created only from
    /// an instance that already exists in the ASG — so this is always present.
    pub instance_id: String,
    /// Friendly `adjective-noun` handle (e.g. `calm-quilt`), unique across
    /// non-terminated boxes and usable as a selector. The reconciler assigns it
    /// when it creates the box; a claimant may override it (`claim --name`).
    /// Empty for documents written before this field existed (not backfilled).
    #[serde(default)]
    pub name: String,
    /// Current state in the lifecycle.
    pub state: DevboxState,
    /// EC2 instance type (e.g., "m5.large").
    pub instance_type: InstanceType,
    /// AMI ID used to launch the instance.
    pub ami_id: AmiId,
    /// Subnet ID where the instance is launched.
    pub subnet_id: SubnetId,
    /// AWS region the instance runs in (read from instance metadata, like
    /// `subnet_id`). Surfaced in the API so the CLI can open the SSM tunnel
    /// without client-side region configuration. Defaults empty for documents
    /// written before this field existed; the reconciler backfills it.
    #[serde(default)]
    pub region: String,
    /// EBS volume ID (if attached).
    pub ebs_volume_id: Option<String>,
    /// Owner (user who claimed the devbox).
    pub owner: Option<String>,
    /// Full email of the claimant, surfaced to the host as the `devbox:owner-email`
    /// tag so `owner-sync` can configure the claimant's git identity. Defaults empty
    /// for documents written before this field existed (not backfilled).
    #[serde(default)]
    pub owner_email: Option<String>,
    /// When the devbox was claimed.
    pub claimed_at: Option<Timestamp>,
    /// When the devbox record was created.
    pub created_at: Timestamp,
    /// Whether the EC2 "devbox:owner" tag has been applied after claiming. The
    /// claim handler applies it inline (so the box is loginable without waiting
    /// for a reconciler tick) and sets this true; if that inline call fails it
    /// stays false and the reconciler re-applies it on its next tick.
    #[serde(default)]
    pub owner_tag_applied: bool,
    /// Warm-up metrics reported by the agent after tagging ready. Absent for
    /// boxes running an older agent, or when the best-effort report never
    /// arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_report: Option<WarmupReport>,
}

/// Warm-up metrics as stored on the [`DevboxDoc`] — the wire request plus the
/// server receive time. A doc-owned copy (rather than the wire type raw) so
/// storage can evolve independently of the agent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmupReport {
    /// `systemctl start docker` wall time, in milliseconds.
    pub docker_start_ms: u64,
    /// Total wall time of the freshen phase, in milliseconds.
    pub freshen_total_ms: u64,
    /// Warm-up wall time from agent start to the report, in milliseconds.
    pub total_ms: u64,
    /// Whether `/workspace` held at least one repo.
    pub workspace_present: bool,
    /// Per-repo freshen outcomes, bounded to [`MAX_REPORT_REPOS`] entries.
    #[serde(default)]
    pub repos: Vec<RepoFreshenReport>,
    /// When the server received the report (server clock — agent clocks are
    /// not trusted for cross-box comparison).
    pub reported_at: Timestamp,
}

impl WarmupReport {
    /// Build the stored report from a wire request, stamping `now` as the
    /// receive time and bounding stored size: at most [`MAX_REPORT_REPOS`]
    /// repo entries, each error truncated to [`MAX_REPORT_ERROR_CHARS`]
    /// characters. The caller is an authenticated pool host, but the document
    /// row shouldn't grow unbounded on a misbehaving one.
    pub(crate) fn from_request(req: &devbox_common::WarmupReportRequest, now: Timestamp) -> Self {
        let repos = req
            .repos
            .iter()
            .take(MAX_REPORT_REPOS)
            .map(|repo| RepoFreshenReport {
                repo: truncate_chars(&repo.repo, MAX_REPORT_ERROR_CHARS),
                success: repo.success,
                duration_ms: repo.duration_ms,
                error: repo
                    .error
                    .as_deref()
                    .map(|e| truncate_chars(e, MAX_REPORT_ERROR_CHARS)),
            })
            .collect();
        Self {
            docker_start_ms: req.docker_start_ms,
            freshen_total_ms: req.freshen_total_ms,
            total_ms: req.total_ms,
            workspace_present: req.workspace_present,
            repos,
            reported_at: now,
        }
    }
}

/// The first `max` characters of `s` (char-boundary-safe; indexing by bytes
/// would panic mid-codepoint and is denied by the lint set anyway).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl DevboxDoc {
    /// The instance tag set dictated by this doc's ownership: `devbox:owner`
    /// always (when an owner is set), plus `devbox:owner-email` when present (for
    /// the claimant's git identity on the host). Empty when the box is unowned, so
    /// callers can skip tagging. Shared by the claim handler (inline, at claim
    /// time) and the reconciler (idempotent fallback) so both tag identically.
    pub(crate) fn owner_tags(&self) -> Vec<(&str, &str)> {
        let mut tags = Vec::new();
        if let Some(owner) = self.owner.as_deref() {
            tags.push(("devbox:owner", owner));
            if let Some(email) = self.owner_email.as_deref() {
                tags.push(("devbox:owner-email", email));
            }
        }
        tags
    }
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

        if !self.name.is_empty() {
            entries.push(IndexEntry {
                field: "name",
                value: self.name.clone(),
            });
        }

        entries.push(IndexEntry {
            field: "instance_id",
            value: self.instance_id.clone(),
        });

        entries
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn sample_devbox() -> DevboxDoc {
        DevboxDoc {
            instance_id: "i-1234567890abcdef0".to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: Some("vol-12345678".to_string()),
            owner: None,
            owner_email: None,
            claimed_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
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
        assert_eq!(entries.len(), 3); // state + name + instance_id
        assert_eq!(entries.first().unwrap().field, "state");
        assert_eq!(entries.first().unwrap().value, "ready");
        assert!(
            entries
                .iter()
                .any(|e| e.field == "name" && e.value == "calm-quilt")
        );
        assert!(entries.iter().any(|e| e.field == "instance_id"));
    }

    #[test]
    fn test_devbox_doc_index_entries_with_owner() {
        let mut doc = sample_devbox();
        doc.owner = Some("user@example.com".to_string());
        let entries = doc.index_entries();
        assert_eq!(entries.len(), 4); // state + owner + name + instance_id
    }

    #[test]
    fn test_devbox_doc_index_entries_empty_name() {
        let mut doc = sample_devbox();
        doc.name = String::new();
        let entries = doc.index_entries();
        assert_eq!(entries.len(), 2); // state + instance_id (no name)
        assert!(!entries.iter().any(|e| e.field == "name"));
    }

    #[test]
    fn test_devbox_doc_type() {
        assert_eq!(DevboxDoc::DOC_TYPE, "devbox");
    }

    #[test]
    fn warmup_report_roundtrips_through_serde() {
        let mut doc = sample_devbox();
        doc.warmup_report = Some(WarmupReport {
            docker_start_ms: 850,
            freshen_total_ms: 12_000,
            total_ms: 13_500,
            workspace_present: true,
            repos: vec![RepoFreshenReport {
                repo: "devbox".to_string(),
                success: true,
                duration_ms: 11_000,
                error: None,
            }],
            reported_at: Timestamp::now(),
        });

        let json = serde_json::to_string(&doc).unwrap();
        let parsed: DevboxDoc = serde_json::from_str(&json).unwrap();
        let report = parsed.warmup_report.unwrap();
        assert_eq!(report.total_ms, 13_500);
        assert_eq!(report.repos.first().unwrap().repo, "devbox");
    }

    #[test]
    fn doc_without_warmup_report_field_deserializes_to_none() {
        // Documents written before the field existed must keep deserializing
        // (forward compat — no migration, no version bump).
        let mut json = serde_json::to_value(sample_devbox()).unwrap();
        let obj = json.as_object_mut().unwrap();
        assert!(
            !obj.contains_key("warmup_report"),
            "None must be skipped in serialization"
        );
        let parsed: DevboxDoc = serde_json::from_value(json).unwrap();
        assert!(parsed.warmup_report.is_none());
    }

    #[test]
    fn from_request_bounds_repos_and_error_length() {
        let req = devbox_common::WarmupReportRequest {
            docker_start_ms: 1,
            freshen_total_ms: 2,
            total_ms: 3,
            workspace_present: true,
            repos: (0..(MAX_REPORT_REPOS + 10))
                .map(|i| RepoFreshenReport {
                    repo: format!("repo-{i}"),
                    success: false,
                    duration_ms: 1,
                    error: Some("é".repeat(MAX_REPORT_ERROR_CHARS + 50)),
                })
                .collect(),
        };

        let stored = WarmupReport::from_request(&req, Timestamp::now());

        assert_eq!(stored.repos.len(), MAX_REPORT_REPOS);
        let error = stored.repos.first().unwrap().error.as_deref().unwrap();
        // Multi-byte chars: the bound is characters, not bytes, and truncation
        // must never split a codepoint.
        assert_eq!(error.chars().count(), MAX_REPORT_ERROR_CHARS);
    }
}
