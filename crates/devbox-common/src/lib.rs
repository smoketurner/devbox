//! Shared types for the devbox orchestration service.

use jiff::Timestamp;
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
// InstanceId
// ============================================================================

/// A strongly-typed EC2 instance ID (e.g., "i-0123456789abcdef0").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(pub String);

impl InstanceId {
    /// Get the inner string value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the instance ID out of an EC2 instance ARN of the form
    /// `arn:aws:ec2:<region>:<account>:instance/<instance-id>`.
    ///
    /// Returns `None` unless the input is an `arn:…:ec2:…:instance/<id>` ARN with
    /// a non-empty id. Partition-agnostic (matches `:instance/` rather than a
    /// fixed prefix). Used to lift the STS-asserted `ec2_source_instance_arn`
    /// claim of an AWS web-identity token into a typed id.
    #[must_use]
    pub fn from_ec2_arn(arn: &str) -> Option<Self> {
        if !arn.starts_with("arn:") || !arn.contains(":ec2:") {
            return None;
        }
        let (_head, id) = arn.split_once(":instance/")?;
        let id = id.trim();
        (!id.is_empty()).then(|| Self(id.to_string()))
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for InstanceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for InstanceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ============================================================================
// Principal / username validation
// ============================================================================

/// Login names that must never be provisioned as a devbox owner: system accounts
/// and cloud-image defaults.
///
/// A devbox `owner` doubles as the Unix account the host provisions for the
/// claimant. Provisioning one of these names would *reuse* a pre-existing account
/// (and grant it passwordless sudo) instead of a dedicated per-claimant account —
/// breaking the one-account-per-claimant design and confusing audit trails. The
/// on-host `owner-sync` also refuses any pre-existing account with UID < 1000 as
/// defense-in-depth; this list additionally catches cloud defaults like `ubuntu`
/// and `ec2-user`, which are UID 1000.
const RESERVED_USERNAMES: &[&str] = &[
    "root",
    "admin",
    "ubuntu",
    "ec2-user",
    "ssm-user",
    "daemon",
    "bin",
    "sys",
    "sync",
    "nobody",
    "sshd",
    "docker",
    "systemd-network",
    "systemd-resolve",
];

/// Whether `name` is a reserved system or cloud-default login name (see
/// [`RESERVED_USERNAMES`]). Case-insensitive.
#[must_use]
pub fn is_reserved_username(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESERVED_USERNAMES.contains(&lower.as_str())
}

/// Whether `name` is a valid Linux login account name for a devbox owner.
///
/// Allows `^[a-z_][a-z0-9_.-]*$`, at most 32 characters — a superset of
/// `useradd`'s stock `NAME_REGEX` that also permits dots, so email-derived
/// `first.last` logins work — and rejects [reserved names](is_reserved_username).
/// A devbox `owner` doubles as the Unix login account the host provisions for the
/// claimant, so a principal that is not a valid username (e.g. a full email
/// address) or that collides with a system/cloud-default account can never be
/// safely logged into over SSH. Claims reject such an owner up front, and the
/// on-host `owner-sync` agent applies the same rule (passing `useradd --badname`
/// for dotted names, which fall outside useradd's stock regex).
#[must_use]
pub fn is_valid_unix_username(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 || is_reserved_username(name) {
        return false;
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    first_ok
        && name.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.'
        })
}

/// Derive a Unix login from an email address.
///
/// Takes the local part (before `@`), trims surrounding whitespace, and
/// lowercases it. Returns `Some(login)` when the result satisfies
/// [`is_valid_unix_username`], `None` otherwise.
///
/// An `@` sign is required — bare usernames without a domain are rejected.
/// This makes the `@`-requirement explicit: a no-`@` input indicates a
/// misconfigured OIDC claim mapping and should fail loudly rather than silently
/// succeed with an unexpected value.
///
/// Only surrounding whitespace is trimmed; internal characters are never
/// stripped and the result is never truncated — a non-conforming local part is
/// rejected, not mangled — so distinct local parts can never collide on the
/// same `owner` (which would let one user act on another's devboxes).
#[must_use]
pub fn username_from_email(email: &str) -> Option<String> {
    // `split_once` returns None when '@' is absent, enforcing the requirement
    // that the input is an email address, not a bare username.
    let (local, _domain) = email.trim().split_once('@')?;
    let local = local.trim().to_ascii_lowercase();
    is_valid_unix_username(&local).then_some(local)
}

/// Maximum length of a devbox name, in characters.
pub const DEVBOX_NAME_MAX_LEN: usize = 32;

/// Whether `name` is a valid devbox name.
///
/// A devbox name is a friendly handle (e.g. `calm-quilt`) shown in the UI and
/// CLI and usable as a selector for `ssh`/`release`/`status`. The rules:
/// non-empty, at most [`DEVBOX_NAME_MAX_LEN`] characters, every character one of
/// `a`–`z`, `0`–`9`, `_` or `-`, and not starting with `-` (so it can be passed
/// as a CLI positional without being mistaken for a flag). Auto-generated names
/// always satisfy these rules.
#[must_use]
pub fn is_valid_devbox_name(name: &str) -> bool {
    if name.is_empty() || name.len() > DEVBOX_NAME_MAX_LEN || name.starts_with('-') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

// ============================================================================
// Environment helpers
// ============================================================================

/// Trimmed value of environment variable `key`, or `None` when it is unset,
/// non-UTF-8, or blank after trimming.
///
/// Non-secret configuration is supplied through the environment across the
/// server, agent, and minter; this is the shared "present and non-blank" read.
#[must_use]
pub fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ============================================================================
// Display helpers
// ============================================================================

/// Display format for timestamps: "Jun 23, 2026, 23:33 UTC" (24-hour, UTC).
const TIMESTAMP_FORMAT: &str = "%b %-d, %Y, %H:%M UTC";

/// Format a timestamp for display, e.g. "Jun 23, 2026, 23:33 UTC".
///
/// `jiff` renders timestamps with nanosecond precision (e.g.
/// `2026-06-23T23:33:39.964772703Z`), which is noisy in dashboards and CLI
/// output; this trims to minute precision in UTC.
#[must_use]
pub fn format_timestamp(ts: Timestamp) -> String {
    ts.strftime(TIMESTAMP_FORMAT).to_string()
}

/// Format an RFC 3339 timestamp string for display (see [`format_timestamp`]).
///
/// For callers that hold the serialized string rather than a [`Timestamp`].
/// Returns the input unchanged if it is not a parseable timestamp (empty
/// strings, placeholders like `-`), so optional values pass straight through.
#[must_use]
pub fn format_timestamp_str(rfc3339: &str) -> String {
    rfc3339
        .parse::<Timestamp>()
        .map_or_else(|_| rfc3339.to_string(), format_timestamp)
}

// ============================================================================
// RFC 9728 — OAuth 2.0 Protected Resource Metadata
// ============================================================================

/// Metadata document served at `/.well-known/oauth-protected-resource` per
/// [RFC 9728](https://www.rfc-editor.org/rfc/rfc9728).
///
/// Clients (the `devbox` CLI) fetch this to discover the authorization server
/// and required scopes without prior out-of-band configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// The URL of this protected resource (advisory, best-effort from Host header).
    pub resource: String,
    /// Authorization server(s) that issue access tokens for this resource.
    pub authorization_servers: Vec<String>,
    /// Scopes supported by this resource (the `email` scope is required for
    /// `devbox`; `openid` is required to obtain an ID token).
    pub scopes_supported: Vec<String>,
    /// AWS account the control plane (and its devbox pool) runs in. A vendor
    /// extension to the RFC 9728 document (RFC 9728 §3.1 permits additional
    /// members): `devbox ssh` reads it to auto-select the local AWS profile for
    /// the SSM tunnel. Absent when the server has no `AWS_ACCOUNT_ID` configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_account_id: Option<String>,
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
        // `f.pad` (not `write!`) so format width/alignment like `{:<12}` is
        // honored — column-aligned tables (e.g. `devbox list`) depend on it.
        f.pad(s)
    }
}

