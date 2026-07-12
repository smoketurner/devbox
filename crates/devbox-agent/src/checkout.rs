//! Clone a list of repositories onto a workspace directory.
//!
//! Mints a per-repo read-only GitHub App installation token for each clone, runs
//! optional per-repo warm hooks under a time budget, then compacts the object store
//! with `git gc`. Run by the snapshot-builder pipeline to seed the EBS workspace
//! volume before a new AMI is cut, and by a developer or agent on a claimed box to
//! add a repo under `/workspace`. Tokens are fetched from the control plane (see
//! [`crate::control_plane`]); `DEVBOX_SERVER_URL` is read from the environment.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::control_plane::ControlPlaneClient;
use crate::git::{control_plane_client, run_git, run_git_clone};

/// Time budget for a single `git clone` operation.
const CLONE_TIMEOUT: Duration = Duration::from_mins(10);

/// Time budget for each per-repo warm hook (`<dest>/.devbox/warm.sh`).
const WARM_HOOK_TIMEOUT: Duration = Duration::from_mins(30);

/// Time budget for the post-clone `git gc` step.
const GC_TIMEOUT: Duration = Duration::from_mins(5);

/// Clone each URL in `repos` onto `workspace`, mint a read-only token per repo,
/// and run warm hooks where present.
///
/// A clone failure is fatal so no broken snapshot is published; a warm-hook or gc
/// failure is a warning and does not abort the remaining repos.
///
/// # Errors
///
/// Returns an error if region cannot be read from IMDS, if any two repos produce the
/// same destination name, or if any `git clone` fails.
pub(crate) async fn run(workspace: &Path, repos: &[String]) -> Result<()> {
    // Make relative workspaces absolute so hook paths stay unambiguous after the
    // child process changes its working directory during warm-hook execution.
    let workspace = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        workspace
            .canonicalize()
            .with_context(|| format!("canonicalize workspace {}", workspace.display()))?
    };

    // Pre-flight: derive and validate all dest names before touching the filesystem.
    // An unresolvable URL is a fatal config error (not skipped) so the build can never
    // succeed with an empty workspace; duplicates are rejected here too, turning an
    // opaque mid-build git failure into a clear config error.
    let mut dest_names: Vec<String> = Vec::with_capacity(repos.len());
    for url in repos {
        let name = dest_name(url).with_context(|| {
            format!("could not derive a destination directory name from repo URL {url:?}")
        })?;
        dest_names.push(name);
    }
    {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for (url, name) in repos.iter().zip(&dest_names) {
            if let Some(prior) = seen.insert(name.as_str(), url.as_str()) {
                anyhow::bail!(
                    "repos {prior:?} and {url:?} both produce destination name {name:?}; \
                     ensure all repo names in the checkout list are unique"
                );
            }
        }
    }

    let mut client = control_plane_client().await;

    for (url, name) in repos.iter().zip(&dest_names) {
        let dest = workspace.join(name);

        let token = resolve_token(client.as_mut(), url).await;

        tracing::info!(url, dest = %dest.display(), "cloning repository");
        run_git_clone(
            url,
            &dest,
            &["--filter=blob:none", "-c", "protocol.version=2"],
            token.as_deref(),
            CLONE_TIMEOUT,
        )
        .await
        .with_context(|| format!("git clone {url} into {}", dest.display()))?;

        run_warm_hook(&dest).await;

        if let Err(e) = run_git(&dest, None, &["gc", "--quiet"], GC_TIMEOUT).await {
            tracing::warn!(
                dest = %dest.display(),
                error = %format!("{e:#}"),
                "git gc failed; continuing"
            );
        }
    }

    Ok(())
}

/// Derive the destination directory name from a clone URL.
///
/// Handles HTTPS URLs (with or without a `.git` suffix, query string, or trailing
/// slash) and SCP-form remotes (`git@host:owner/repo.git`). Returns `None` for URLs
/// with no extractable name (e.g. a bare host, a path component of `.` or `..`).
fn dest_name(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');

    // Isolate the path component so we don't mistake the hostname for a repo name.
    let path_part = if let Some((_, after_scheme)) = url.split_once("://") {
        // Standard URL (https://, ssh://, git://…): skip past authority to path.
        after_scheme.split_once('/').map(|(_, path)| path)?
    } else if let Some((authority, path)) = url.split_once(':') {
        // SCP form — git@host:owner/repo. Reject a local path like `./repo:tag`.
        if authority.contains('/') {
            return None;
        }
        path
    } else {
        // No scheme and no colon — treat as a bare path and take the last segment.
        url
    };

    let last = path_part.trim_end_matches('/').rsplit('/').next()?;
    // Strip query string (`?…`) and fragment (`#…`) before stripping `.git`, so a
    // URL like `repo.git?ref=main` produces `repo` and not `repo.git?ref=main`.
    let last = last.split_once('?').map_or(last, |(base, _)| base);
    let last = last.split_once('#').map_or(last, |(base, _)| base);
    let name = last.strip_suffix(".git").unwrap_or(last);
    // Reject path-traversal segments: `workspace.join("..")` escapes the tree and
    // `workspace.join(".")` resolves to the workspace root itself (non-empty).
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.to_string())
}

