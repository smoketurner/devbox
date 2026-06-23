//! Output formatting functions for the devbox CLI.

use devbox_common::{DevboxListResponse, DevboxResponse, format_timestamp_str};

/// Format a list of devboxes as a column-aligned table.
pub(crate) fn format_list_table(list: &DevboxListResponse) -> String {
    let header = format!(
        "{:<20}  {:<12}  {:<12}  {}",
        "INSTANCE ID", "STATE", "TYPE", "OWNER"
    );
    let separator = "-".repeat(header.len());
    let mut lines = vec![header, separator];

    for d in &list.devboxes {
        let owner = d.owner.as_deref().unwrap_or("-");
        lines.push(format!(
            "{:<20}  {:<12}  {:<12}  {}",
            d.instance_id.as_str(),
            d.state,
            d.instance_type.as_ref(),
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
    let created = format_timestamp_str(&d.created_at);
    let claimed = d
        .claimed_at
        .as_deref()
        .map_or_else(|| "-".to_string(), format_timestamp_str);
    let pairs: &[(&str, &str)] = &[
        ("Instance ID", d.instance_id.as_str()),
        ("State", &state_str),
        ("Type", instance_type_str),
        ("Region", &d.region),
        ("AMI", ami_id_str),
        ("Owner", d.owner.as_deref().unwrap_or("-")),
        ("Created", &created),
        ("Claimed At", &claimed),
    ];
    pairs
        .iter()
        .map(|(k, v)| format!("  {:<12} {}", format!("{}:", k), v))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format a successful claim response with a connection hint.
pub(crate) fn format_claim_success(d: &DevboxResponse) -> String {
    format!(
        "Claimed devbox {}\n  Type: {}\n  Region: {}\n  Connect: devbox ssh \
         (saved as an active claim; --id is needed only when you hold several)",
        d.instance_id.as_str(),
        d.instance_type.as_ref(),
        d.region,
    )
}

/// Format a successful release confirmation.
pub(crate) fn format_release_success(d: &DevboxResponse) -> String {
    format!(
        "Released devbox {} (now {})",
        d.instance_id.as_str(),
        d.state
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use devbox_common::{AmiId, DevboxState, InstanceType};

    #[test]
    fn test_format_list_table_empty() {
        let list = DevboxListResponse { devboxes: vec![] };
        let output = format_list_table(&list);
        assert!(output.contains("INSTANCE ID"));
        assert!(output.contains("STATE"));
        assert!(output.contains("TYPE"));
        assert!(output.contains("OWNER"));
    }

    #[test]
    fn test_format_list_table_with_entries() {
        let list = DevboxListResponse {
            devboxes: vec![DevboxResponse {
                id: "abcd1234".to_string(),
                instance_id: "i-0123456789abcdef0".to_string(),
                state: DevboxState::Ready,
                instance_type: InstanceType("m5.large".to_string()),
                ami_id: AmiId("ami-12345678".to_string()),
                owner: Some("alice".to_string()),
                region: "us-east-1".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                claimed_at: None,
            }],
        };
        let output = format_list_table(&list);
        // The instance ID is shown; the internal UUID never is.
        assert!(output.contains("i-0123456789abcdef0"));
        assert!(!output.contains("abcd1234"));
        assert!(output.contains("ready"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("alice"));
    }

    #[test]
    fn test_format_status() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: "i-abc".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("bob".to_string()),
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
        };
        let output = format_status(&d);
        // The instance ID is shown; the internal UUID is not.
        assert!(output.contains("i-abc"));
        assert!(!output.contains("test-id"));
        assert!(output.contains("claimed"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("us-east-1"));
        assert!(output.contains("ami-123"));
        assert!(output.contains("bob"));
        // The noisy RFC 3339 timestamp is rendered human-readable.
        assert!(output.contains("Jan 1, 2024, 00:00 UTC"));
    }

    #[test]
    fn test_format_claim_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: "i-abc123".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("alice".to_string()),
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
        };
        let output = format_claim_success(&d);
        assert!(output.contains("Claimed devbox i-abc123"));
        assert!(!output.contains("test-id"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("us-east-1"));
        assert!(output.contains("devbox ssh"));
    }

    #[test]
    fn test_format_release_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: "i-rel123".to_string(),
            state: DevboxState::Terminating,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: None,
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: None,
        };
        let output = format_release_success(&d);
        assert!(output.contains("Released devbox i-rel123"));
        assert!(!output.contains("test-id"));
        assert!(output.contains("terminating"));
    }
}
