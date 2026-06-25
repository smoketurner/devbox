//! Workspace freshening: bring snapshot-seeded repos to near-HEAD before ready.
//!
//! A warm box launches with `/workspace` seeded from a periodically-refreshed EBS
//! snapshot (provisioned by Terraform), so the repos are present but a few
//! minutes-to-hours stale. During warm-up the agent fetches the small delta since
//! the snapshot was cut and resets each repo to its upstream HEAD, so a claimant
//! gets a near-HEAD checkout without paying a full clone at launch.
//!
//! The fetch is **read-only** (a GitHub App installation token supplied off-box via
//! `DEVBOX_GITHUB_TOKEN`) and **time-budgeted**: if the delta is too large to land
//! within the budget, the box still becomes Ready serving the snapshot-age checkout
//! (degrade, don't reap) — a slightly-stale box beats no box, and the claimant can
//! fetch HEAD themselves. The one hard failure is a workspace that was *required*
//! (snapshot expected) but is absent: that means the snapshot failed to attach, so
//! the box is left un-tagged for the reconciler to reap.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::Command;

/// Where the snapshot-seeded repositories live.
const WORKSPACE: &str = "/workspace";

/// Environment variable carrying the read-only GitHub credential for the fetch.
const TOKEN_ENV: &str = "DEVBOX_GITHUB_TOKEN";

/// Environment variable that, when truthy, makes an empty `/workspace` a hard
/// failure (the deployment seeds repos via snapshot, so empty means the snapshot
/// did not attach). Unset/false preserves the no-snapshot behaviour: skip freshen.
const REQUIRE_ENV: &str = "DEVBOX_REQUIRE_WORKSPACE";

/// Environment variable overriding the overall fetch time budget, in seconds.
const BUDGET_ENV: &str = "WARMUP_FETCH_TIMEOUT_SECS";

/// Default overall budget for fetching all repos; stays well under `ready_timeout`.
const DEFAULT_BUDGET: Duration = Duration::from_secs(120);

/// Timeout for local-only git operations (reset/clean) that touch no network.
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Inline git credential helper: emit `x-access-token` plus the token from the
/// child environment. The token itself never appears in the process arguments —
/// only the variable name does.
const CREDENTIAL_HELPER: &str =
    "!f() { echo username=x-access-token; echo \"password=$DEVBOX_GITHUB_TOKEN\"; }; f";

/// What warm-up should do after attempting to freshen the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadyDecision {
    /// Tag the box ready: workspace is fresh, acceptably stale, or not seeded.
    TagReady,
    /// Leave the box un-tagged so the reconciler reaps it: a required workspace is
    /// absent, so the snapshot almost certainly failed to attach.
    FailAndReap,
}

/// Freshen every repository under `/workspace`, returning the readiness decision.
///
/// Each repo is fetched and hard-reset to its upstream HEAD within the shared time
/// budget; a repo that errors or times out is left at its snapshot-age state and
/// logged. The only outcome that withholds readiness is a required-but-empty
/// workspace.
pub(crate) async fn freshen_workspace() -> ReadyDecision {
    let repos = repos_under(Path::new(WORKSPACE));
    let decision = classify(&repos, require_workspace());
    if repos.is_empty() {
        match decision {
            ReadyDecision::FailAndReap => tracing::error!(
                workspace = WORKSPACE,
                "workspace required but no repositories present; snapshot likely failed to attach"
            ),
            ReadyDecision::TagReady => {
                tracing::info!(
                    workspace = WORKSPACE,
                    "no repositories to freshen; skipping"
                );
            }
        }
        return decision;
    }

    let token = github_token();
    if token.is_none() {
        tracing::warn!(
            "{TOKEN_ENV} unset; fetching without credentials (private repos will fail to freshen)"
        );
    }
    let start = Instant::now();
    let budget = fetch_budget();
    for repo in &repos {
        match freshen_repo(repo, token.as_deref(), start, budget).await {
            Ok(()) => tracing::info!(repo = %repo.display(), "freshened to upstream HEAD"),
            Err(e) => tracing::warn!(
                repo = %repo.display(),
                error = %format!("{e:#}"),
                "failed to freshen repo; serving snapshot-age checkout"
            ),
        }
    }
    ReadyDecision::TagReady
}

