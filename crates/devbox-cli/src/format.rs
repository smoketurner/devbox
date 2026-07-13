//! Output formatting functions for the devbox CLI.

use devbox_common::{DevboxListResponse, DevboxResponse, SessionResponse, format_timestamp_str};

/// Format a list of devboxes as a column-aligned table.
pub(crate) fn format_list_table(list: &DevboxListResponse) -> String {
    let header = format!(
        "{:<18}  {:<20}  {:<12}  {:<12}  {}",
        "NAME", "INSTANCE ID", "STATE", "TYPE", "OWNER"
    );
    let separator = "-".repeat(header.len());
    let mut lines = vec![header, separator];

    for d in &list.devboxes {
        let owner = d.owner.as_deref().unwrap_or("-");
        lines.push(format!(
            "{:<18}  {:<20}  {:<12}  {:<12}  {}",
            d.name.as_str(),
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
        ("Name", &d.name),
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
        "Claimed devbox {name}\n  Instance: {instance}\n  Type: {ty}\n  Region: {region}\n  \
         Connect: devbox ssh {name}",
        name = d.name,
        instance = d.instance_id.as_str(),
        ty = d.instance_type.as_ref(),
        region = d.region,
    )
}

/// Format a successful release confirmation.
pub(crate) fn format_release_success(d: &DevboxResponse) -> String {
    format!("Released devbox {} (now {})", d.name, d.state)
}

/// Format a successful rename confirmation.
pub(crate) fn format_rename_success(d: &DevboxResponse) -> String {
    format!("Renamed devbox to {}", d.name)
}

/// Format the caller's archived sessions as a column-aligned table.
pub(crate) fn format_sessions_table(sessions: &[SessionResponse]) -> String {
    if sessions.is_empty() {
        return "No archived sessions. Create one with `devbox release --keep`.".to_string();
    }
    let header = format!(
        "{:<18}  {:<10}  {:<10}  {:<20}  {}",
        "NAME", "STATE", "SIZE", "CREATED", "EXPIRES"
    );
    let separator = "-".repeat(header.len());
    let mut lines = vec![header, separator];

    for s in sessions {
        let size = s
            .size_bytes
            .map_or_else(|| "-".to_string(), format_size_bytes);
        let created = format_timestamp_str(&s.created_at);
        let expires = s
            .expires_at
            .as_deref()
            .map_or_else(|| "-".to_string(), format_timestamp_str);
        lines.push(format!(
            "{:<18}  {:<10}  {:<10}  {:<20}  {}",
            s.name, s.state, size, created, expires,
        ));
    }
    lines.push(String::new());
    lines.push("Resume one with `devbox claim --resume <name>`.".to_string());
    lines.join("\n")
}

/// A byte count as a compact human-readable size.
fn format_size_bytes(bytes: u64) -> String {
    const UNITS: &[(u64, &str)] = &[(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    for (scale, unit) in UNITS {
        if bytes >= *scale {
            let whole = bytes.checked_div(*scale).unwrap_or(0);
            let tenths = bytes
                .checked_rem(*scale)
                .unwrap_or(0)
                .saturating_mul(10)
                .checked_div(*scale)
                .unwrap_or(0);
            return format!("{whole}.{tenths} {unit}");
        }
    }
    format!("{bytes} B")
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
                name: "calm-quilt".to_string(),
                state: DevboxState::Ready,
                instance_type: InstanceType("m5.large".to_string()),
                ami_id: AmiId("ami-12345678".to_string()),
                owner: Some("alice".to_string()),
                region: "us-east-1".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                claimed_at: None,
                session: None,
            }],
        };
        let output = format_list_table(&list);
        // The name and instance ID are shown; the internal UUID never is.
        assert!(output.contains("calm-quilt"));
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
            name: "calm-quilt".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("bob".to_string()),
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
            session: None,
        };
        let output = format_status(&d);
        // The name and instance ID are shown; the internal UUID is not.
        assert!(output.contains("calm-quilt"));
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
            name: "calm-quilt".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("alice".to_string()),
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
            session: None,
        };
        let output = format_claim_success(&d);
        assert!(output.contains("Claimed devbox calm-quilt"));
        assert!(output.contains("i-abc123"));
        assert!(!output.contains("test-id"));
        assert!(output.contains("m5.large"));
        assert!(output.contains("us-east-1"));
        // The connect hint selects the box by name.
        assert!(output.contains("devbox ssh calm-quilt"));
    }

    #[test]
    fn test_format_release_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: "i-rel123".to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Terminating,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: None,
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: None,
            session: None,
        };
        let output = format_release_success(&d);
        assert!(output.contains("Released devbox calm-quilt"));
        assert!(!output.contains("test-id"));
        assert!(output.contains("terminating"));
    }

    #[test]
    fn test_format_rename_success() {
        let d = DevboxResponse {
            id: "test-id".to_string(),
            instance_id: "i-ren123".to_string(),
            name: "my-feature".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: Some("alice".to_string()),
            region: "us-east-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
            session: None,
        };
        let output = format_rename_success(&d);
        assert!(output.contains("Renamed devbox to my-feature"));
        assert!(!output.contains("test-id"));
    }

    #[test]
    fn sessions_table_lists_and_hints_resume() {
        use devbox_common::SessionState;
        let sessions = vec![SessionResponse {
            id: "0197-abc".to_string(),
            name: "calm-quilt".to_string(),
            state: SessionState::Complete,
            source_devbox: "i-1234567890abcdef0".to_string(),
            created_at: "2026-07-12T00:00:00Z".to_string(),
            expires_at: Some("2026-08-11T00:00:00Z".to_string()),
            size_bytes: Some(2 * 1024 * 1024),
        }];
        let output = format_sessions_table(&sessions);
        assert!(output.contains("calm-quilt"));
        assert!(output.contains("complete"));
        assert!(output.contains("2.0 MiB"));
        assert!(output.contains("--resume"));
    }

    #[test]
    fn sessions_table_empty_suggests_keep() {
        assert!(format_sessions_table(&[]).contains("release --keep"));
    }

    #[test]
    fn size_formatting_covers_units() {
        assert_eq!(format_size_bytes(512), "512 B");
        assert_eq!(format_size_bytes(2048), "2.0 KiB");
        assert_eq!(format_size_bytes(1024 * 1024 + 512 * 1024), "1.5 MiB");
        assert_eq!(format_size_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
