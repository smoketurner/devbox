//! Session archive pack/restore.
//!
//! A session archive is one `session.tar.gz` capturing the claimant's
//! work-in-progress so `devbox claim --resume` can restore it onto a fresh
//! warm box:
//!
//! - `manifest.json` — version + one entry per repo with local work.
//! - `repos/<dir>.bundle` — a `git bundle` of everything not on `origin`,
//!   preceded by a synthetic snapshot commit (`git add -A && git commit`) so
//!   dirty and untracked files ride the bundle too. The box is terminating;
//!   mutating its checkout is fine.
//! - `home/` — an allowlist of agent-context entries from the claimant's home
//!   (`.claude`, `.gitconfig`, …).
//!
//! Restore is non-destructive to repos absent from the manifest: only listed
//! repos are touched. For each, the bundle is fetched, the original branch is
//! recreated at the snapshot commit, and a `git reset --mixed` to the
//! pre-snapshot HEAD leaves the user's work as unstaged changes (the
//! staged/unstaged distinction is not preserved). Home entries untar over the
//! freshly-provisioned defaults — the session wins there — and are chowned to
//! the claimant.
//!
//! Everything here is filesystem + local git (no network), so pack/restore
//! round-trip under plain unit tests over temp directories.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::freshen::repos_under;

/// Manifest format version.
const MANIFEST_VERSION: u32 = 1;

/// Per-invocation cap for a local git operation (no network involved).
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Total pack budget — comfortably under the server's archive deadline
/// (`SESSION_ARCHIVE_TIMEOUT_SECS`, default 600 s) so the deadline only fires
/// when the box is truly stuck, not on a merely large workspace.
const PACK_BUDGET: Duration = Duration::from_secs(480);

/// Home entries worth carrying across boxes: agent context and shell state.
/// Everything else in the home is provisioning output that a fresh box
/// recreates.
const HOME_ALLOWLIST: &[&str] = &[
    ".claude",
    ".claude.json",
    ".gitconfig",
    ".bash_history",
    ".config",
];

/// Identity for the synthetic snapshot commit.
const SNAPSHOT_IDENT: &[&str] = &[
    "-c",
    "user.email=agent@devbox",
    "-c",
    "user.name=devbox session",
];

/// The session archive manifest.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    /// Repos with local work, in workspace-directory order.
    pub repos: Vec<RepoEntry>,
    /// Whether the archive carries a `home/` tree.
    pub home: bool,
}

/// One repo's snapshot coordinates.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RepoEntry {
    /// Directory name under the workspace.
    pub dir: String,
    /// Branch the claimant was on (`devbox-session` when HEAD was detached).
    pub branch: String,
    /// HEAD before the snapshot commit — the restore resets back to it so the
    /// snapshot's contents become unstaged changes.
    pub head_before: String,
    /// The snapshot commit (equals `head_before` when the tree was clean).
    pub snapshot: String,
}

/// Pack the session into `staging/session.tar.gz` and return its path.
///
/// `home` is the claimant's home directory when known; `None` skips the home
/// tree (repos still pack). Repos with nothing local (no commits off origin,
/// clean tree) are skipped entirely.
///
/// # Errors
///
/// Returns an error when a git operation fails, the pack budget is exhausted,
/// or the archive cannot be written.
pub(crate) async fn pack_session(
    workspace: &Path,
    home: Option<&Path>,
    staging: &Path,
) -> Result<PathBuf> {
    let start = Instant::now();
    let repos_dir = staging.join("repos");
    std::fs::create_dir_all(&repos_dir)
        .with_context(|| format!("create {}", repos_dir.display()))?;

    let mut entries = Vec::new();
    for repo in repos_under(workspace) {
        if start.elapsed() > PACK_BUDGET {
            bail!("session pack budget exhausted");
        }
        if let Some(entry) = pack_repo(&repo, &repos_dir).await? {
            entries.push(entry);
        }
    }

    let home_packed = match home {
        Some(home) => pack_home(home, staging).await?,
        None => false,
    };

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        repos: entries,
        home: home_packed,
    };
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("serialize session manifest")?;
    std::fs::write(staging.join("manifest.json"), manifest_json)
        .context("write session manifest")?;

    // Assemble the single outer archive from whatever pieces exist.
    let archive = staging.join("session.tar.gz");
    let mut parts = vec!["manifest.json".to_string(), "repos".to_string()];
    if home_packed {
        parts.push("home".to_string());
    }
    let mut args = vec![
        "-czf".to_string(),
        archive.display().to_string(),
        "-C".to_string(),
        staging.display().to_string(),
    ];
    args.extend(parts);
    run_tool("tar", &args)
        .await
        .context("pack session archive")?;
    Ok(archive)
}

