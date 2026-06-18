//! Output formatting functions for the devbox CLI.

use devbox_common::{DevboxListResponse, DevboxResponse};

/// Format a list of devboxes as a column-aligned table.
pub(crate) fn format_list_table(list: &DevboxListResponse) -> String {
    let header = format!(
        "{:<8}  {:<12}  {:<12}  {:<19}  {}",
        "ID", "STATE", "TYPE", "INSTANCE", "OWNER"
    );
    let separator = "-".repeat(header.len());
    let mut lines = vec![header, separator];

    for d in &list.devboxes {
        let id_short = truncate(&d.id, 8);
        let instance_short = truncate(d.instance_id.as_deref().unwrap_or("-"), 19);
        let owner = d.owner.as_deref().unwrap_or("-");
        lines.push(format!(
            "{:<8}  {:<12}  {:<12}  {:<19}  {}",
            id_short,
            d.state,
            d.instance_type.as_ref(),
            instance_short,
            owner,
        ));
    }
    lines.join("\n")
}

/// Format a single devbox in labeled key-value style.
pub(crate) fn format_status(d: &DevboxResponse) -> String {
    let state_str = d.state.to_string();
    let instance_type_str: &str = d.instance_type.as_ref();
    let ami_id_str: &str = d.ami_id.as_ref();
    let pairs: &[(&str, &str)] = &[
        ("ID", d.id.as_str()),
        ("State", &state_str),
        ("Type", instance_type_str),
        ("AMI", ami_id_str),
        ("Instance", d.instance_id.as_deref().unwrap_or("-")),
        ("Owner", d.owner.as_deref().unwrap_or("-")),
        ("Created", &d.created_at),
        ("Claimed At", d.claimed_at.as_deref().unwrap_or("-")),
    ];
    pairs
        .iter()
        .map(|(k, v)| format!("  {:<12} {}", format!("{}:", k), v))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format a successful claim response with SSM connection hint.
pub(crate) fn format_claim_success(d: &DevboxResponse) -> String {
    let instance = d.instance_id.as_deref().unwrap_or("(pending)");
    format!(
        "Claimed devbox {}\n  Instance: {}\n  Type: {}\n  Connect: aws ssm start-session --target {}",
        &d.id,
        instance,
        d.instance_type.as_ref(),
        instance
    )
}

/// Format a successful release confirmation.
pub(crate) fn format_release_success(d: &DevboxResponse) -> String {
    format!("Released devbox {} (now {})", &d.id, d.state)
}

/// Truncate a string to `max_len` characters, appending "\u{2026}" if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let end = max_len.saturating_sub(1);
        let truncated: String = s.chars().take(end).collect();
        format!("{}\u{2026}", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devbox_common::{AmiId, DevboxState, InstanceType};

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 8), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate("12345678", 8), "12345678");
    }

    #[test]
    fn test_truncate_long_string() {
        let result = truncate("123456789", 8);
        assert_eq!(result, "1234567\u{2026}");
    }

    #[test]
    fn test_format_list_table_empty() {
        let list = DevboxListResponse { devboxes: vec![] };
        let output = format_list_table(&list);
        assert!(output.contains("ID"));
        assert!(output.contains("STATE"));
        assert!(output.contains("TYPE"));
        assert!(output.contains("INSTANCE"));
        assert!(output.contains("OWNER"));
    }

    #[test]
    fn test_format_list_table_with_entries() {
        let list = DevboxListResponse {
            devboxes: vec![DevboxResponse {
                id: "abcd1234".to_string(),
                instance_id: Some("i-0123456789abcdef0".to_string()),
                state: DevboxState::Ready,
                instance_type: InstanceType("m5.large".to_string()),
                ami_id: AmiId("ami-12345678".to_string()),
                owner: Some("alice".to_string()),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                claimed_at: None,
            }],
        };
        let output = format_list_table(&list);
        assert!(output.contains("abcd1234"));
        assert!(output.contains("ready"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("alice"));
    }

    #[test]
    fn test_format_status() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: Some("i-abc".to_string()),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("bob".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
        };
        let output = format_status(&d);
        assert!(output.contains("test-id"));
        assert!(output.contains("claimed"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("ami-123"));
        assert!(output.contains("i-abc"));
        assert!(output.contains("bob"));
    }

    #[test]
    fn test_format_claim_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: Some("i-abc123".to_string()),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("alice".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
        };
        let output = format_claim_success(&d);
        assert!(output.contains("Claimed devbox test-id"));
        assert!(output.contains("i-abc123"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("aws ssm start-session --target i-abc123"));
    }

    #[test]
    fn test_format_release_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: None,
            state: DevboxState::Terminating,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: None,
        };
        let output = format_release_success(&d);
        assert!(output.contains("Released devbox test-id"));
        assert!(output.contains("terminating"));
    }
}
