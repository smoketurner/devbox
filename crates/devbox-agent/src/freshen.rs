//! Workspace freshening: bring snapshot-seeded repos to near-HEAD before ready.
//!
//! A warm box launches with `/workspace` seeded from a periodically-refreshed EBS
//! snapshot (provisioned by Terraform), so the repos are present but a few
//! minutes-to-hours stale. During warm-up the agent fetches the small delta since
//! the snapshot was cut and resets each repo to its upstream HEAD, so a claimant
//! gets a near-HEAD checkout without paying a full clone at launch.
//!
//! The fetch is **read-only** — the agent requests a short-lived, repo-scoped
//! token per repo from the control plane, which mints it from each repo's `origin`
//! (see [`crate::control_plane`]) — and
//! **time-budgeted**: if the delta is too large to land within the budget, the box
//! still becomes Ready serving the snapshot-age checkout (degrade, don't reap) — a
//! slightly-stale box beats no box, and the claimant can fetch HEAD themselves. An
//! absent or empty `/workspace` (e.g. the EBS volume didn't mount, so the directory
//! falls back to the root disk) simply skips freshening and the box still becomes
//! Ready — there is no reap path.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use devbox_common::RepoFreshenReport;
use tokio::process::Command;

use crate::control_plane::ControlPlaneClient;
use crate::git::run_git;

/// Where the snapshot-seeded repositories live.
const WORKSPACE: &str = "/workspace";

/// Bound on a reported per-repo error string, in characters (the server bounds
/// again at its trust boundary; this just keeps the request body small).
const MAX_ERROR_CHARS: usize = 256;

/// Environment variable overriding the overall fetch time budget, in seconds.
const BUDGET_ENV: &str = "WARMUP_FETCH_TIMEOUT_SECS";

/// Default overall budget for fetching all repos; stays well under `ready_timeout`.
const DEFAULT_BUDGET: Duration = Duration::from_secs(120);

/// Smallest budget worth granting the fetch. Below this we degrade instead of
/// running: GNU `timeout`'s granularity is one second and a sub-second fetch is
/// useless.
const MIN_STEP: Duration = Duration::from_secs(1);

/// Fixed cap for the local, near-instant reset/clean that follow a successful fetch.
/// They're charged separately from the network budget so a fetch can never succeed
/// while the working-tree reset is skipped.
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Aggregate outcome of the freshen phase, for the warm-up report.
pub(crate) struct FreshenOutcome {
    /// Per-repo results, one entry per repo found under `/workspace`.
    pub repos: Vec<RepoFreshenReport>,
    /// Wall time of the whole phase (token minting + fetch loop).
    pub total: Duration,
    /// Whether `/workspace` held at least one repo. `false` means the
    /// snapshot-seeded volume didn't deliver (or the box has no workspace).
    pub workspace_present: bool,
}