/// Snapshot and bundle one repo; `Ok(None)` when it has nothing local.
async fn pack_repo(repo: &Path, repos_dir: &Path) -> Result<Option<RepoEntry>> {
    let dir = repo.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    let head_before = git_stdout(repo, &["rev-parse", "HEAD"]).await?;
    let branch = match git_stdout(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await? {
        b if b == "HEAD" => "devbox-session".to_string(),
        b => b,
    };

    // Snapshot dirty/untracked state as a commit so it rides the bundle.
    let dirty = !git_stdout(repo, &["status", "--porcelain"])
        .await?
        .is_empty();
    if dirty {
        run_git(repo, &["add", "-A"]).await?;
        let mut args: Vec<&str> = SNAPSHOT_IDENT.to_vec();
        args.extend([
            "commit",
            "--no-verify",
            "--quiet",
            "-m",
            "devbox session snapshot",
        ]);
        run_git(repo, &args).await?;
    }
    let snapshot = git_stdout(repo, &["rev-parse", "HEAD"]).await?;

    // Anything not on origin? (Includes the snapshot commit when dirty.)
    let local_count = git_stdout(
        repo,
        &[
            "rev-list",
            "--count",
            "--branches",
            "--not",
            "--remotes=origin",
        ],
    )
    .await?;
    if local_count.trim() == "0" {
        return Ok(None);
    }

    let bundle = repos_dir.join(format!("{dir}.bundle"));
    let bundle_path = bundle.display().to_string();
    run_git(
        repo,
        &[
            "bundle",
            "create",
            &bundle_path,
            "--branches",
            "--not",
            "--remotes=origin",
        ],
    )
    .await
    .with_context(|| format!("bundle {dir}"))?;

    Ok(Some(RepoEntry {
        dir,
        branch,
        head_before,
        snapshot,
    }))
}

/// Tar the allowlisted home entries into `staging/home/`; `Ok(false)` when none
/// exist.
async fn pack_home(home: &Path, staging: &Path) -> Result<bool> {
    let present: Vec<&str> = HOME_ALLOWLIST
        .iter()
        .copied()
        .filter(|entry| home.join(entry).exists())
        .collect();
    if present.is_empty() {
        return Ok(false);
    }
    let dest = staging.join("home");
    std::fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
    // cp -a preserves modes/symlinks; the outer tar carries it all.
    for entry in present {
        let from = home.join(entry);
        run_tool(
            "cp",
            &[
                "-a".to_string(),
                from.display().to_string(),
                dest.display().to_string(),
            ],
        )
        .await
        .with_context(|| format!("copy home entry {entry}"))?;
    }
    Ok(true)
}

/// Extract a session archive into `dest`.
///
/// # Errors
///
/// Returns an error when `tar` fails (corrupt or truncated archive).
pub(crate) async fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    run_tool(
        "tar",
        &[
            "-xzf".to_string(),
            archive.display().to_string(),
            "-C".to_string(),
            dest.display().to_string(),
        ],
    )
    .await
    .context("extract session archive")
}