/// Resolve a read-only token for `url` via the control-plane client, logging at the
/// appropriate level when unavailable so the caller can proceed unauthenticated.
async fn resolve_token(client: Option<&mut ControlPlaneClient>, url: &str) -> Option<String> {
    let client = client?;
    match client.token_for(url).await {
        Ok(Some(token)) => Some(token),
        Ok(None) => {
            tracing::debug!(
                url,
                "not a repo on the App's GitHub host; cloning unauthenticated"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                url,
                error = %format!("{e:#}"),
                "could not mint GitHub App token; cloning unauthenticated \
                 (private repos will fail to authenticate)"
            );
            None
        }
    }
}

/// Run `<dest>/.devbox/warm.sh` if it exists and is executable.
///
/// A warm-hook failure is logged as a warning and does not abort the clone loop.
async fn run_warm_hook(dest: &Path) {
    let hook = dest.join(".devbox/warm.sh");
    if !is_executable(&hook) {
        return;
    }
    tracing::info!(dest = %dest.display(), "running warm hook");
    match run_hook_process(&hook, dest).await {
        Ok(()) => tracing::info!(dest = %dest.display(), "warm hook completed"),
        Err(e) => tracing::warn!(
            dest = %dest.display(),
            error = %format!("{e:#}"),
            "warm hook failed; continuing"
        ),
    }
}

/// Return `true` when `path` exists and has at least one executable bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Run the hook script under GNU `timeout` with `cwd = dest`, inheriting the
/// process environment minus `DEVBOX_SERVER_URL` so a repo-controlled warm hook
/// can't trivially reach the control plane to mint tokens. (The GitHub App key is
/// no longer on the box, so the worst a hook could obtain via the instance profile
/// is a bounded, repo-scoped, read-only token — but withholding the server URL is
/// cheap defense-in-depth.)
async fn run_hook_process(hook: &Path, dest: &Path) -> Result<()> {
    let status = tokio::process::Command::new("timeout")
        .arg("-k")
        .arg("5")
        .arg(WARM_HOOK_TIMEOUT.as_secs().to_string())
        .arg(hook)
        .current_dir(dest)
        .env_remove("DEVBOX_SERVER_URL")
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("run warm hook {}", hook.display()))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("warm hook exited with {:?}", status.code())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{dest_name, is_executable};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique, empty temp directory for the calling test.
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn tempdir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("devbox-checkout-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- dest_name ---

    #[test]
    fn dest_name_https_with_git_suffix() {
        assert_eq!(
            dest_name("https://github.com/smoketurner/devbox.git"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn dest_name_https_without_git_suffix() {
        assert_eq!(
            dest_name("https://github.com/smoketurner/devbox"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn dest_name_https_with_trailing_slash() {
        assert_eq!(
            dest_name("https://github.com/smoketurner/devbox/"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn dest_name_scp_form_with_git_suffix() {
        assert_eq!(
            dest_name("git@github.com:smoketurner/devbox.git"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn dest_name_scp_form_without_git_suffix() {
        assert_eq!(
            dest_name("git@github.com:smoketurner/devbox"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn dest_name_local_path_scp_returns_none() {
        // SCP authority contains '/' — not a valid git remote host.
        assert_eq!(dest_name("./repo:tag"), None);
        assert_eq!(dest_name("/abs/path:main"), None);
    }

    #[test]
    fn dest_name_bare_host_returns_none() {
        // No path after the authority — cannot derive a repo name.
        assert_eq!(dest_name("https://github.com/"), None);
        assert_eq!(dest_name("https://github.com"), None);
    }

    #[test]
    fn dest_name_empty_returns_none() {
        assert_eq!(dest_name(""), None);
    }

    #[test]
    fn dest_name_strips_query_string() {
        // `repo.git?ref=main` must produce `repo`, not `repo.git?ref=main`.
        assert_eq!(
            dest_name("https://github.com/owner/repo.git?ref=main"),
            Some("repo".to_string())
        );
        assert_eq!(
            dest_name("https://github.com/owner/repo?token=abc"),
            Some("repo".to_string())
        );
    }

    #[test]
    fn dest_name_strips_fragment() {
        assert_eq!(
            dest_name("https://github.com/owner/repo.git#readme"),
            Some("repo".to_string())
        );
    }

    #[test]
    fn dest_name_dot_dot_returns_none() {
        // `..` would escape the workspace directory via `workspace.join("..")`.
        assert_eq!(dest_name("https://github.com/owner/.."), None);
        assert_eq!(dest_name("git@github.com:owner/.."), None);
    }

    #[test]
    fn dest_name_dot_returns_none() {
        // `.` resolves to the workspace root itself, which is already non-empty.
        assert_eq!(dest_name("https://github.com/owner/."), None);
    }

    // --- is_executable / warm-hook discovery ---

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn warm_hook_detected_when_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let hook_dir = dir.join(".devbox");
        std::fs::create_dir_all(&hook_dir).unwrap();
        let hook = hook_dir.join("warm.sh");
        std::fs::write(&hook, b"#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(is_executable(&hook));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn warm_hook_not_detected_when_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let hook_dir = dir.join(".devbox");
        std::fs::create_dir_all(&hook_dir).unwrap();
        let hook = hook_dir.join("warm.sh");
        std::fs::write(&hook, b"#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(!is_executable(&hook));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn warm_hook_not_detected_when_missing() {
        let dir = tempdir();
        let hook = dir.join(".devbox/warm.sh");
        assert!(!is_executable(&hook));
        std::fs::remove_dir_all(&dir).ok();
    }
}