/// Freshen every repository under `/workspace`.
///
/// Each repo is fetched and hard-reset to its upstream HEAD within the shared time
/// budget; a repo that errors or times out is left at its snapshot-age state and
/// logged. An absent or empty `/workspace` simply skips freshening. The returned
/// [`FreshenOutcome`] feeds the warm-up report; freshening itself never fails
/// warm-up (degrade, don't reap).
///
/// `client` is the caller's control-plane client (`None` when the box isn't
/// configured for it), borrowed rather than built here so the same cached
/// web-identity JWT serves both the token minting and the warm-up report.
pub(crate) async fn freshen_workspace(
    mut client: Option<&mut ControlPlaneClient>,
) -> FreshenOutcome {
    let phase_start = Instant::now();
    let repos = repos_under(Path::new(WORKSPACE));
    if repos.is_empty() {
        tracing::info!(
            workspace = WORKSPACE,
            "no repositories to freshen; skipping"
        );
        return FreshenOutcome {
            repos: Vec::new(),
            total: phase_start.elapsed(),
            workspace_present: false,
        };
    }

    // Resolve a token per repo *before* starting the fetch timer, so installation
    // discovery/mint latency is not charged against the budget — only the git fetch
    // is. Tokens last an hour, well beyond the fetch loop.
    let mut tokens = Vec::with_capacity(repos.len());
    for repo in &repos {
        tokens.push(repo_token(client.as_deref_mut(), repo).await);
    }

    // One shared budget. Each git op is given the time *left* in it (see `run_git`),
    // so once it's spent the remaining repos serve their snapshot-age checkout
    // (already near-HEAD) and the box still goes Ready — better than hanging warm-up
    // until the reaper kills the box.
    let start = Instant::now();
    let budget = fetch_budget();
    let mut reports = Vec::with_capacity(repos.len());
    for (repo, token) in repos.iter().zip(&tokens) {
        let repo_start = Instant::now();
        let result = freshen_repo(repo, token.as_deref(), start, budget).await;
        let duration_ms = millis_u64(repo_start.elapsed());
        match &result {
            Ok(()) => tracing::info!(repo = %repo.display(), "freshened to upstream HEAD"),
            Err(e) => tracing::warn!(
                repo = %repo.display(),
                error = %format!("{e:#}"),
                "stopped freshening repo; serving snapshot-age checkout"
            ),
        }
        reports.push(RepoFreshenReport {
            repo: dir_name(repo),
            success: result.is_ok(),
            duration_ms,
            error: result
                .err()
                .map(|e| truncate_chars(&format!("{e:#}"), MAX_ERROR_CHARS)),
        });
    }
    FreshenOutcome {
        repos: reports,
        total: phase_start.elapsed(),
        workspace_present: true,
    }
}

