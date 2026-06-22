//! Owner-sync: provision the claimant's Unix login account.
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
use devbox_common::is_valid_unix_username;

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

/// What a provisioning pass should do, decided purely from the owner tag value.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// The box is unclaimed (tag absent or empty); keep polling.
    Wait,
    /// The owner is a valid login name; provision the account.
    Provision(String),
    /// The owner appeared but is not a valid Unix account name; give up.
    Unsafe(String),
}

/// Decide the action for the current `devbox:owner` tag value. Pure (no I/O) so
/// the branch logic can be unit-tested without IMDS.
fn decide(owner: Option<&str>) -> Decision {
    let Some(owner) = owner else {
        return Decision::Wait;
    };
    let owner = owner.trim();
    if owner.is_empty() {
        return Decision::Wait;
    }
    if !is_valid_unix_username(owner) {
        return Decision::Unsafe(owner.to_string());
    }
    Decision::Provision(owner.to_string())
}

/// One provisioning pass: read the owner tag and act on it.
async fn tick(client: &Client) -> Result<Pass> {
    let owner = imds::instance_tag(client, "devbox:owner").await?;
    match decide(owner.as_deref()) {
        Decision::Wait => Ok(Pass::Waiting), // unclaimed — nothing to provision yet
        Decision::Unsafe(owner) => {
            // The owner appeared but cannot be a Unix account; polling more
            // won't help (the owner does not change), so stop. SSH will fail,
            // surfacing it.
            tracing::error!(
                owner,
                "refusing to provision unsafe principal as a Unix account"
            );
            Ok(Pass::Done)
        }
        Decision::Provision(owner) => {
            ensure_user(&owner)?;
            Ok(Pass::Done)
        }
    }
}

/// Provision the login account for `user`, re-running each step idempotently.
///
/// Only `useradd` is gated on existence; the sudoers file is always rewritten so
/// a retry after a partial provisioning (account created but sudoers write
/// failed) self-heals and the claimant keeps passwordless sudo.
fn ensure_user(user: &str) -> Result<()> {
    if !user_exists(user) {
        // useradd's stock NAME_REGEX rejects dots; pass --badname for the
        // email-derived `first.last` logins we allow (omitted for plain names so
        // their behavior is unchanged).
        let mut args: Vec<&str> = Vec::new();
        if user.contains('.') {
            args.push("--badname");
        }
        args.extend(["-m", "-s", "/bin/bash", "-G", "docker", user]);
        run_cmd("useradd", &args).with_context(|| format!("create login account for {user}"))?;
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
    use super::{Decision, decide};

    #[test]
    fn absent_or_empty_owner_waits() {
        assert_eq!(decide(None), Decision::Wait);
        assert_eq!(decide(Some("")), Decision::Wait);
        assert_eq!(decide(Some("   ")), Decision::Wait);
    }

    #[test]
    fn valid_owner_provisions_trimmed() {
        assert_eq!(
            decide(Some("  jdoe  ")),
            Decision::Provision("jdoe".to_string())
        );
        assert_eq!(
            decide(Some("agent-42")),
            Decision::Provision("agent-42".to_string())
        );
    }

    #[test]
    fn unsafe_owner_is_refused() {
        assert_eq!(
            decide(Some("jane@example.com")),
            Decision::Unsafe("jane@example.com".to_string())
        );
        assert_eq!(
            decide(Some("Justin")),
            Decision::Unsafe("Justin".to_string())
        );
    }
}
