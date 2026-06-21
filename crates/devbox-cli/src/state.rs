//! Local registry of the user's active devbox claims.
//!
//! `claim` records the new devbox here and `release` removes it, so that
//! `ssh`/`status`/`release` can resolve the target devbox without an explicit
//! `--id` when the user holds exactly one (or pick interactively when they hold
//! several). The file lives under the XDG *state* directory — it is
//! machine-managed runtime state, not user-edited config.
//!
//! Owner is intentionally never stored: it is always re-derived from the
//! authenticated session (see [`crate::token`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single active claim the user holds, keyed by `(id, server_url)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Claim {
    /// Devbox id.
    pub id: String,
    /// Server the claim was made against (entries are scoped per server).
    pub server_url: String,
    /// When the claim happened, for display only. May be absent.
    #[serde(default)]
    pub claimed_at: Option<String>,
}

/// The on-disk registry: a flat list of active claims.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Claims {
    #[serde(default)]
    pub claims: Vec<Claim>,
}

/// Resolve the devbox state directory: `$XDG_STATE_HOME/devbox`, falling back to
/// `$HOME/.local/state/devbox` when `XDG_STATE_HOME` is unset or empty.
fn state_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("devbox"));
    }

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("neither XDG_STATE_HOME nor HOME is set; cannot locate devbox state")?;
    Ok(PathBuf::from(home).join(".local/state").join("devbox"))
}

/// The claims file inside `dir`. Pure helper so callers/tests can supply a dir.
fn claims_file(dir: &Path) -> PathBuf {
    dir.join("claims.json")
}

/// Load the registry from `dir`, treating a missing file as empty.
fn load_from(dir: &Path) -> Result<Claims> {
    let path = claims_file(dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Claims::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Persist the registry to `dir`, creating the directory if needed.
fn save_to(dir: &Path, claims: &Claims) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = claims_file(dir);
    let bytes = serde_json::to_vec_pretty(claims).context("failed to serialize claims")?;
    std::fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Add (or replace) a claim, keyed by `(id, server_url)`. Idempotent re-claims.
pub(crate) fn add(claim: Claim) -> Result<()> {
    let dir = state_dir()?;
    add_in(&dir, claim)
}

fn add_in(dir: &Path, claim: Claim) -> Result<()> {
    let mut claims = load_from(dir)?;
    claims
        .claims
        .retain(|c| !(c.id == claim.id && c.server_url == claim.server_url));
    claims.claims.push(claim);
    save_to(dir, &claims)
}

/// Remove the claim matching `(id, server_url)`. No-op if absent.
pub(crate) fn remove(id: &str, server_url: &str) -> Result<()> {
    let dir = state_dir()?;
    remove_in(&dir, id, server_url)
}

fn remove_in(dir: &Path, id: &str, server_url: &str) -> Result<()> {
    let mut claims = load_from(dir)?;
    let before = claims.claims.len();
    claims
        .claims
        .retain(|c| !(c.id == id && c.server_url == server_url));
    if claims.claims.len() == before {
        return Ok(());
    }
    save_to(dir, &claims)
}

/// Prune local entries for `server_url` whose id the server no longer reports as
/// Claimed. Entries for other servers are left untouched. Prune-only — never
/// adds discovered ids.
pub(crate) fn reconcile(server_url: &str, live_claimed_ids: &BTreeSet<String>) -> Result<()> {
    let dir = state_dir()?;
    reconcile_in(&dir, server_url, live_claimed_ids)
}

fn reconcile_in(dir: &Path, server_url: &str, live_claimed_ids: &BTreeSet<String>) -> Result<()> {
    let mut claims = load_from(dir)?;
    let before = claims.claims.len();
    claims
        .claims
        .retain(|c| c.server_url != server_url || live_claimed_ids.contains(&c.id));
    if claims.claims.len() == before {
        return Ok(());
    }
    save_to(dir, &claims)
}

/// The user's active claims for `server_url` (no prompting — selection happens in
/// the caller).
pub(crate) fn active_claims(server_url: &str) -> Result<Vec<Claim>> {
    let dir = state_dir()?;
    active_claims_in(&dir, server_url)
}

fn active_claims_in(dir: &Path, server_url: &str) -> Result<Vec<Claim>> {
    let claims = load_from(dir)?;
    Ok(claims
        .claims
        .into_iter()
        .filter(|c| c.server_url == server_url)
        .collect())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    /// A unique temp dir for an isolated registry, without a `tempfile` dep.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "devbox-cli-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn claim(id: &str, server: &str) -> Claim {
        Claim {
            id: id.to_string(),
            server_url: server.to_string(),
            claimed_at: None,
        }
    }

    #[test]
    fn claims_file_is_under_dir() {
        let dir = Path::new("/tmp/devbox-x");
        assert_eq!(claims_file(dir), Path::new("/tmp/devbox-x/claims.json"));
    }

    #[test]
    fn load_missing_is_empty() {
        let dir = temp_dir("load-missing");
        assert!(load_from(&dir).unwrap().claims.is_empty());
    }

    #[test]
    fn add_then_load_roundtrip_and_idempotent() {
        let dir = temp_dir("add-roundtrip");
        add_in(&dir, claim("box1", "http://s1")).unwrap();
        add_in(&dir, claim("box2", "http://s1")).unwrap();
        // Re-adding the same (id, server_url) replaces rather than duplicates.
        add_in(&dir, claim("box1", "http://s1")).unwrap();

        let loaded = load_from(&dir).unwrap();
        assert_eq!(loaded.claims.len(), 2);
        let ids: BTreeSet<_> = loaded.claims.iter().map(|c| c.id.clone()).collect();
        assert!(ids.contains("box1"));
        assert!(ids.contains("box2"));
    }

    #[test]
    fn remove_only_matching_entry() {
        let dir = temp_dir("remove");
        add_in(&dir, claim("box1", "http://s1")).unwrap();
        add_in(&dir, claim("box1", "http://s2")).unwrap();

        remove_in(&dir, "box1", "http://s1").unwrap();
        let loaded = load_from(&dir).unwrap();
        assert_eq!(loaded.claims.len(), 1);
        assert_eq!(loaded.claims.first().unwrap().server_url, "http://s2");

        // Removing an absent entry is a no-op (no error).
        remove_in(&dir, "nope", "http://s1").unwrap();
    }

    #[test]
    fn reconcile_prunes_only_stale_for_server() {
        let dir = temp_dir("reconcile");
        add_in(&dir, claim("live", "http://s1")).unwrap();
        add_in(&dir, claim("stale", "http://s1")).unwrap();
        add_in(&dir, claim("other", "http://s2")).unwrap();

        let live: BTreeSet<String> = ["live".to_string()].into_iter().collect();
        reconcile_in(&dir, "http://s1", &live).unwrap();

        let loaded = load_from(&dir).unwrap();
        let ids: BTreeSet<_> = loaded.claims.iter().map(|c| c.id.clone()).collect();
        assert!(ids.contains("live"), "present claim kept");
        assert!(!ids.contains("stale"), "absent claim pruned");
        assert!(ids.contains("other"), "other server untouched");
    }

    #[test]
    fn active_claims_filters_by_server() {
        let dir = temp_dir("active");
        add_in(&dir, claim("box1", "http://s1")).unwrap();
        add_in(&dir, claim("box2", "http://s2")).unwrap();

        let s1 = active_claims_in(&dir, "http://s1").unwrap();
        assert_eq!(s1.len(), 1);
        assert_eq!(s1.first().unwrap().id, "box1");
    }
}