/// Restore an unpacked session (`extracted` = the dir `session.tar.gz` was
/// extracted into) onto `workspace` and `home`, chowning restored home entries
/// to `owner`.
///
/// Repos named in the manifest but absent from the workspace are skipped with
/// a warning. Failures on one repo do not abort the others.
///
/// # Errors
///
/// Returns an error when the manifest is missing/unreadable — partial repo
/// failures only warn.
pub(crate) async fn restore_session(
    extracted: &Path,
    workspace: &Path,
    home: Option<&Path>,
    owner: &str,
) -> Result<()> {
    let manifest_path = extracted.join("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&manifest_text).context("parse session manifest")?;

    for entry in &manifest.repos {
        let repo = workspace.join(&entry.dir);
        if !repo.join(".git").exists() {
            tracing::warn!(
                repo = %entry.dir,
                "session repo not present on this box; skipping"
            );
            continue;
        }
        let bundle = extracted
            .join("repos")
            .join(format!("{}.bundle", entry.dir));
        if let Err(e) = restore_repo(&repo, &bundle, entry).await {
            tracing::warn!(
                repo = %entry.dir,
                error = %format!("{e:#}"),
                "failed to restore session repo; continuing"
            );
        }
    }

    if manifest.home
        && let Some(home) = home
        && let Err(e) = restore_home(extracted, home, owner).await
    {
        tracing::warn!(
            error = %format!("{e:#}"),
            "failed to restore home entries; continuing"
        );
    }

    Ok(())
}

/// Fetch the bundle and land the claimant back on their branch with their
/// work-in-progress as unstaged changes.
async fn restore_repo(repo: &Path, bundle: &Path, entry: &RepoEntry) -> Result<()> {
    let bundle_path = bundle.display().to_string();
    // Bundle refs land under refs/remotes/session/* so the fetch never fights
    // the currently-checked-out branch.
    run_git(
        repo,
        &[
            "fetch",
            &bundle_path,
            "+refs/heads/*:refs/remotes/session/*",
        ],
    )
    .await?;
    run_git(
        repo,
        &["checkout", "--quiet", "-B", &entry.branch, &entry.snapshot],
    )
    .await?;
    // Back to the pre-snapshot HEAD: the snapshot's contents become unstaged
    // changes in the working tree.
    run_git(repo, &["reset", "--quiet", "--mixed", &entry.head_before]).await?;
    Ok(())
}

/// Copy the archived home entries over the fresh home and chown them to the
/// claimant.
async fn restore_home(extracted: &Path, home: &Path, owner: &str) -> Result<()> {
    let source = extracted.join("home");
    let entries =
        std::fs::read_dir(&source).with_context(|| format!("read {}", source.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        run_tool(
            "cp",
            &[
                "-a".to_string(),
                from.display().to_string(),
                home.display().to_string(),
            ],
        )
        .await?;
        let target = home.join(entry.file_name());
        run_tool(
            "chown",
            &[
                "-R".to_string(),
                format!("{owner}:{owner}"),
                target.display().to_string(),
            ],
        )
        .await?;
    }
    Ok(())
}

/// Run `git -C <repo> <args>` under GNU `timeout` (local ops only).
async fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = timeout_cmd("git");
    cmd.arg("-C").arg(repo).args(args);
    let status = cmd
        .status()
        .await
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} exited with {:?}", args.join(" "), status.code());
    }
    Ok(())
}

/// Run `git -C <repo> <args>` and return trimmed stdout.
async fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = timeout_cmd("git");
    cmd.arg("-C").arg(repo).args(args);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} exited with {:?}",
            args.join(" "),
            output.status.code()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a non-git tool (tar/cp/chown) under GNU `timeout`.
async fn run_tool(program: &str, args: &[String]) -> Result<()> {
    let mut cmd = timeout_cmd(program);
    cmd.args(args);
    let status = cmd
        .status()
        .await
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {:?}", status.code());
    }
    Ok(())
}