/// Git repositories directly under `root`: `root` itself if it is a repo, otherwise
/// each immediate child directory containing a `.git` entry. Sorted for determinism.
fn repos_under(root: &Path) -> Vec<PathBuf> {
    if root.join(".git").exists() {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut repos: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").exists())
        .collect();
    repos.sort();
    repos
}

/// Decide readiness from what was discovered. An empty, *required* workspace is the
/// sole hard failure; everything else proceeds (degrade over reap).
fn classify(repos: &[PathBuf], require_workspace: bool) -> ReadyDecision {
    if repos.is_empty() && require_workspace {
        ReadyDecision::FailAndReap
    } else {
        ReadyDecision::TagReady
    }
}

/// Freshen one repo, clearing lock files a killed git op may leave so the box
/// never goes Ready with a wedged repo.
async fn freshen_repo(
    repo: &Path,
    token: Option<&str>,
    start: Instant,
    budget: Duration,
) -> Result<()> {
    clear_stale_locks(repo);
    let outcome = fetch_and_reset(repo, token, start, budget).await;
    // A git op killed at the budget (or one that exits mid-write) can leave a
    // `.git/*.lock` behind; warm-up still tags the box ready, so clear it now or
    // the claimant inherits "Unable to create '.git/index.lock'".
    clear_stale_locks(repo);
    outcome
}

/// Run fetch → reset → clean, charging every step against the shared deadline so
/// the whole pass (across all repos) is bounded by `budget`, not just the fetch.
async fn fetch_and_reset(
    repo: &Path,
    token: Option<&str>,
    start: Instant,
    budget: Duration,
) -> Result<()> {
    let fetch_timeout = budget_remaining(start, budget, repo, "fetch")?;
    run_git(repo, token, &["fetch", "--prune", "origin"], fetch_timeout)
        .await
        .with_context(|| format!("git fetch in {}", repo.display()))?;

    // reset/clean are local-only — no credentials. They are charged against the
    // same budget (capped at LOCAL_GIT_TIMEOUT) so a large repo set cannot push
    // warm-up past `ready_timeout`. `clean -fd` (no `-x`) drops stray untracked
    // files while preserving ignored build caches (e.g. `target/`).
    let reset_timeout = budget_remaining(start, budget, repo, "reset")?.min(LOCAL_GIT_TIMEOUT);
    run_git(repo, None, &["reset", "--hard", "@{u}"], reset_timeout)
        .await
        .with_context(|| format!("git reset in {}", repo.display()))?;

    let clean_timeout = budget_remaining(start, budget, repo, "clean")?.min(LOCAL_GIT_TIMEOUT);
    run_git(repo, None, &["clean", "-fd"], clean_timeout)
        .await
        .with_context(|| format!("git clean in {}", repo.display()))?;
    Ok(())
}

/// Time left in the shared budget, or an error (so the caller degrades) once spent.
fn budget_remaining(start: Instant, budget: Duration, repo: &Path, step: &str) -> Result<Duration> {
    let remaining = budget.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        anyhow::bail!("fetch budget exhausted before {step} in {}", repo.display());
    }
    Ok(remaining)
}

/// Run `git -C <repo> <args>` under GNU `timeout` so the whole process group dies
/// on overrun, not just the top-level `git`.
///
/// `git fetch` spawns helpers (`git-remote-https`, …) in its process group; a
/// parent-only kill would orphan them to keep doing network I/O and writing under
/// `.git`. GNU `timeout` (AL2023 coreutils, like the `git`/`useradd`/`chown` the
/// agent already shells out to) runs `git` in its own group and signals the whole
/// group on expiry — SIGTERM first so `git` removes its own locks, then SIGKILL
/// (`-k`) for stragglers. An outer tokio deadline guards against `timeout` itself
/// misbehaving.
async fn run_git(repo: &Path, token: Option<&str>, args: &[&str], budget: Duration) -> Result<()> {
    let secs = budget.as_secs().max(1);
    let mut cmd = Command::new("timeout");
    cmd.arg("-k")
        .arg("5")
        .arg(secs.to_string())
        .arg("git")
        .arg("-C")
        .arg(repo);
    if let Some(token) = token {
        cmd.arg("-c")
            .arg(format!("credential.helper={CREDENTIAL_HELPER}"))
            .env(TOKEN_ENV, token);
    }
    // Never block on an interactive prompt if a credential is missing or rejected.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .kill_on_drop(true);

    let backstop = budget.saturating_add(Duration::from_secs(10));
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn timeout git {}", args.join(" ")))?;
    match tokio::time::timeout(backstop, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => anyhow::bail!("git {} exited with {:?}", args.join(" "), status.code()),
        Ok(Err(e)) => Err(e).with_context(|| format!("wait on git {}", args.join(" "))),
        Err(_) => {
            child.start_kill().ok();
            child.wait().await.ok();
            anyhow::bail!("git {} exceeded backstop {backstop:?}", args.join(" "))
        }
    }
}

/// Remove git `*.lock` files an interrupted or killed op may leave, so the fetch is
/// re-entrant and the claimant never inherits a wedged repo.
///
/// Sweeps the top level of `.git` (`index.lock`, `packed-refs.lock`, `config.lock`,
/// `FETCH_HEAD.lock`, `shallow.lock`, …) and recurses through `.git/refs` for
/// per-ref locks. `.git/objects` is intentionally skipped — it holds no lock files
/// and is the one subtree large enough to be worth not walking.
fn clear_stale_locks(repo: &Path) {
    let git_dir = repo.join(".git");
    remove_lock_files(&git_dir, false);
    remove_lock_files(&git_dir.join("refs"), true);
}

/// Remove every `*.lock` file directly in `dir`, recursing into subdirectories when
/// `recurse` is set. Missing directories and individual removal errors are ignored
/// (best-effort cleanup); only unexpected errors are logged.
fn remove_lock_files(dir: &Path, recurse: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if recurse {
                remove_lock_files(&path, true);
            }
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "lock") {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "could not remove stale git lock");
                }
            }
        }
    }
}

