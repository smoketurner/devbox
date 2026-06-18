//! Shared types for the devbox orchestration service.

use serde::{Deserialize, Serialize};

// ============================================================================
// DevboxId
// ============================================================================

/// A unique identifier for a devbox instance, wrapping a UUIDv7 string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DevboxId(pub String);

impl DevboxId {
    /// Generate a new DevboxId using UUIDv7.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    /// Get the inner string value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for DevboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DevboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// InstanceType
// ============================================================================

/// A strongly-typed EC2 instance type (e.g., "m5.large").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceType(pub String);

impl std::fmt::Display for InstanceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for InstanceType {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for InstanceType {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// AmiId
// ============================================================================

/// A strongly-typed AMI ID (e.g., "ami-0123456789abcdef0").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AmiId(pub String);

impl std::fmt::Display for AmiId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for AmiId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for AmiId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// SubnetId
// ============================================================================

/// A strongly-typed subnet ID (e.g., "subnet-0123456789abcdef0").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubnetId(pub String);

impl std::fmt::Display for SubnetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SubnetId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for SubnetId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// SecurityGroupId
// ============================================================================

/// A strongly-typed security group ID (e.g., "sg-0123456789abcdef0").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecurityGroupId(pub String);

impl std::fmt::Display for SecurityGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SecurityGroupId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for SecurityGroupId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// DevboxState
// ============================================================================

/// State machine for devbox instances.
///
/// Lifecycle: Launching -> Warming -> Ready -> Claimed -> Terminating
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevboxState {
    /// Instance is being launched by the Auto Scaling Group.
    Launching,
    /// Instance is running but not yet ready (warming up).
    Warming,
    /// Instance is ready to be claimed by a user.
    Ready,
    /// Instance has been claimed by a user.
    Claimed,
    /// Instance is being terminated.
    Terminating,
}

impl std::fmt::Display for DevboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Launching => "launching",
            Self::Warming => "warming",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Terminating => "terminating",
        };
        write!(f, "{s}")
    }
}

// ============================================================================
// API Request Types
// ============================================================================

/// Request to claim a devbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// The user/owner requesting a devbox.
    pub owner: String,
    /// Optional preferred instance type.
    pub instance_type: Option<InstanceType>,
}

/// Request to release a claimed devbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// The user/owner releasing the devbox.
    pub owner: String,
}

// ============================================================================
// API Response Types
// ============================================================================

/// Response representing a single devbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxResponse {
    pub id: String,
    pub instance_id: Option<String>,
    pub state: DevboxState,
    pub instance_type: InstanceType,
    pub ami_id: AmiId,
    pub owner: Option<String>,
    pub created_at: String,
    pub claimed_at: Option<String>,
}

/// Response representing a list of devboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxListResponse {
    pub devboxes: Vec<DevboxResponse>,
}

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub database: String,
}

/// Pool metrics response showing instance counts by state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolMetricsResponse {
    pub warming: u32,
    pub ready: u32,
    pub claimed: u32,
    pub terminating: u32,
    pub target_warm_pool_size: u32,
    /// Positive = deficit (need more Ready), negative = surplus.
    pub ready_delta: i32,
}

// ============================================================================
// Config Structs
// ============================================================================

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Port to listen on.
    pub port: u16,
    /// Database URL.
    pub database_url: String,
}

/// Database configuration.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Database URL (sqlite: or postgres:).
    pub url: String,
    /// Maximum pool connections.
    pub max_connections: u32,
    /// Minimum idle connections.
    pub min_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "sqlite::memory:".to_string(),
            max_connections: 25,
            min_connections: 2,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    #[test]
    fn test_devbox_id_new() {
        let id = DevboxId::new();
        assert!(!id.0.is_empty());
    }

    #[test]
    fn test_devbox_id_display() {
        let id = DevboxId("test-id-123".to_string());
        assert_eq!(id.to_string(), "test-id-123");
    }

    #[test]
    fn test_devbox_state_serde_roundtrip() {
        let states = vec![
            DevboxState::Launching,
            DevboxState::Warming,
            DevboxState::Ready,
            DevboxState::Claimed,
            DevboxState::Terminating,
        ];

        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: DevboxState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, parsed);
        }
    }

    #[test]
    fn test_devbox_state_display() {
        assert_eq!(DevboxState::Launching.to_string(), "launching");
        assert_eq!(DevboxState::Warming.to_string(), "warming");
        assert_eq!(DevboxState::Ready.to_string(), "ready");
        assert_eq!(DevboxState::Claimed.to_string(), "claimed");
        assert_eq!(DevboxState::Terminating.to_string(), "terminating");
    }

    #[test]
    fn test_devbox_id_serde_roundtrip() {
        let id = DevboxId("abc-123".to_string());
        let json = serde_json::to_string(&id).unwrap();
        let parsed: DevboxId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_claim_request_serde() {
        let req = ClaimRequest {
            owner: "user@example.com".to_string(),
            instance_type: Some(InstanceType("m5.large".to_string())),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ClaimRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.owner, "user@example.com");
        assert_eq!(
            parsed.instance_type,
            Some(InstanceType("m5.large".to_string()))
        );
    }

    #[test]
    fn test_health_response_serde() {
        let resp = HealthResponse {
            status: "ok".to_string(),
            database: "healthy".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "ok");
    }

    #[test]
    fn test_instance_type_serde_transparent() {
        let it = InstanceType("m5.large".to_string());
        let json = serde_json::to_string(&it).unwrap();
        assert_eq!(json, "\"m5.large\"");
        let parsed: InstanceType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, it);
    }

    #[test]
    fn test_ami_id_serde_transparent() {
        let ami = AmiId("ami-12345678".to_string());
        let json = serde_json::to_string(&ami).unwrap();
        assert_eq!(json, "\"ami-12345678\"");
        let parsed: AmiId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ami);
    }

    #[test]
    fn test_subnet_id_serde_transparent() {
        let subnet = SubnetId("subnet-abcdef".to_string());
        let json = serde_json::to_string(&subnet).unwrap();
        assert_eq!(json, "\"subnet-abcdef\"");
        let parsed: SubnetId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, subnet);
    }

    #[test]
    fn test_security_group_id_serde_transparent() {
        let sg = SecurityGroupId("sg-abcdef0123456789".to_string());
        let json = serde_json::to_string(&sg).unwrap();
        assert_eq!(json, "\"sg-abcdef0123456789\"");
        let parsed: SecurityGroupId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sg);
    }

    #[test]
    fn test_pool_metrics_response_serde() {
        let resp = PoolMetricsResponse {
            warming: 2,
            ready: 3,
            claimed: 4,
            terminating: 5,
            target_warm_pool_size: 3,
            ready_delta: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PoolMetricsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.warming, 2);
        assert_eq!(parsed.ready_delta, 0);
    }
}