/// `timeout -k 5 <cap> <program>` base command (same group-kill rationale as
/// [`crate::git::run_git`]).
fn timeout_cmd(program: &str) -> Command {
    let mut cmd = Command::new("timeout");
    cmd.arg("-k")
        .arg("5")
        .arg(LOCAL_GIT_TIMEOUT.as_secs().to_string())
        .arg(program)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    cmd
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("devbox-session-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn git(repo: &Path, args: &[&str]) {
        run_git(repo, args)
            .await
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e:#}"));
    }

    /// A bare "origin" plus a clone with one pushed commit, mimicking a
    /// snapshot-seeded workspace repo.
    async fn seeded_repo(root: &Path, name: &str) -> PathBuf {
        let origin = root.join(format!("{name}-origin.git"));
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", "--quiet", "-b", "main"]).await;

        let repo = root.join("workspace").join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).await;
        git(
            &repo,
            &["remote", "add", "origin", &origin.display().to_string()],
        )
        .await;
        std::fs::write(repo.join("README.md"), "seeded\n").unwrap();
        git(&repo, &["add", "-A"]).await;
        let mut commit: Vec<&str> = SNAPSHOT_IDENT.to_vec();
        commit.extend(["commit", "--quiet", "--no-verify", "-m", "seed"]);
        git(&repo, &commit).await;
        git(&repo, &["push", "--quiet", "origin", "main"]).await;
        repo
    }

    #[tokio::test]
    async fn pack_and_restore_round_trips_wip() {
        let root = temp_root();
        let repo = seeded_repo(&root, "proj").await;
        let workspace = root.join("workspace");

        // Local work: a feature branch with one commit, a dirty tracked file,
        // and an untracked file.
        git(&repo, &["checkout", "--quiet", "-b", "feature"]).await;
        std::fs::write(repo.join("committed.txt"), "local commit\n").unwrap();
        git(&repo, &["add", "-A"]).await;
        let mut commit: Vec<&str> = SNAPSHOT_IDENT.to_vec();
        commit.extend(["commit", "--quiet", "--no-verify", "-m", "wip"]);
        git(&repo, &commit).await;
        std::fs::write(repo.join("README.md"), "dirty edit\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "scratch\n").unwrap();

        // Home context.
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/context.md"), "agent notes\n").unwrap();
        std::fs::write(home.join(".gitconfig"), "[user]\n\temail = a@b\n").unwrap();

        // Pack.
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let archive = pack_session(&workspace, Some(&home), &staging)
            .await
            .expect("pack");
        assert!(archive.exists());

        // A fresh box: a new seeded clone of the same origin, a fresh home.
        let root2 = temp_root();
        let origin = root.join("proj-origin.git");
        let fresh_ws = root2.join("workspace");
        let fresh_repo = fresh_ws.join("proj");
        std::fs::create_dir_all(&fresh_ws).unwrap();
        run_tool(
            "git",
            &[
                "clone".to_string(),
                "--quiet".to_string(),
                origin.display().to_string(),
                fresh_repo.display().to_string(),
            ],
        )
        .await
        .expect("clone fresh repo");
        let fresh_home = root2.join("home");
        std::fs::create_dir_all(&fresh_home).unwrap();

        // Extract + restore. Owner chown: use the current user so it succeeds
        // in tests.
        let extracted = root2.join("extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        run_tool(
            "tar",
            &[
                "-xzf".to_string(),
                archive.display().to_string(),
                "-C".to_string(),
                extracted.display().to_string(),
            ],
        )
        .await
        .expect("extract");
        let owner = users_current().unwrap_or_else(|| "root".to_string());
        restore_session(&extracted, &fresh_ws, Some(&fresh_home), &owner)
            .await
            .expect("restore");

        // Branch, local commit, dirty edit, untracked file all restored.
        let branch = git_stdout(&fresh_repo, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(branch, "feature");
        assert!(fresh_repo.join("committed.txt").exists(), "local commit");
        assert_eq!(
            std::fs::read_to_string(fresh_repo.join("README.md")).unwrap(),
            "dirty edit\n"
        );
        assert!(fresh_repo.join("untracked.txt").exists(), "untracked file");
        // The dirty/untracked material is unstaged changes, not a commit.
        let status = git_stdout(&fresh_repo, &["status", "--porcelain"])
            .await
            .unwrap();
        assert!(!status.is_empty(), "restored WIP must be unstaged");

        // Home entries restored.
        assert_eq!(
            std::fs::read_to_string(fresh_home.join(".claude/context.md")).unwrap(),
            "agent notes\n"
        );
        assert!(fresh_home.join(".gitconfig").exists());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&root2).ok();
    }

    /// The current unix user name (for chown in tests).
    fn users_current() -> Option<String> {
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    #[tokio::test]
    async fn clean_repo_is_skipped() {
        let root = temp_root();
        seeded_repo(&root, "clean").await;
        let workspace = root.join("workspace");
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        pack_session(&workspace, None, &staging)
            .await
            .expect("pack");

        let manifest: Manifest =
            serde_json::from_str(&std::fs::read_to_string(staging.join("manifest.json")).unwrap())
                .unwrap();
        assert!(manifest.repos.is_empty(), "clean repo must not be packed");
        assert!(!manifest.home);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn manifest_repo_missing_on_restore_is_skipped() {
        let root = temp_root();
        let repo = seeded_repo(&root, "gone").await;
        std::fs::write(repo.join("untracked.txt"), "wip\n").unwrap();
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let archive = pack_session(&root.join("workspace"), None, &staging)
            .await
            .expect("pack");

        // Restore into an empty workspace: must not error.
        let root2 = temp_root();
        let extracted = root2.join("extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        run_tool(
            "tar",
            &[
                "-xzf".to_string(),
                archive.display().to_string(),
                "-C".to_string(),
                extracted.display().to_string(),
            ],
        )
        .await
        .unwrap();
        let empty_ws = root2.join("workspace");
        std::fs::create_dir_all(&empty_ws).unwrap();

        restore_session(&extracted, &empty_ws, None, "nobody")
            .await
            .expect("restore over missing repo must not fail");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&root2).ok();
    }
}