// ============================================================================
// API Request Types
// ============================================================================

/// Request to claim a devbox.
///
/// The owner is never supplied by the client — the server binds it to the
/// authenticated principal (the Unix login derived from the token's `email`
/// claim). Only an optional name override travels in the body: every box is
/// auto-named at creation, and a claimant may rename it to something of their
/// own choosing (see [`is_valid_devbox_name`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// Optional name override. When omitted (or blank), the box keeps its
    /// auto-generated name. Must satisfy [`is_valid_devbox_name`].
    #[serde(default)]
    pub name: Option<String>,
}

/// Request to rename a claimed devbox.
///
/// The box must be in the `Claimed` state and the caller must be its owner.
/// The new name is required (unlike [`ClaimRequest`] where the name is
/// optional) and must satisfy [`is_valid_devbox_name`]. Uniqueness is enforced
/// atomically by the document store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRequest {
    /// New name for the box. Must satisfy [`is_valid_devbox_name`].
    pub name: String,
}

// ============================================================================
// Agent git-token API
// ============================================================================

/// A GitHub repository identified by `owner/repo`.
///
/// The unit the server scopes a minted installation token to. Distinct from a
/// git remote URL (which the server parses into this); see [`GitHubRepository::parse`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubRepository {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepository {
    /// Parse an `owner/repo` string (a trailing `.git` is stripped).
    ///
    /// Returns `None` unless there are exactly two non-empty segments split on a
    /// single `/`. This parses the canonical `owner/repo` form only — git remote
    /// URLs (`https://…`, scp-like) are parsed server-side by the minter.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (owner, repo) = s.trim().split_once('/')?;
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

