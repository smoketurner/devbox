//! Cached OAuth session and DCR client registration.
//!
//! Two files live under the XDG *config* directory (`$XDG_CONFIG_HOME/devbox`,
//! falling back to `$HOME/.config/devbox`) — distinct from the *state* dir used
//! by [`crate::state`] for the active-claim registry.
//!
//! - `client.json` — the DCR-registered `client_id`, keyed by issuer (not a
//!   credential; public clients carry no secret).
//! - `config.json` — the remembered `current_server` plus per-server sessions
//!   (the bearer token and derived identity, keyed by server hostname so
//!   several servers stay logged in at once). Written **0600** on Unix because
//!   the entries hold bearer tokens. The `current_server` is set on `login` and
//!   is the default when no `--server`/`$DEVBOX_SERVER` is given.
//!
//! # Windows note
//!
//! File-permission enforcement on Windows is ACL-based and out of scope. On a
//! normal Windows profile, `%APPDATA%` ACLs already restrict access to the
//! current user, so the token is not materially exposed. The `mode(0o600)` call
//! is gated behind `#[cfg(unix)]` and is a no-op on non-Unix targets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jsonwebtoken::dangerous::insecure_decode;
use serde::{Deserialize, Serialize};

use devbox_common::username_from_email;

// ============================================================================
// On-disk types
// ============================================================================

/// The DCR registration result, cached per issuer. Not a credential: public
/// clients have no `client_secret`. Safe to write at default permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Client {
    /// The authorization server that issued this registration.
    pub issuer: String,
    /// The `client_id` returned by the DCR endpoint.
    pub client_id: String,
}

/// A cached session: the raw bearer token and fields derived from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    /// The bearer token (a Vouch access token), sent as a `Bearer` on API calls.
    pub token: String,
    /// The Unix login name derived from the `email` claim local part.
    pub owner: String,
    /// The full email address from the `email` claim.
    pub email: String,
    /// Token expiry as Unix epoch seconds (`exp` claim).
    pub expires_at: i64,
}

/// The on-disk `config.json`: the remembered current server plus per-server
/// sessions. Holds bearer tokens, so it is written 0600 on Unix.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ConfigFile {
    /// Server used when no `--server`/`$DEVBOX_SERVER` is given (full URL). Set
    /// on `login`; cleared by `logout` when it points at the logged-out server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_server: Option<String>,
    /// Sessions keyed by server hostname (`host`, or `host:port` for an
    /// explicit port) so multiple servers stay logged in independently.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    servers: BTreeMap<String, Session>,
}

// ============================================================================
// Internal JWT claims (only the fields we read)
// ============================================================================

#[derive(Deserialize)]
struct TokenClaims {
    email: Option<String>,
    exp: Option<i64>,
}

// ============================================================================
// Directory helpers
// ============================================================================

/// Resolve `$XDG_CONFIG_HOME/devbox` (fallback: `$HOME/.config/devbox`).
fn config_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("devbox"));
    }

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("neither XDG_CONFIG_HOME nor HOME is set; cannot locate devbox config")?;
    Ok(PathBuf::from(home).join(".config").join("devbox"))
}

fn client_file(dir: &Path) -> PathBuf {
    dir.join("client.json")
}

fn config_file(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

/// The hostname key for `server`: `host`, or `host:port` when the URL names an
/// explicit port (so `https://x.devbox.farm` and `http://localhost:3000` get
/// distinct, stable keys regardless of path or trailing slash).
fn server_key(server: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(server).with_context(|| format!("invalid server URL: {server}"))?;
    let host = url
        .host_str()
        .with_context(|| format!("server URL has no host: {server}"))?;
    match url.port() {
        Some(port) => Ok(format!("{host}:{port}")),
        None => Ok(host.to_string()),
    }
}

// ============================================================================
// File I/O helpers
// ============================================================================

/// Write `bytes` to `path` with mode 0600 on Unix (atomic via temp+rename).
///
/// On non-Unix targets the file is created with whatever permissions the OS
/// assigns (see module-level Windows note).
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    // Write to a sibling temp file first (atomic rename avoids partial writes
    // and, on POSIX, preserves the 0600 mode from creation rather than relying
    // on a post-write chmod race window).
    let tmp = path.with_added_extension("tmp");
    write_secret_direct(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))
}