/// A duration as u64 milliseconds, saturating at `u64::MAX` (`as_millis` is
/// u128; the no-`as`-cast lint wants an explicit, non-truncating conversion).
pub(crate) fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The repo's directory name under `/workspace` (final path component), falling
/// back to the full path display for pathological paths.
fn dir_name(repo: &Path) -> String {
    repo.file_name().map_or_else(
        || repo.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The first `max` characters of `s` (char-boundary-safe truncation).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Git repositories directly under `root`: `root` itself if it is a repo, otherwise
/// each immediate child directory containing a `.git` entry. Sorted for determinism.
pub(crate) fn repos_under(root: &Path) -> Vec<PathBuf> {
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

/// Freshen one repo, clearing lock files a killed git op may leave so the box
/// never goes Ready with a wedged repo.
async fn freshen_repo(
    repo: &Path,
    token: Option<&str>,
    start: Instant,
    budget: Duration,
) -> Result<()> {
    clear_stale_locks(repo);
    let outcome = fetch_reset_clean(repo, token, start, budget).await;
    // A git op killed at the budget (or one that exits mid-write) can leave a
    // `.git/*.lock` behind; warm-up still tags the box ready, so clear it now or
    // the claimant inherits "Unable to create '.git/index.lock'".
    clear_stale_locks(repo);
    outcome
}

/// Fetch the delta since the snapshot, hard-reset to upstream HEAD, and drop stray
/// untracked files. `clean -fd` (no `-x`) preserves ignored build caches (`target/`).
async fn fetch_reset_clean(
    repo: &Path,
    token: Option<&str>,
    start: Instant,
    budget: Duration,
) -> Result<()> {
    // Only the network fetch is charged against the shared budget. If it's spent,
    // skip the whole repo — don't fetch refs we then wouldn't reset onto.
    let Some(fetch_timeout) = step_timeout(start, budget) else {
        anyhow::bail!("fetch budget spent before {}", repo.display());
    };
    run_git(repo, token, &["fetch", "--prune", "origin"], fetch_timeout)
        .await
        .with_context(|| format!("git fetch in {}", repo.display()))?;
    // reset/clean are local and near-instant; always run them after a successful
    // fetch so the working tree actually advances (a fixed cap guards pathology).
    run_git(repo, None, &["reset", "--hard", "@{u}"], LOCAL_GIT_TIMEOUT)
        .await
        .with_context(|| format!("git reset in {}", repo.display()))?;
    run_git(repo, None, &["clean", "-fd"], LOCAL_GIT_TIMEOUT)
        .await
        .with_context(|| format!("git clean in {}", repo.display()))?;
    Ok(())
}

/// Time left in the shared budget, or `None` once too little remains to be worth
/// granting (below `MIN_STEP`), so the caller degrades instead of running.
fn step_timeout(start: Instant, budget: Duration) -> Option<Duration> {
    let left = budget.saturating_sub(start.elapsed());
    (left >= MIN_STEP).then_some(left)
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

/// A read-only token for `repo`'s `origin` owner, or `None` to fetch unauthenticated
/// (no client, no/non-GitHub remote, or the App isn't installed on the owner).
async fn repo_token(client: Option<&mut ControlPlaneClient>, repo: &Path) -> Option<String> {
    let client = client?;
    let url = repo_origin_url(repo).await?;
    match client.token_for(&url).await {
        Ok(Some(token)) => Some(token),
        Ok(None) => {
            tracing::debug!(
                repo = %repo.display(),
                remote = %url,
                "origin is not a repo on the App's GitHub host; fetching unauthenticated"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                repo = %repo.display(),
                error = %format!("{e:#}"),
                "could not mint GitHub token; fetching unauthenticated"
            );
            None
        }
    }
}

/// The `origin` remote URL for `repo`, or `None` if it has no `origin`. Run under
/// GNU `timeout` like the other git ops here, so a wedged `git` can't stall warm-up.
async fn repo_origin_url(repo: &Path) -> Option<String> {
    let output = Command::new("timeout")
        .arg("-k")
        .arg("5")
        .arg(LOCAL_GIT_TIMEOUT.as_secs().max(1).to_string())
        .arg("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BUDGET, Duration, Instant, Path, PathBuf, clear_stale_locks, dir_name, millis_u64,
        parse_budget, repos_under, step_timeout, truncate_chars,
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
    fn parse_budget_handles_overrides_and_fallbacks() {
        assert_eq!(parse_budget(Some("300")), Duration::from_secs(300));
        assert_eq!(parse_budget(Some("  60 ")), Duration::from_secs(60));
        assert_eq!(parse_budget(None), DEFAULT_BUDGET);
        assert_eq!(parse_budget(Some("0")), DEFAULT_BUDGET);
        assert_eq!(parse_budget(Some("nonsense")), DEFAULT_BUDGET);
    }

    #[test]
    fn millis_u64_converts_and_saturates() {
        assert_eq!(millis_u64(Duration::from_millis(1500)), 1500);
        assert_eq!(millis_u64(Duration::ZERO), 0);
        // A duration whose millis exceed u64 saturates instead of truncating.
        assert_eq!(millis_u64(Duration::MAX), u64::MAX);
    }

    #[test]
    fn dir_name_takes_the_final_path_component() {
        assert_eq!(dir_name(Path::new("/workspace/devbox")), "devbox");
        assert_eq!(dir_name(Path::new("relative/repo")), "repo");
        // Pathological path with no final component falls back to the display form.
        assert_eq!(dir_name(Path::new("/")), "/");
    }

    #[test]
    fn truncate_chars_is_codepoint_safe() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        // Multi-byte chars: the bound is characters, and no codepoint is split.
        assert_eq!(truncate_chars("ééééé", 3), "ééé");
    }

    #[test]
    fn step_timeout_none_once_budget_spent() {
        // Spent or sub-second budget -> no grant, so the op is skipped (degrade).
        assert!(step_timeout(Instant::now(), Duration::ZERO).is_none());
        assert!(step_timeout(Instant::now(), Duration::from_millis(500)).is_none());
        // A fresh budget grants ~the whole thing.
        assert!(step_timeout(Instant::now(), Duration::from_secs(120)).is_some());
    }
}
