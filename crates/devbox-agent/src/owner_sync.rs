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
//!
//! On provisioning it also writes the claimant's `~/.gitconfig`: their git identity
//! (`user.email`/`user.name`, from the `devbox:owner-email` tag) so commits are
//! attributed with no manual setup, and — when `DEVBOX_SERVER_URL` is set — the
//! reverse-proxy remotes (`insteadOf` + a credential helper) so GitHub traffic is
//! authenticated with a server-minted token the box never holds.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_config::imds::client::Client;
use devbox_common::{env_non_empty, is_valid_unix_username};
use ini::{EscapePolicy, Ini, WriteOption};

use crate::imds;

/// The control-plane base URL; the claimant's git is pointed at its reverse proxy.
const SERVER_URL_ENV: &str = "DEVBOX_SERVER_URL";

/// Absolute path to this binary on the golden AMI (matches the systemd units), used
/// in the git credential helper so it resolves regardless of the claimant's `PATH`.
const AGENT_BIN: &str = "/usr/local/sbin/devbox-agent";

/// How often to poll IMDS for the owner tag. Kept short so the claimant's account
/// is provisioned within a couple of seconds of the `devbox:owner` tag becoming
/// visible (the tag is now applied inline at claim time, not on the next
/// reconciler tick), trimming the first-SSH wait. The cost on an unclaimed box is
/// only a cheap IMDS read every few seconds.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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
            // The email tag must be read before we finish. A transient IMDS error
            // propagates so the poll loop retries instead of permanently skipping
            // the git identity; an absent tag (`Ok(None)`) is final and just leaves
            // it unset.
            let email = imds::instance_tag(client, "devbox:owner-email")
                .await
                .context("read devbox:owner-email")?;
            configure_git(&owner, email.as_deref());
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
    match existing_uid(user) {
        // Refuse to hand passwordless sudo to a pre-existing system account.
        // `is_valid_unix_username` already blocks the known cloud defaults
        // (ubuntu/ec2-user, UID 1000); this catches any other system account
        // (UID < 1000) so a misconfigured principal fails loudly here rather
        // than silently reusing a shared account.
        Some(uid) if uid < 1000 => {
            bail!(
                "refusing to reuse pre-existing system account '{user}' (uid {uid}); \
                 a devbox owner must map to a dedicated account"
            );
        }
        // A dedicated account from a prior pass — fall through to re-assert
        // sudoers idempotently.
        Some(_) => {}
        None => {
            // useradd's stock NAME_REGEX rejects dots; pass --badname for the
            // email-derived `first.last` logins we allow (omitted for plain names
            // so their behavior is unchanged).
            let mut args: Vec<&str> = Vec::new();
            if user.contains('.') {
                args.push("--badname");
            }
            args.extend(["-m", "-s", "/bin/bash", "-G", "docker", user]);
            run_cmd("useradd", &args)
                .with_context(|| format!("create login account for {user}"))?;
        }
    }
    let sudoers = format!("/etc/sudoers.d/devbox-{user}");
    std::fs::write(&sudoers, format!("{user} ALL=(ALL) NOPASSWD: ALL\n"))
        .with_context(|| format!("write {sudoers}"))?;
    set_mode(&sudoers, 0o440)?;
    if let Err(e) = run_cmd("chown", &["-R", &format!("{user}:{user}"), WORKSPACE]) {
        tracing::warn!(user, error = %e, "failed to hand workspace to claimant");
    }
    // The snapshot-seeded repos were fetched by warm-up as root; a recursive chown
    // over a large tree can be slow or partial. Mark all workspace repos as safe so
    // the claimant's first `git` command does not abort with "dubious ownership".
    if let Err(e) = run_cmd("git", &["config", "--system", "safe.directory", "*"]) {
        tracing::warn!(user, error = %e, "failed to mark workspace repos as safe git directories");
    }
    tracing::info!(user, "provisioned claimant login account");
    Ok(())
}

/// A single git-config entry: section, optional subsection, value name, and value.
struct GitEntry {
    section: &'static str,
    subsection: Option<String>,
    name: &'static str,
    value: String,
}

/// Write the claimant's `~/.gitconfig` — git identity (from the `devbox:owner-email`
/// tag) and the reverse-proxy remotes (from `DEVBOX_SERVER_URL`) — then hand the file
/// to the claimant. Best-effort: an absent home or write failure is logged, not fatal
/// (the account is already usable; git just talks to GitHub directly).
fn configure_git(user: &str, email: Option<&str>) {
    let entries = git_config_entries(user, email, env_non_empty(SERVER_URL_ENV).as_deref());
    if entries.is_empty() {
        tracing::info!(
            user,
            "no git identity or server URL; leaving .gitconfig unset"
        );
        return;
    }
    let Some(home) = user_home(user) else {
        tracing::warn!(
            user,
            "could not resolve home directory; skipping .gitconfig"
        );
        return;
    };
    let gitconfig = format!("{home}/.gitconfig");
    let rendered = match render_gitconfig(&entries) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(user, error = %format!("{e:#}"), "failed to render .gitconfig");
            return;
        }
    };
    if let Err(e) = std::fs::write(&gitconfig, rendered) {
        tracing::warn!(user, error = %e, "failed to write .gitconfig");
        return;
    }
    if let Err(e) = run_cmd("chown", &[&format!("{user}:{user}"), &gitconfig]) {
        tracing::warn!(user, error = %e, "failed to hand .gitconfig to claimant");
    }
    tracing::info!(user, "configured claimant .gitconfig");
}