#[cfg(unix)]
fn write_secret_direct(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open {} for writing", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    // Re-tighten perms in case the file pre-existed with looser perms (mode()
    // on OpenOptions only applies on creation, not on an existing file).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn write_secret_direct(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

// ============================================================================
// Client (DCR registration) API
// ============================================================================

/// Load the cached DCR client for `issuer`. Returns `None` when the file is
/// absent or the stored issuer does not match (different deployment).
pub(crate) fn load_client(issuer: &str) -> Result<Option<Client>> {
    let dir = config_dir()?;
    load_client_from(&dir, issuer)
}

fn load_client_from(dir: &Path, issuer: &str) -> Result<Option<Client>> {
    let path = client_file(dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let client: Client = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if client.issuer == issuer {
                Ok(Some(client))
            } else {
                Ok(None)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Persist the DCR registration. Stored at default permissions (not a credential).
pub(crate) fn save_client(client: &Client) -> Result<()> {
    let dir = config_dir()?;
    save_client_to(&dir, client)
}

fn save_client_to(dir: &Path, client: &Client) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = client_file(dir);
    let bytes = serde_json::to_vec_pretty(client).context("failed to serialize client")?;
    std::fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Remove the cached DCR registration (called on `invalid_client` to force
/// re-registration). No-op when the file is absent.
pub(crate) fn forget_client() -> Result<()> {
    let dir = config_dir()?;
    forget_client_in(&dir)
}

fn forget_client_in(dir: &Path) -> Result<()> {
    let path = client_file(dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

// ============================================================================
// Session API
// ============================================================================

impl Session {
    /// Decode `token` (signature not verified — the server does that) and
    /// construct a `Session` from the `email` and `exp` claims.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is malformed, `email` is missing, the
    /// email local part cannot be turned into a valid Unix login, or `exp` is
    /// missing.
    pub(crate) fn from_token(token: String) -> Result<Self> {
        let data = insecure_decode::<TokenClaims>(&token)
            .context("failed to decode token; is it a valid JWT?")?;

        let email = data
            .claims
            .email
            .filter(|e| !e.is_empty())
            .context("token is missing an 'email' claim")?;

        let owner = username_from_email(&email).context(format!(
            "cannot derive a Unix login from email '{email}'; \
             check that the Vouch principal matches ^[a-z_][a-z0-9_.-]*$"
        ))?;

        let exp = data.claims.exp.context("token is missing an 'exp' claim")?;

        Ok(Self {
            token,
            owner,
            email,
            expires_at: exp,
        })
    }

    /// Whether this session has expired.
    pub(crate) fn is_expired(&self) -> bool {
        // Mirror state.rs:155-158 — use SystemTime/UNIX_EPOCH, no new dep.
        // A clock-before-epoch Err is treated as expired (fail-safe).
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            // clock < UNIX_EPOCH → treat as expired (i64::MAX ensures the
            // comparison self.expires_at <= now_secs is always true).
            .map_or(i64::MAX, |d| {
                // u64 seconds — clamp to i64::MAX on overflow (tokens expire
                // long before the year 292 billion, but be explicit).
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
            });
        self.expires_at <= now_secs
    }
}

/// Load `config.json`, returning a default (empty) config when it is absent.
///
/// A parse failure on a corrupt file surfaces as a contextual error rather than
/// silently resetting — the caller can suggest `devbox login`.
fn load_config(dir: &Path) -> Result<ConfigFile> {
    let path = config_file(dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "failed to parse {}; run `devbox login` to refresh your session",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Persist `config.json` 0600 on Unix (it holds bearer tokens).
fn save_config(dir: &Path, config: &ConfigFile) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(config).context("failed to serialize config")?;
    write_secret(&config_file(dir), &bytes)
}

/// The server to target when no `--server`/`$DEVBOX_SERVER` is given: the one
/// remembered from the last `devbox login`, or `None` if never logged in.
pub(crate) fn current_server() -> Result<Option<String>> {
    let dir = config_dir()?;
    Ok(load_config(&dir)?.current_server)
}

/// Return the valid session for `server`, or `None` when not logged in to it or
/// the session has expired.
pub(crate) fn current(server: &str) -> Result<Option<Session>> {
    let dir = config_dir()?;
    current_from(&dir, server)
}

fn current_from(dir: &Path, server: &str) -> Result<Option<Session>> {
    let key = server_key(server)?;
    let config = load_config(dir)?;
    let Some(session) = config.servers.get(&key) else {
        return Ok(None);
    };
    if session.is_expired() {
        return Ok(None);
    }
    Ok(Some(session.clone()))
}

/// Persist `session` for `server` and mark `server` as the current default.
/// Written 0600 on Unix (contains a bearer token).
pub(crate) fn save_session(server: &str, session: &Session) -> Result<()> {
    let dir = config_dir()?;
    save_session_to(&dir, server, session)
}

fn save_session_to(dir: &Path, server: &str, session: &Session) -> Result<()> {
    let key = server_key(server)?;
    let mut config = load_config(dir)?;
    config.current_server = Some(server.to_string());
    config.servers.insert(key, session.clone());
    save_config(dir, &config)
}

/// Forget the cached session for `server`. Clears `current_server` when it
/// points at `server`. Keeps the DCR client registration — it is not a
/// credential and can be reused across logins.
pub(crate) fn logout(server: &str) -> Result<()> {
    let dir = config_dir()?;
    logout_from(&dir, server)
}

fn logout_from(dir: &Path, server: &str) -> Result<()> {
    let key = server_key(server)?;
    let mut config = load_config(dir)?;
    config.servers.remove(&key);
    // Clear current_server when it points at the same host:port, comparing by
    // server_key rather than exact string. A current_server persisted with a
    // trailing slash (or any other URL-equivalent difference) would otherwise
    // survive logout — the session entry is keyed by server_key and is removed,
    // but an exact-string match would leave a stale default pointing at a server
    // the user just logged out of.
    let current_key = config
        .current_server
        .as_deref()
        .and_then(|s| server_key(s).ok());
    if current_key.as_deref() == Some(key.as_str()) {
        config.current_server = None;
    }
    save_config(dir, &config)
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
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    /// Unique temp dir without a `tempfile` dep (mirrors state.rs pattern).
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "devbox-session-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sign(claims: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"irrelevant"),
        )
        .unwrap()
    }

    #[test]
    fn from_token_derives_owner_and_expiry() {
        let exp: i64 = 9_999_999_999;
        let token = sign(json!({ "email": "jane@example.com", "exp": exp }));
        let session = Session::from_token(token).unwrap();
        assert_eq!(session.owner, "jane");
        assert_eq!(session.email, "jane@example.com");
        assert_eq!(session.expires_at, exp);
    }

    #[test]
    fn is_expired_reflects_exp_claim() {
        // A past `exp` is expired; a far-future one is not. This is the property
        // `login` checks before persisting a freshly minted session.
        let past = Session::from_token(sign(json!({ "email": "j@x.com", "exp": 1_i64 }))).unwrap();
        assert!(past.is_expired(), "a token with exp=1 must be expired");

        let future = Session::from_token(sign(
            json!({ "email": "j@x.com", "exp": 9_999_999_999_i64 }),
        ))
        .unwrap();
        assert!(!future.is_expired(), "a far-future exp must not be expired");
    }

    #[test]
    fn from_token_allows_dotted_login() {
        let token = sign(json!({ "email": "first.last@corp.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();
        assert_eq!(session.owner, "first.last");
    }

    #[test]
    fn from_token_errors_on_missing_email() {
        let token = sign(json!({ "sub": "uuid-only", "exp": 9_999_999_999_i64 }));
        assert!(Session::from_token(token).is_err());
    }

    #[test]
    fn from_token_errors_on_underivable_email() {
        // Leading digit → invalid Unix login
        let token = sign(json!({ "email": "123user@example.com", "exp": 9_999_999_999_i64 }));
        assert!(Session::from_token(token).is_err());
    }

    #[test]
    fn from_token_errors_on_missing_exp() {
        let token = sign(json!({ "email": "jane@example.com" }));
        assert!(Session::from_token(token).is_err());
    }

    #[test]
    fn expired_session_treated_as_logged_out() {
        let token = sign(json!({ "email": "jane@example.com", "exp": 1_i64 }));
        let session = Session::from_token(token).unwrap();
        assert!(session.is_expired());
    }

    #[test]
    fn valid_session_not_expired() {
        let token = sign(json!({ "email": "jane@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();
        assert!(!session.is_expired());
    }

    const SERVER: &str = "https://smoketurner.devbox.farm";

    #[test]
    fn session_roundtrip_via_temp_dir() {
        let dir = temp_dir("session-roundtrip");
        let token = sign(json!({ "email": "bob@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();

        save_session_to(&dir, SERVER, &session).unwrap();
        let loaded = current_from(&dir, SERVER).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.owner, "bob");
        assert_eq!(loaded.email, "bob@example.com");
    }

    #[test]
    fn save_session_records_current_server() {
        let dir = temp_dir("current-server");
        let token = sign(json!({ "email": "bob@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();

        save_session_to(&dir, SERVER, &session).unwrap();
        assert_eq!(
            load_config(&dir).unwrap().current_server.as_deref(),
            Some(SERVER),
            "login must remember the server so --server is not needed next time"
        );
    }

    #[test]
    fn sessions_are_scoped_per_server() {
        let dir = temp_dir("per-server");
        let prod = "https://smoketurner.devbox.farm";
        let local = "http://localhost:3000";

        let prod_session = Session::from_token(sign(
            json!({ "email": "prod-user@example.com", "exp": 9_999_999_999_i64 }),
        ))
        .unwrap();
        let local_session = Session::from_token(sign(
            json!({ "email": "local-user@example.com", "exp": 9_999_999_999_i64 }),
        ))
        .unwrap();

        save_session_to(&dir, prod, &prod_session).unwrap();
        save_session_to(&dir, local, &local_session).unwrap();

        // Each server keeps its own session; the later login is the current one.
        assert_eq!(
            current_from(&dir, prod).unwrap().unwrap().owner,
            "prod-user"
        );
        assert_eq!(
            current_from(&dir, local).unwrap().unwrap().owner,
            "local-user"
        );
        assert_eq!(
            load_config(&dir).unwrap().current_server.as_deref(),
            Some(local)
        );
    }

    #[test]
    fn current_returns_none_when_file_missing() {
        let dir = temp_dir("missing-session");
        assert!(current_from(&dir, SERVER).unwrap().is_none());
    }

    #[test]
    fn current_returns_none_for_unknown_server() {
        let dir = temp_dir("unknown-server");
        let session = Session::from_token(sign(
            json!({ "email": "bob@example.com", "exp": 9_999_999_999_i64 }),
        ))
        .unwrap();
        save_session_to(&dir, SERVER, &session).unwrap();
        // A different server has no session even though the file exists.
        assert!(
            current_from(&dir, "https://other.devbox.farm")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn current_returns_none_when_expired() {
        let dir = temp_dir("expired-session");
        let token = sign(json!({ "email": "alice@example.com", "exp": 1_i64 }));
        let session = Session::from_token(token).unwrap();
        save_session_to(&dir, SERVER, &session).unwrap();
        assert!(current_from(&dir, SERVER).unwrap().is_none());
    }

    #[test]
    fn corrupt_config_json_returns_err_not_panic() {
        let dir = temp_dir("corrupt-config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(config_file(&dir), b"{bad json}").unwrap();
        let result = current_from(&dir, SERVER);
        assert!(
            result.is_err(),
            "corrupt config.json must Err, not Ok or panic"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("devbox login"),
            "error must suggest re-login, got: {msg}"
        );
    }

    #[test]
    fn logout_removes_session_clears_current_keeps_client() {
        let dir = temp_dir("logout");
        let token = sign(json!({ "email": "carol@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();
        save_session_to(&dir, SERVER, &session).unwrap();

        let client = Client {
            issuer: "https://us.vouch.sh".to_string(),
            client_id: "cid-123".to_string(),
        };
        save_client_to(&dir, &client).unwrap();

        logout_from(&dir, SERVER).unwrap();

        // session gone and current_server cleared, client stays
        assert!(current_from(&dir, SERVER).unwrap().is_none());
        assert!(load_config(&dir).unwrap().current_server.is_none());
        assert!(
            load_client_from(&dir, "https://us.vouch.sh")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn logout_clears_current_server_saved_with_trailing_slash() {
        // current_server persisted with a trailing slash (e.g. by a pre-fix CLI)
        // must still be cleared when logging out of the normalized server, so the
        // CLI does not keep defaulting to a server the user logged out of.
        let dir = temp_dir("logout-trailing-slash");
        let token = sign(json!({ "email": "carol@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();
        save_session_to(&dir, "http://localhost:3000/", &session).unwrap();
        assert_eq!(
            load_config(&dir).unwrap().current_server.as_deref(),
            Some("http://localhost:3000/")
        );

        logout_from(&dir, "http://localhost:3000").unwrap();

        assert!(
            current_from(&dir, "http://localhost:3000")
                .unwrap()
                .is_none()
        );
        assert!(
            load_config(&dir).unwrap().current_server.is_none(),
            "current_server must be cleared despite the slash-only difference"
        );
    }

    #[test]
    fn logout_keeps_other_servers_signed_in() {
        let dir = temp_dir("logout-other");
        let prod = "https://smoketurner.devbox.farm";
        let local = "http://localhost:3000";
        let mk = |email: &str| {
            Session::from_token(sign(json!({ "email": email, "exp": 9_999_999_999_i64 }))).unwrap()
        };
        save_session_to(&dir, prod, &mk("prod@example.com")).unwrap();
        save_session_to(&dir, local, &mk("local@example.com")).unwrap();

        logout_from(&dir, local).unwrap();

        // Only the local session is gone; prod stays. current_server pointed at
        // local (the last save), so it is cleared.
        assert!(current_from(&dir, local).unwrap().is_none());
        assert_eq!(current_from(&dir, prod).unwrap().unwrap().owner, "prod");
        assert!(load_config(&dir).unwrap().current_server.is_none());
    }

    #[test]
    fn client_roundtrip_via_temp_dir() {
        let dir = temp_dir("client-roundtrip");
        let client = Client {
            issuer: "https://us.vouch.sh".to_string(),
            client_id: "my-client-id".to_string(),
        };
        save_client_to(&dir, &client).unwrap();

        let loaded = load_client_from(&dir, "https://us.vouch.sh")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.client_id, "my-client-id");
    }

    #[test]
    fn client_returns_none_on_issuer_mismatch() {
        let dir = temp_dir("client-issuer-mismatch");
        let client = Client {
            issuer: "https://us.vouch.sh".to_string(),
            client_id: "cid".to_string(),
        };
        save_client_to(&dir, &client).unwrap();

        assert!(
            load_client_from(&dir, "https://other.vouch.sh")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forget_client_is_idempotent() {
        let dir = temp_dir("forget-client");
        // No file yet — must not error.
        forget_client_in(&dir).unwrap();
        // File present — must remove without error.
        let client = Client {
            issuer: "https://us.vouch.sh".to_string(),
            client_id: "cid".to_string(),
        };
        save_client_to(&dir, &client).unwrap();
        forget_client_in(&dir).unwrap();
        assert!(
            load_client_from(&dir, "https://us.vouch.sh")
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_file_has_0600_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("config-perms");
        let token = sign(json!({ "email": "dave@example.com", "exp": 9_999_999_999_i64 }));
        let session = Session::from_token(token).unwrap();
        save_session_to(&dir, SERVER, &session).unwrap();

        let meta = std::fs::metadata(config_file(&dir)).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config.json must be 0600");
    }

    #[test]
    fn server_key_distinguishes_explicit_port() {
        assert_eq!(
            server_key("https://smoketurner.devbox.farm").unwrap(),
            "smoketurner.devbox.farm"
        );
        assert_eq!(
            server_key("https://smoketurner.devbox.farm/").unwrap(),
            "smoketurner.devbox.farm",
            "a trailing slash must not change the key"
        );
        assert_eq!(
            server_key("http://localhost:3000").unwrap(),
            "localhost:3000"
        );
    }

    #[test]
    fn server_key_rejects_garbage() {
        assert!(server_key("not a url").is_err());
    }

    // SAFETY: `std::env::set_var` is unsafe in edition 2024 because concurrent
    // env mutation is UB in multi-threaded processes. This test is marked
    // `#[ignore]` and must be run with `--test-threads=1` if invoked directly
    // (or via `cargo test -- --test-threads=1 config_dir_uses_xdg_config_home`).
    // It is excluded from the default parallel test run to prevent data races
    // with other tests that call `config_dir()`.
    #[test]
    #[ignore = "mutates process env; run with --test-threads=1"]
    fn config_dir_uses_xdg_config_home() {
        let dir = temp_dir("xdg-routing");
        let old = std::env::var_os("XDG_CONFIG_HOME");

        // SAFETY: This test modifies process-global env state. It is #[ignore]
        // and must be run single-threaded to avoid concurrent env mutation.
        #[expect(
            unsafe_code,
            reason = "env mutation in test; single-threaded guard via #[ignore]"
        )]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }

        let resolved = config_dir().unwrap();

        // Restore before asserting to ensure cleanup even on panic.
        #[expect(
            unsafe_code,
            reason = "env mutation in test; single-threaded guard via #[ignore]"
        )]
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert_eq!(
            resolved,
            dir.join("devbox"),
            "XDG_CONFIG_HOME must be respected"
        );
    }
}
