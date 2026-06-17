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
// DevboxState
// ============================================================================

/// State machine for devbox instances.
///
/// Lifecycle: Launching -> Warming -> Ready -> Claimed -> Terminating
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevboxState {
    /// Instance is being launched (EC2 RunInstances called).
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
    pub instance_type: Option<String>,
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
    pub instance_type: String,
    pub ami_id: String,
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
            instance_type: Some("m5.large".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ClaimRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.owner, "user@example.com");
        assert_eq!(parsed.instance_type, Some("m5.large".to_string()));
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
}