impl std::fmt::Display for GitHubRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// Request body for `POST /api/v1/agent/git-token`.
///
/// The agent sends the git remote URL it needs a token for; the server parses
/// `owner/repo`, gates on the GitHub App's host, and mints. The agent cannot
/// decide locally whether a remote is mintable — the App's host config lives on
/// the server, not the box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitTokenRequest {
    /// A git remote URL (`https://…`, `ssh://…`, or scp-like `git@host:owner/repo`).
    pub remote: String,
}

/// Response body for `POST /api/v1/agent/git-token`.
///
/// `token` is `None` when `remote` is not a repository on the App's GitHub host —
/// the agent then fetches unauthenticated, matching the prior on-box behavior. A
/// repository that *is* on the host but isn't covered by the App installation is
/// an error (the GitHub installation lookup 404s), not a `None` token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitTokenResponse {
    /// The repository the token is scoped to, when one was minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<GitHubRepository>,
    /// A short-lived `contents:read`+`metadata:read` token scoped to `repository`,
    /// or `None` when the remote isn't a mintable GitHub repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

// ============================================================================
// API Response Types
// ============================================================================

/// Response representing a single devbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxResponse {
    pub id: String,
    pub instance_id: String,
    /// Friendly `adjective-noun` handle (e.g. `calm-quilt`), unique across
    /// non-terminated boxes. Assigned by the reconciler at creation; a claimant
    /// may override it. Used as a selector for `ssh`/`release`/`status`.
    pub name: String,
    pub state: DevboxState,
    pub instance_type: InstanceType,
    pub ami_id: AmiId,
    pub owner: Option<String>,
    /// AWS region the instance runs in. Every pool instance resides in the
    /// control plane's region; the CLI uses this to open the SSM tunnel without
    /// any client-side AWS region configuration.
    pub region: String,
    pub created_at: String,
    pub claimed_at: Option<String>,
}

/// Response representing a list of devboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxListResponse {
    pub devboxes: Vec<DevboxResponse>,
}

