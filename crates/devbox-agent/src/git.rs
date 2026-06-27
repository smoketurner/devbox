//! Shared git plumbing: credential-helper runner, clone helper, and token minter factory.
//!
//! Both [`crate::freshen`] and [`crate::checkout`] invoke `git` with an optional
//! short-lived GitHub App token injected via an inline credential helper, wrapped in
//! GNU `timeout` and `kill_on_drop`. This module owns that machinery so it is written
//! once and tested in one place.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::server_client::ServerTokenClient;

/// Env var the credential helper reads the token from. The agent sets it on the
/// git child process — the token is never baked into the binary, logged, or placed
/// in process arguments.
pub(crate) const TOKEN_ENV: &str = "DEVBOX_GITHUB_TOKEN";

/// Inline git credential helper: emit `x-access-token` plus the token from the
/// child environment. The token itself never appears in the process arguments —
/// only the variable name does.
pub(crate) const CREDENTIAL_HELPER: &str =
    "!f() { echo username=x-access-token; echo \"password=$DEVBOX_GITHUB_TOKEN\"; }; f";

/// Run `git -C <repo> <args>` under GNU `timeout <cap>`, so a stuck op is
/// group-killed exactly at `cap` rather than orphaning git's process group.
///
/// `git fetch` spawns helpers (`git-remote-https`, …) in its process group; a
/// parent-only kill would orphan them to keep doing network I/O and writing under
/// `.git`. GNU `timeout` (AL2023 coreutils) runs `git` in its own group and signals
/// the whole group on expiry — SIGTERM first so `git` removes its own locks, then
/// SIGKILL (`-k`) for stragglers.
///
/// The credential helper and token are injected via `-c` and an environment variable
/// — the token never appears in process arguments.
pub(crate) async fn run_git(
    repo: &Path,
    token: Option<&str>,
    args: &[&str],
    cap: Duration,
) -> Result<()> {
    let mut cmd = base_git_cmd(token, cap);
    cmd.arg("-C").arg(repo).args(args);
    await_git(cmd, &format!("git {}", args.join(" "))).await
}

/// Run `git clone <clone_args> <url> <dest>` under GNU `timeout <cap>`.
///
/// Applies the same credential-helper and timeout machinery as [`run_git`], so the
/// token is never written to disk or placed in process arguments.
pub(crate) async fn run_git_clone(
    url: &str,
    dest: &Path,
    clone_args: &[&str],
    token: Option<&str>,
    cap: Duration,
) -> Result<()> {
    let mut cmd = base_git_cmd(token, cap);
    cmd.arg("clone").args(clone_args).arg(url).arg(dest);
    await_git(cmd, &format!("git clone {url}")).await
}

/// Build the control-plane token client from the environment, or `None` when the
/// box is not configured for server-backed minting (`DEVBOX_SERVER_URL` unset).
/// Degrades gracefully so callers can proceed unauthenticated.
pub(crate) async fn build_minter() -> Option<ServerTokenClient> {
    match ServerTokenClient::new().await {
        Ok(Some(client)) => Some(client),
        Ok(None) => {
            tracing::warn!(
                "DEVBOX_SERVER_URL not set; proceeding without credentials \
                 (private repos require authentication)"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "failed to build control-plane token client; proceeding without credentials"
            );
            None
        }
    }
}

/// Build a `timeout -k 5 <cap> git [credential config]` base command. Both
/// [`run_git`] and [`run_git_clone`] extend this with their specific arguments.
///
/// GNU `timeout` (AL2023 coreutils) group-kills git and all its helpers on expiry,
/// so a stalled network fetch can never orphan subprocesses.
fn base_git_cmd(token: Option<&str>, cap: Duration) -> Command {
    let mut cmd = Command::new("timeout");
    cmd.arg("-k")
        .arg("5")
        .arg(cap.as_secs().max(1).to_string())
        .arg("git");
    if let Some(token) = token {
        cmd.arg("-c")
            .arg(format!("credential.helper={CREDENTIAL_HELPER}"))
            .env(TOKEN_ENV, token);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0").kill_on_drop(true);
    cmd
}

/// Await `cmd.status()` and convert non-zero exits to an error.
async fn await_git(mut cmd: Command, description: &str) -> Result<()> {
    let status = cmd
        .status()
        .await
        .with_context(|| format!("run {description}"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{description} exited with {:?}", status.code())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::process::Command;

    use super::{TOKEN_ENV, await_git, base_git_cmd};

    #[test]
    fn base_git_cmd_with_token_sets_credential_env_and_helper() {
        let cmd = base_git_cmd(Some("secret-token"), Duration::from_secs(30));
        let std_cmd = cmd.as_std();

        // TOKEN_ENV must appear in the child env when a token is provided.
        assert!(
            std_cmd.get_envs().any(|(key, _)| key == TOKEN_ENV),
            "TOKEN_ENV ({TOKEN_ENV}) must be set in the child env"
        );

        // A `-c credential.helper=…` argument pair must be present.
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has_helper = args.windows(2).any(|w| match (w.first(), w.get(1)) {
            (Some(flag), Some(val)) => flag == "-c" && val.starts_with("credential.helper="),
            _ => false,
        });
        assert!(has_helper, "credential.helper= must be passed via -c");
    }

    #[test]
    fn base_git_cmd_without_token_omits_credential_env_and_helper() {
        let cmd = base_git_cmd(None, Duration::from_secs(30));
        let std_cmd = cmd.as_std();

        // TOKEN_ENV must not appear in the child env when no token is provided.
        assert!(
            !std_cmd.get_envs().any(|(key, _)| key == TOKEN_ENV),
            "TOKEN_ENV ({TOKEN_ENV}) must not be set in the child env without a token"
        );

        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has_helper = args.windows(2).any(|w| match (w.first(), w.get(1)) {
            (Some(flag), Some(val)) => flag == "-c" && val.starts_with("credential.helper="),
            _ => false,
        });
        assert!(
            !has_helper,
            "credential.helper= must not be set without a token"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::unwrap_used,
        reason = "test assertion; a failure should fail the test"
    )]
    async fn await_git_non_zero_exit_returns_error() {
        // `/usr/bin/false` always exits with code 1 — exercises the non-zero
        // exit → Err branch without requiring the git binary or a real repo.
        let cmd = Command::new("/usr/bin/false");
        let result = await_git(cmd, "false command").await;
        assert!(result.is_err(), "non-zero exit must return Err");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("false command"),
            "error must include the description; got: {msg}"
        );
    }
}