/// Build the claimant's git-config entries: identity when an email is known
/// (`user.name` is the login), and the reverse-proxy remotes when `server_base` is
/// set — an `insteadOf` rewriting `https://github.com/` to the proxy and a credential
/// helper that supplies the web-identity token. Either group may be empty.
fn git_config_entries(user: &str, email: Option<&str>, server_base: Option<&str>) -> Vec<GitEntry> {
    let mut entries = Vec::new();
    if let Some(email) = email.map(str::trim).filter(|e| !e.is_empty()) {
        entries.push(GitEntry {
            section: "user",
            subsection: None,
            name: "email",
            value: email.to_string(),
        });
        entries.push(GitEntry {
            section: "user",
            subsection: None,
            name: "name",
            value: user.to_string(),
        });
    }
    if let Some(base) = server_base.map(str::trim).filter(|b| !b.is_empty()) {
        let base = base.trim_end_matches('/');
        entries.push(GitEntry {
            section: "url",
            subsection: Some(format!("{base}/git/")),
            name: "insteadOf",
            value: "https://github.com/".to_string(),
        });
        entries.push(GitEntry {
            section: "credential",
            subsection: Some(base.to_string()),
            name: "helper",
            value: format!("!{AGENT_BIN} git-credential"),
        });
    }
    entries
}

/// Render `entries` into git-config text with rust-ini. `EscapePolicy::Nothing` is
/// required so the quoted-subsection headers (`[url "…"]`) and values (URLs, the
/// `!helper` command) are written verbatim — the default policy would INI-escape
/// `:`/`"` and corrupt the git config (`https\://…` would break the rewrite).
fn render_gitconfig(entries: &[GitEntry]) -> Result<String> {
    let mut ini = Ini::new();
    for entry in entries {
        let section = match &entry.subsection {
            Some(subsection) => format!("{} \"{subsection}\"", entry.section),
            None => entry.section.to_string(),
        };
        ini.with_section(Some(section))
            .set(entry.name, entry.value.as_str());
    }
    let opt = WriteOption {
        escape_policy: EscapePolicy::Nothing,
        kv_separator: " = ",
        ..WriteOption::default()
    };
    let mut buf = Vec::new();
    ini.write_to_opt(&mut buf, opt)
        .context("serialize .gitconfig")?;
    String::from_utf8(buf).context(".gitconfig is not valid UTF-8")
}

/// The home directory of an existing Unix account, read from `getent passwd`.
fn user_home(user: &str) -> Option<String> {
    let output = Command::new("getent")
        .arg("passwd")
        .arg(user)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    // passwd format: name:passwd:uid:gid:gecos:home:shell
    stdout.trim().split(':').nth(5).map(str::to_string)
}

/// The numeric UID of an existing Unix account named `user`, or `None` if no such
/// account exists (or its UID can't be read). Used to distinguish a fresh login
/// from a pre-existing system account.
fn existing_uid(user: &str) -> Option<u32> {
    let output = Command::new("id")
        .arg("-u")
        .arg(user)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.trim().parse::<u32>().ok()
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
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::{Decision, decide, git_config_entries, render_gitconfig};

    #[test]
    fn no_email_or_server_yields_no_entries() {
        assert!(git_config_entries("jdoe", None, None).is_empty());
        // Only the proxy remotes when the server URL is set but no email is known.
        assert_eq!(
            git_config_entries("jdoe", None, Some("https://cp.example")).len(),
            2
        );
    }

    #[test]
    fn renders_git_native_subsections_and_values() {
        // The point of gix-config over an INI/TOML writer: git's quoted-subsection
        // headers and unquoted values, which git parses exactly.
        let entries = git_config_entries(
            "jdoe",
            Some("jdoe@example.com"),
            Some("https://cp.example/"),
        );
        let out = render_gitconfig(&entries).unwrap();
        assert!(out.contains("[url \"https://cp.example/git/\"]"), "{out}");
        assert!(out.contains("insteadOf = https://github.com/"), "{out}");
        assert!(out.contains("[credential \"https://cp.example\"]"), "{out}");
        assert!(
            out.contains("helper = !/usr/local/sbin/devbox-agent git-credential"),
            "{out}"
        );
        assert!(out.contains("[user]"), "{out}");
        assert!(out.contains("email = jdoe@example.com"), "{out}");
        assert!(out.contains("name = jdoe"), "{out}");
    }

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

    #[test]
    fn reserved_owner_is_refused() {
        // Reserved system / cloud-default names must never provision an account.
        assert_eq!(decide(Some("root")), Decision::Unsafe("root".to_string()));
        assert_eq!(
            decide(Some("ec2-user")),
            Decision::Unsafe("ec2-user".to_string())
        );
    }
}