/// JSON error body returned by every API error response: `{ "error": "..." }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
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
    fn devbox_state_display_honors_width() {
        // `Display` must use `f.pad`, so width/alignment specs work in tables.
        assert_eq!(format!("{:<12}", DevboxState::Claimed), "claimed     ");
        assert_eq!(format!("{:>7}", DevboxState::Ready), "  ready");
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
            name: Some("calm-quilt".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ClaimRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("calm-quilt"));
    }

    #[test]
    fn test_claim_request_omits_name() {
        let parsed: ClaimRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn test_is_valid_devbox_name_accepts() {
        assert!(is_valid_devbox_name("calm-quilt"));
        assert!(is_valid_devbox_name("box1"));
        assert!(is_valid_devbox_name("a"));
        assert!(is_valid_devbox_name("web_app"));
        assert!(is_valid_devbox_name("9lives")); // leading digit is fine for names
        assert!(is_valid_devbox_name(&"x".repeat(DEVBOX_NAME_MAX_LEN)));
    }

    #[test]
    fn test_is_valid_devbox_name_rejects() {
        assert!(!is_valid_devbox_name(""));
        assert!(!is_valid_devbox_name("MyProj")); // uppercase
        assert!(!is_valid_devbox_name("-lead")); // leading hyphen
        assert!(!is_valid_devbox_name("has space"));
        assert!(!is_valid_devbox_name("dots.bad")); // dots not allowed
        assert!(!is_valid_devbox_name("a/../b"));
        assert!(!is_valid_devbox_name(&"x".repeat(DEVBOX_NAME_MAX_LEN + 1)));
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
    fn test_is_valid_unix_username_accepts() {
        assert!(is_valid_unix_username("jdoe"));
        assert!(is_valid_unix_username("agent-42"));
        assert!(is_valid_unix_username("_svc"));
        assert!(is_valid_unix_username("a"));
        assert!(is_valid_unix_username("first.last")); // dotted (first.last) logins
    }

    #[test]
    fn test_is_valid_unix_username_rejects() {
        assert!(!is_valid_unix_username(""));
        assert!(!is_valid_unix_username("jane@example.com"));
        assert!(!is_valid_unix_username("9lives"));
        assert!(!is_valid_unix_username("Justin"));
        assert!(!is_valid_unix_username(".hidden")); // leading dot
        assert!(!is_valid_unix_username("a/../b"));
        assert!(!is_valid_unix_username(&"x".repeat(33)));
    }

    #[test]
    fn test_is_valid_unix_username_rejects_reserved() {
        // System and cloud-default accounts must not be reused as owners.
        assert!(!is_valid_unix_username("root"));
        assert!(!is_valid_unix_username("ubuntu"));
        assert!(!is_valid_unix_username("ec2-user"));
        assert!(!is_valid_unix_username("ssm-user")); // SSM Session Manager default
        assert!(is_reserved_username("ROOT")); // case-insensitive
        // An email whose local part is a reserved name yields no owner.
        assert_eq!(username_from_email("root@example.com"), None);
    }

    #[test]
    fn test_pool_metrics_response_serde() {
        let resp = PoolMetricsResponse {
            warming: 2,
            ready: 3,
            claimed: 4,
            terminating: 5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PoolMetricsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.warming, 2);
        assert_eq!(parsed.terminating, 5);
    }

    #[test]
    fn username_from_email_takes_local_part() {
        assert_eq!(
            username_from_email("jdoe@example.com").as_deref(),
            Some("jdoe")
        );
    }

    #[test]
    fn username_from_email_lowercases_and_trims() {
        assert_eq!(
            username_from_email("  JDoe@example.com  ").as_deref(),
            Some("jdoe")
        );
    }

    #[test]
    fn username_from_email_allows_dots_without_collision() {
        // Dots are kept (first.last logins), never stripped — so distinct local
        // parts can't fold onto the same owner (a.b stays distinct from ab).
        assert_eq!(
            username_from_email("first.last@example.com").as_deref(),
            Some("first.last")
        );
        assert_eq!(username_from_email("a.b@corp.com").as_deref(), Some("a.b"));
        assert_eq!(username_from_email("ab@corp.com").as_deref(), Some("ab"));
    }

    #[test]
    fn username_from_email_rejects_underivable() {
        assert!(username_from_email("123@example.com").is_none()); // leading digit
        assert!(username_from_email("@example.com").is_none()); // empty local part
        assert!(username_from_email("a+b@example.com").is_none()); // '+' not allowed
        let long = format!("{}@example.com", "a".repeat(33));
        assert!(username_from_email(&long).is_none()); // >32 chars, never truncated
    }

    #[test]
    fn username_from_email_rejects_no_at_sign() {
        // A bare username is not an email; returning None avoids silent misuse
        // of a misconfigured OIDC claim mapping.
        assert!(username_from_email("jdoe").is_none());
        assert!(username_from_email("first.last").is_none());
    }

    #[test]
    fn format_timestamp_str_renders_human_readable() {
        assert_eq!(
            format_timestamp_str("2026-06-23T23:33:39.964772703Z"),
            "Jun 23, 2026, 23:33 UTC"
        );
    }

    #[test]
    fn format_timestamp_str_passes_through_non_timestamps() {
        // Placeholders and empty strings are returned unchanged.
        assert_eq!(format_timestamp_str("-"), "-");
        assert_eq!(format_timestamp_str(""), "");
        assert_eq!(format_timestamp_str("not-a-timestamp"), "not-a-timestamp");
    }

    #[test]
    fn protected_resource_metadata_serde_roundtrip() {
        let meta = ProtectedResourceMetadata {
            resource: "https://cp.example".to_string(),
            authorization_servers: vec!["https://us.vouch.sh".to_string()],
            scopes_supported: vec!["openid".to_string(), "email".to_string()],
            aws_account_id: Some("123456789012".to_string()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: ProtectedResourceMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, meta);
        assert_eq!(parsed.authorization_servers.len(), 1);
        assert_eq!(
            parsed.authorization_servers.first().map(String::as_str),
            Some("https://us.vouch.sh")
        );
        assert_eq!(parsed.scopes_supported, ["openid", "email"]);
        assert_eq!(parsed.aws_account_id.as_deref(), Some("123456789012"));
    }

    #[test]
    fn protected_resource_metadata_omits_absent_account_id() {
        // No AWS_ACCOUNT_ID configured: the field must not appear in the JSON,
        // so the standard RFC 9728 document is unchanged for such deployments.
        let meta = ProtectedResourceMetadata {
            resource: "https://cp.example".to_string(),
            authorization_servers: vec!["https://us.vouch.sh".to_string()],
            scopes_supported: vec!["openid".to_string(), "email".to_string()],
            aws_account_id: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("aws_account_id"), "got: {json}");
    }

    #[test]
    fn instance_id_serde_transparent() {
        let id = InstanceId("i-0123456789abcdef0".to_string());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"i-0123456789abcdef0\"");
        let parsed: InstanceId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn instance_id_from_ec2_arn_parses() {
        assert_eq!(
            InstanceId::from_ec2_arn("arn:aws:ec2:us-east-1:123456789012:instance/i-abc123def456"),
            Some(InstanceId("i-abc123def456".to_string()))
        );
        // Partition-agnostic.
        assert_eq!(
            InstanceId::from_ec2_arn("arn:aws-us-gov:ec2:us-gov-west-1:123456789012:instance/i-1"),
            Some(InstanceId("i-1".to_string()))
        );
    }

    #[test]
    fn instance_id_from_ec2_arn_rejects_non_instance_arns() {
        // Wrong service, wrong resource, empty id, and non-ARN inputs all fail.
        assert_eq!(
            InstanceId::from_ec2_arn("arn:aws:ec2:us-east-1:123456789012:volume/vol-abc"),
            None
        );
        assert_eq!(
            InstanceId::from_ec2_arn("arn:aws:iam::123456789012:role/PoolRole"),
            None
        );
        assert_eq!(
            InstanceId::from_ec2_arn("arn:aws:ec2:us-east-1:123456789012:instance/"),
            None
        );
        assert_eq!(InstanceId::from_ec2_arn("not-an-arn"), None);
    }

    #[test]
    fn github_repository_parse_accepts() {
        assert_eq!(
            GitHubRepository::parse("smoketurner/devbox"),
            Some(GitHubRepository {
                owner: "smoketurner".to_string(),
                repo: "devbox".to_string(),
            })
        );
        // Trailing .git is stripped; surrounding whitespace trimmed.
        assert_eq!(
            GitHubRepository::parse("  smoketurner/devbox.git "),
            Some(GitHubRepository {
                owner: "smoketurner".to_string(),
                repo: "devbox".to_string(),
            })
        );
    }

    #[test]
    fn github_repository_parse_rejects() {
        assert_eq!(GitHubRepository::parse("owner-only"), None);
        assert_eq!(GitHubRepository::parse("/repo"), None);
        assert_eq!(GitHubRepository::parse("owner/"), None);
        assert_eq!(GitHubRepository::parse("a/b/c"), None);
        assert_eq!(GitHubRepository::parse(""), None);
    }

    #[test]
    fn github_repository_display_roundtrips_parse() {
        let repo = GitHubRepository {
            owner: "smoketurner".to_string(),
            repo: "devbox".to_string(),
        };
        assert_eq!(repo.to_string(), "smoketurner/devbox");
        assert_eq!(GitHubRepository::parse(&repo.to_string()), Some(repo));
    }

    #[test]
    fn git_token_request_serde() {
        let req = GitTokenRequest {
            remote: "https://github.com/smoketurner/devbox.git".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: GitTokenRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.remote, req.remote);
    }

    #[test]
    fn git_token_response_omits_none_fields() {
        // A non-mintable remote returns an empty object, not nulls.
        let resp = GitTokenResponse {
            repository: None,
            token: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, "{}");
        let parsed: GitTokenResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.token.is_none() && parsed.repository.is_none());
    }

    #[test]
    fn git_token_response_roundtrips_minted() {
        let resp = GitTokenResponse {
            repository: Some(GitHubRepository {
                owner: "smoketurner".to_string(),
                repo: "devbox".to_string(),
            }),
            token: Some("ghs_exampletoken".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GitTokenResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.token.as_deref(), Some("ghs_exampletoken"));
        assert_eq!(
            parsed
                .repository
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("smoketurner/devbox")
        );
    }
}