/// The read-only GitHub token, if present and non-empty.
fn github_token() -> Option<String> {
    std::env::var(TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
}

/// Whether an empty `/workspace` should fail warm-up (snapshot deployments).
fn require_workspace() -> bool {
    parse_require(std::env::var(REQUIRE_ENV).ok().as_deref())
}

/// The overall fetch budget from the environment, or the default.
fn fetch_budget() -> Duration {
    parse_budget(std::env::var(BUDGET_ENV).ok().as_deref())
}

/// Parse the fetch budget; a missing, zero, or unparseable value yields the default.
fn parse_budget(value: Option<&str>) -> Duration {
    value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_BUDGET, Duration::from_secs)
}

/// Parse the require flag; only `1`/`true` (case-insensitive) are truthy.
fn parse_require(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value = value.trim();
    value == "1" || value.eq_ignore_ascii_case("true")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BUDGET, Duration, Instant, Path, PathBuf, ReadyDecision, budget_remaining,
        classify, clear_stale_locks, parse_budget, parse_require, repos_under,
    };
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique, empty temp directory for the calling test (no extra crates needed).
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("devbox-freshen-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn make_repo(parent: &Path, name: &str) {
        std::fs::create_dir_all(parent.join(name).join(".git")).unwrap();
    }

    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn make_plain_dir(parent: &Path, name: &str) {
        std::fs::create_dir_all(parent.join(name)).unwrap();
    }

    #[test]
    fn repos_under_returns_only_git_children_sorted() {
        let root = temp_root();
        make_repo(&root, "beta");
        make_repo(&root, "alpha");
        make_plain_dir(&root, "not-a-repo");

        let found = repos_under(&root);

        assert_eq!(found, vec![root.join("alpha"), root.join("beta")]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repos_under_treats_root_itself_as_a_repo() {
        let root = temp_root();
        std::fs::create_dir_all(root.join(".git")).ok();

        assert_eq!(repos_under(&root), vec![root.clone()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repos_under_missing_directory_is_empty() {
        let missing = Path::new("/no/such/devbox/workspace/path");
        assert!(repos_under(missing).is_empty());
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    fn clear_stale_locks_sweeps_top_level_and_refs_but_keeps_real_files() {
        let root = temp_root();
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("refs/remotes/origin")).unwrap();
        std::fs::create_dir_all(git.join("objects")).unwrap();
        for lock in [
            "index.lock",
            "packed-refs.lock",
            "config.lock",
            "FETCH_HEAD.lock",
        ] {
            std::fs::write(git.join(lock), b"").unwrap();
        }
        std::fs::write(git.join("refs/remotes/origin/main.lock"), b"").unwrap();
        // Non-lock files must survive — including one named like a lock under objects.
        std::fs::write(git.join("config"), b"[core]\n").unwrap();
        std::fs::write(git.join("objects/pack-abc.idx"), b"x").unwrap();

        clear_stale_locks(&root);

        for lock in [
            "index.lock",
            "packed-refs.lock",
            "config.lock",
            "FETCH_HEAD.lock",
        ] {
            assert!(!git.join(lock).exists(), "{lock} should be removed");
        }
        assert!(!git.join("refs/remotes/origin/main.lock").exists());
        assert!(git.join("config").exists());
        assert!(git.join("objects/pack-abc.idx").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classify_fails_only_when_required_and_empty() {
        assert_eq!(classify(&[], true), ReadyDecision::FailAndReap);
        assert_eq!(classify(&[], false), ReadyDecision::TagReady);
        let repos = vec![PathBuf::from("/workspace/repo")];
        assert_eq!(classify(&repos, true), ReadyDecision::TagReady);
        assert_eq!(classify(&repos, false), ReadyDecision::TagReady);
    }

    #[test]
    fn parse_budget_handles_overrides_and_fallbacks() {
        assert_eq!(parse_budget(Some("300")), Duration::from_secs(300));
        assert_eq!(parse_budget(Some("  60 ")), Duration::from_secs(60));
        assert_eq!(parse_budget(None), DEFAULT_BUDGET);
        assert_eq!(parse_budget(Some("0")), DEFAULT_BUDGET);
        assert_eq!(parse_budget(Some("nonsense")), DEFAULT_BUDGET);
    }

    #[test]
    fn budget_remaining_bails_once_spent() {
        let repo = Path::new("/workspace/repo");
        // A spent budget errors so the caller degrades rather than running unbounded.
        assert!(budget_remaining(Instant::now(), Duration::ZERO, repo, "fetch").is_err());
        // A fresh budget leaves time for the step.
        assert!(budget_remaining(Instant::now(), Duration::from_secs(120), repo, "fetch").is_ok());
    }

    #[test]
    fn parse_require_only_truthy_for_one_or_true() {
        assert!(parse_require(Some("1")));
        assert!(parse_require(Some("true")));
        assert!(parse_require(Some(" TRUE ")));
        assert!(!parse_require(Some("0")));
        assert!(!parse_require(Some("no")));
        assert!(!parse_require(None));
    }
}
