//! Owner-sync service: provision the claimant's Unix login account.
//!
//! In a warm pool a box is generic until claimed; the `devbox:owner` tag (the
//! claimant's Vouch principal) is applied *after* the claim. sshd authorizes
//! that principal dynamically (see [`crate::principals`]), but the matching Unix
//! account must exist before the login can complete — sshd resolves the target
//! account before running `AuthorizedPrincipalsCommand`. This loop watches IMDS
//! and creates the account idempotently when an owner appears.
//!
//! Authorization stays with the `principals` command (always current,
//! fail-closed). This service only makes the account *exist*; an extra account
//! with no valid certificate cannot be logged into, so staleness is harmless.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_config::imds::client::Client;

use crate::imds;

/// How often to poll IMDS for the owner tag.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Shared workspace handed to the claimant on provisioning.
const WORKSPACE: &str = "/workspace";

/// Outcome of one provisioning pass.
enum Pass {
    /// The box is still unclaimed (no owner tag); keep polling.
    Waiting,
    /// The owner has appeared and been handled; the service can exit.
    Done,
}

/// Poll IMDS until the box is claimed, provision the claimant's account, then
/// return. A devbox is claimed once and terminated on release (cattle), so there
/// is nothing to do after provisioning — the systemd unit uses
/// `Restart=on-failure` so a clean exit stays stopped.
pub(crate) async fn run() {
    tracing::info!("owner-sync started; waiting for devbox:owner");
    let client = imds::client();
    loop {
        match tick(&client).await {
            Ok(Pass::Done) => {
                tracing::info!("owner-sync finished; exiting");
                return;
            }
            Ok(Pass::Waiting) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "owner-sync tick failed; will retry");
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One provisioning pass: read the owner tag and ensure its account exists.
async fn tick(client: &Client) -> Result<Pass> {
    let Some(owner) = imds::instance_tag(client, "devbox:owner").await? else {
        return Ok(Pass::Waiting); // unclaimed — nothing to provision yet
    };
    let owner = owner.trim();
    if owner.is_empty() {
        return Ok(Pass::Waiting);
    }
    if !is_valid_username(owner) {
        // The owner appeared but cannot be a Unix account; polling more won't
        // help (the owner does not change), so stop. SSH will fail, surfacing it.
        tracing::error!(
            owner,
            "refusing to provision unsafe principal as a Unix account"
        );
        return Ok(Pass::Done);
    }
    ensure_user(owner)?;
    Ok(Pass::Done)
}

/// Provision the login account for `user`, re-running each step idempotently.
///
/// Only `useradd` is gated on existence; the sudoers file is always rewritten so
/// a retry after a partial provisioning (account created but sudoers write
/// failed) self-heals and the claimant keeps passwordless sudo.
fn ensure_user(user: &str) -> Result<()> {
    if !user_exists(user) {
        run_cmd("useradd", &["-m", "-s", "/bin/bash", "-G", "docker", user])
            .with_context(|| format!("create login account for {user}"))?;
    }
    let sudoers = format!("/etc/sudoers.d/devbox-{user}");
    std::fs::write(&sudoers, format!("{user} ALL=(ALL) NOPASSWD: ALL\n"))
        .with_context(|| format!("write {sudoers}"))?;
    set_mode(&sudoers, 0o440)?;
    if let Err(e) = run_cmd("chown", &["-R", &format!("{user}:{user}"), WORKSPACE]) {
        tracing::warn!(user, error = %e, "failed to hand workspace to claimant");
    }
    tracing::info!(user, "provisioned claimant login account");
    Ok(())
}

/// Whether a Unix account named `user` already exists.
fn user_exists(user: &str) -> bool {
    Command::new("id")
        .arg("-u")
        .arg(user)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Validate that `name` is a safe Linux account name (default `useradd`
/// `NAME_REGEX`: `^[a-z_][a-z0-9_-]*$`, at most 32 characters). Vouch principals
/// that are not Unix-safe usernames are rejected rather than silently mangled.
fn is_valid_username(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    first_ok
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Run a command, returning an error if it cannot be spawned or exits non-zero.
fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawn {program}"))?;
    if !status.success() {
        bail!("{program} exited with status {:?}", status.code());
    }
    Ok(())
}

/// Set file permissions to `mode`.
fn set_mode(path: &str, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {path}"))
}

#[cfg(test)]
mod tests {
    use super::is_valid_username;

    #[test]
    fn accepts_simple_usernames() {
        assert!(is_valid_username("jplock"));
        assert!(is_valid_username("agent-42"));
        assert!(is_valid_username("_svc"));
        assert!(is_valid_username("a"));
    }

    #[test]
    fn rejects_unsafe_usernames() {
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("justin@plock.net"));
        assert!(!is_valid_username("9lives"));
        assert!(!is_valid_username("Justin"));
        assert!(!is_valid_username("a/../b"));
        assert!(!is_valid_username(&"x".repeat(33)));
    }
}
