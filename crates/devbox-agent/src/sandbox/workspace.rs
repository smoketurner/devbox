//! The sandbox's copy of the claimant's repository, and bringing its work back.
//!
//! A sandbox never sees the claimant's checkout. It gets a fresh clone at the
//! claimant's `HEAD` on a new `agent/<name>` branch. When it stops, that branch
//! is fetched back into the claimant's repository. Nothing else comes back: no
//! hooks, no config, no working-tree files.
//!
//! The guest is untrusted, so its exported repository is checked and sanitized
//! before any git command runs inside it:
//!
//! - no symlink anywhere under `.git`, so writing the sanitized config, removing
//!   hooks, or committing leftover changes can't be redirected onto the host;
//! - `.git/config` is replaced with a minimal one (keeping only the object
//!   format and partial-clone markers git needs to read the store), so
//!   guest-written `core.fsmonitor`, filters, aliases or `uploadpack.*` never run;
//! - `.git/hooks` is deleted, and every command also runs with
//!   `core.hooksPath=/dev/null`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::git::{git_output, run_git, run_git_clone};

/// Time budget for each git step.
const GIT_TIMEOUT: Duration = Duration::from_mins(5);

/// Flags for every git command that touches a guest-written repository.
const UNTRUSTED: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// The top level of the git repository containing `path`.
///
/// # Errors
///
/// Returns an error if `path` is not inside a git repository.
pub(crate) async fn repo_root(path: &Path) -> Result<PathBuf> {
    let top = git_output(path, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT)
        .await
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
    Ok(PathBuf::from(top))
}

/// The commit `HEAD` points at in `repo`.
///
/// # Errors
///
/// Returns an error if `repo` has no commits.
pub(crate) async fn head_commit(repo: &Path) -> Result<String> {
    git_output(
        repo,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        GIT_TIMEOUT,
    )
    .await
    .with_context(|| format!("{} has no commits to start from", repo.display()))
}

/// Clone `repo` into `dest`, checked out at `base` on a new branch `branch`.
///
/// The clone's `origin` points at `repo`'s own `origin` (so the guest fetches
/// upstream, never the claimant's checkout). When `repo` is a blobless partial
/// clone, as `/workspace` checkouts are, the promisor settings are carried over
/// so git treats the missing blobs as fetchable rather than corrupt.
///
/// # Errors
///
/// Returns an error if `branch` already exists in `repo` (collecting would
/// overwrite it), or if any git step fails.
pub(crate) async fn prepare_clone(
    repo: &Path,
    base: &str,
    branch: &str,
    dest: &Path,
) -> Result<()> {
    let existing = git_output(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        GIT_TIMEOUT,
    )
    .await;
    anyhow::ensure!(
        existing.is_err(),
        "branch {branch} already exists in {}; pick another --name",
        repo.display()
    );

    let source = repo
        .to_str()
        .with_context(|| format!("repository path {} is not UTF-8", repo.display()))?;
    run_git_clone(
        source,
        dest,
        &["--quiet", "--no-hardlinks", "--no-checkout"],
        None,
        GIT_TIMEOUT,
    )
    .await?;

    match git_output(repo, &["remote", "get-url", "origin"], GIT_TIMEOUT).await {
        Ok(url) => {
            run_git(
                dest,
                None,
                &["remote", "set-url", "origin", &url],
                GIT_TIMEOUT,
            )
            .await?
        }
        Err(_) => run_git(dest, None, &["remote", "remove", "origin"], GIT_TIMEOUT).await?,
    }
    carry_partial_clone(repo, dest).await?;

    run_git(
        dest,
        None,
        &["checkout", "--quiet", "-b", branch, base],
        GIT_TIMEOUT,
    )
    .await
}

/// Copy `repo`'s partial-clone settings onto `dest`, if it has any.
async fn carry_partial_clone(repo: &Path, dest: &Path) -> Result<()> {
    let promisor = git_output(
        repo,
        &["config", "--get", "remote.origin.promisor"],
        GIT_TIMEOUT,
    )
    .await
    .unwrap_or_default();
    if promisor != "true" {
        return Ok(());
    }
    let filter = git_output(
        repo,
        &["config", "--get", "remote.origin.partialclonefilter"],
        GIT_TIMEOUT,
    )
    .await
    .unwrap_or_else(|_| "blob:none".to_owned());
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialclone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.partialclonefilter", filter.as_str()),
    ] {
        run_git(dest, None, &["config", key, value], GIT_TIMEOUT).await?;
    }
    Ok(())
}

/// What collecting a sandbox brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Collected {
    /// Commits on the branch beyond the base commit.
    pub(crate) commits: u64,
    /// Whether uncommitted guest changes were committed as a WIP commit first.
    pub(crate) wip_committed: bool,
}

/// Fetch `branch` from the guest's exported repository at `export` into `repo`.
///
/// Uncommitted changes left on `branch` are committed first as a WIP commit, so
/// nothing the sub-agent did is lost.
///
/// # Errors
///
/// Returns an error if the export fails the safety checks (see the module docs),
/// if the guest no longer has `branch`, or if any git step fails (for example the
/// claimant has `branch` checked out).
pub(crate) async fn collect(
    repo: &Path,
    export: &Path,
    name: &str,
    branch: &str,
    base: &str,
) -> Result<Collected> {
    sanitize_export(export).await?;
    let wip_committed = commit_leftovers(export, name, branch).await?;

    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    let source = export
        .to_str()
        .with_context(|| format!("export path {} is not UTF-8", export.display()))?;
    run_git(
        repo,
        None,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            source,
            &refspec,
        ],
        GIT_TIMEOUT,
    )
    .await
    .with_context(|| format!("fetch {branch} from the sandbox"))?;

    let count = git_output(
        repo,
        &[
            "rev-list",
            "--count",
            &format!("{base}..refs/heads/{branch}"),
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let commits = count
        .parse()
        .with_context(|| format!("unexpected rev-list count {count:?}"))?;
    Ok(Collected {
        commits,
        wip_committed,
    })
}

/// Fetch what a running sandbox has pushed to its inbox onto `live_ref` in
/// `repo`. Returns `false` when nothing has been pushed to `branch` yet.
///
/// The inbox is a bare repository the host created and only `receive-pack`
/// writes to, so its config and hooks are the host's own; pushed objects were
/// fsck-checked on the way in, and are checked again here.
///
/// # Errors
///
/// Returns an error if a git step fails.
pub(crate) async fn fetch_live(
    repo: &Path,
    inbox: &Path,
    branch: &str,
    live_ref: &str,
) -> Result<bool> {
    let pushed = git_output(
        inbox,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        GIT_TIMEOUT,
    )
    .await
    .is_ok();
    if !pushed {
        return Ok(false);
    }
    let source = inbox
        .to_str()
        .with_context(|| format!("inbox path {} is not UTF-8", inbox.display()))?;
    run_git(
        repo,
        None,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "fetch.fsckObjects=true",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            source,
            &format!("+refs/heads/{branch}:{live_ref}"),
        ],
        GIT_TIMEOUT,
    )
    .await
    .with_context(|| format!("fetch {branch} from the sandbox inbox"))?;
    Ok(true)
}

/// Check `branch` out into a new linked worktree at `path`, so the claimant can
/// open, build and test the sub-agent's work before merging it.
///
/// This runs only after [`collect`], so the files written here are exactly the
/// committed contents of the fetched branch, never the guest's working tree.
///
/// # Errors
///
/// Returns an error if `path` exists or git refuses (e.g. `branch` is already
/// checked out elsewhere).
pub(crate) async fn add_review_worktree(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    let target = path
        .to_str()
        .with_context(|| format!("worktree path {} is not UTF-8", path.display()))?;
    run_git(
        repo,
        None,
        &["worktree", "add", "--quiet", target, branch],
        GIT_TIMEOUT,
    )
    .await
    .with_context(|| format!("check out {branch} at {target}"))
}

/// Reject symlinks under `.git` and replace its config and hooks (module docs).
async fn sanitize_export(export: &Path) -> Result<()> {
    let git_dir = export.join(".git");
    let meta = std::fs::symlink_metadata(&git_dir)
        .with_context(|| format!("sandbox export has no .git at {}", export.display()))?;
    anyhow::ensure!(
        meta.is_dir(),
        "sandbox export's .git is not a directory; refusing to collect"
    );
    reject_symlinks(&git_dir)?;

    let config_path = git_dir.join("config");
    let object_format = config_value(&config_path, "extensions.objectformat").await;
    let partial = config_value(&config_path, "extensions.partialclone")
        .await
        .is_some();

    let mut config = String::from("[core]\n");
    let version = if object_format.is_some() || partial {
        1
    } else {
        0
    };
    config.push_str(&format!(
        "\trepositoryformatversion = {version}\n\tbare = false\n"
    ));
    if object_format.is_some() || partial {
        config.push_str("[extensions]\n");
    }
    if let Some(format) = object_format.filter(|f| f == "sha1" || f == "sha256") {
        config.push_str(&format!("\tobjectformat = {format}\n"));
    }
    if partial {
        // No url: anything missing stays missing instead of reaching the network.
        config.push_str("\tpartialclone = origin\n[remote \"origin\"]\n\tpromisor = true\n");
    }
    std::fs::write(&config_path, config)
        .with_context(|| format!("write sanitized {}", config_path.display()))?;

    let hooks = git_dir.join("hooks");
    if hooks.exists() {
        std::fs::remove_dir_all(&hooks).with_context(|| format!("remove {}", hooks.display()))?;
    }
    Ok(())
}

/// Read `key` from the config file at `path` without running anything it names.
async fn config_value(path: &Path, key: &str) -> Option<String> {
    let file = path.to_str()?;
    git_output(
        Path::new("/"),
        &["config", "--file", file, "--get", key],
        GIT_TIMEOUT,
    )
    .await
    .ok()
}

/// Fail if anything beneath `root` is a symlink.
fn reject_symlinks(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("list {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("list {}", dir.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            anyhow::ensure!(
                !file_type.is_symlink(),
                "sandbox export contains a symlink at {}; refusing to collect",
                entry.path().display()
            );
            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Commit uncommitted changes on `branch` in the export. Returns whether it did.
async fn commit_leftovers(export: &Path, name: &str, branch: &str) -> Result<bool> {
    let head = git_output(
        export,
        &[UNTRUSTED[0], UNTRUSTED[1], "symbolic-ref", "-q", "HEAD"],
        GIT_TIMEOUT,
    )
    .await
    .unwrap_or_default();
    if head != format!("refs/heads/{branch}") {
        return Ok(false);
    }
    let status = git_output(
        export,
        &[
            UNTRUSTED[0],
            UNTRUSTED[1],
            "status",
            "--porcelain",
            "--untracked-files=all",
        ],
        GIT_TIMEOUT,
    )
    .await?;
    if status.is_empty() {
        return Ok(false);
    }
    let message = format!("WIP: uncommitted changes from sandbox {name}");
    let email = format!("sandbox+{name}@devbox.invalid");
    for args in [
        vec![UNTRUSTED[0], UNTRUSTED[1], "add", "--all"],
        vec![
            UNTRUSTED[0],
            UNTRUSTED[1],
            "-c",
            "user.name=devbox sandbox",
            "-c",
            &format!("user.email={email}"),
            "commit",
            "--quiet",
            "--no-verify",
            "--no-gpg-sign",
            "-m",
            &message,
        ],
    ] {
        run_git(export, None, &args, GIT_TIMEOUT).await?;
    }
    Ok(true)
}

// These drive real git through GNU `timeout`, which the agent's Linux hosts
// have and macOS does not.
#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::path::Path;

    use super::{add_review_worktree, collect, head_commit, prepare_clone, repo_root};
    use crate::sandbox::test_support::{commit_file, git, init_repo, temp_root};

    #[tokio::test]
    async fn clone_starts_at_head_on_the_agent_branch_with_upstream_origin() {
        let root = temp_root("ws-clone");
        let repo = init_repo(&root.join("repo"));
        git(
            &repo,
            &["remote", "add", "origin", "https://example.invalid/r.git"],
        );
        let base = head_commit(&repo).await.unwrap();

        let dest = root.join("sandbox");
        prepare_clone(&repo, &base, "agent/calm-quilt", &dest)
            .await
            .unwrap();

        assert_eq!(git(&dest, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            git(&dest, &["branch", "--show-current"]),
            "agent/calm-quilt"
        );
        assert_eq!(
            git(&dest, &["remote", "get-url", "origin"]),
            "https://example.invalid/r.git"
        );
        assert!(dest.join("README.md").exists());
    }

    #[tokio::test]
    async fn repo_root_finds_the_top_level_from_a_subdirectory() {
        let root = temp_root("ws-root");
        let repo = init_repo(&root.join("repo"));
        std::fs::create_dir_all(repo.join("a/b")).unwrap();
        assert_eq!(repo_root(&repo.join("a/b")).await.unwrap(), repo);
        assert!(repo_root(&root).await.is_err());
    }

    #[tokio::test]
    async fn clone_refuses_an_existing_branch() {
        let root = temp_root("ws-existing");
        let repo = init_repo(&root.join("repo"));
        git(&repo, &["branch", "agent/calm-quilt"]);
        let base = head_commit(&repo).await.unwrap();
        let err = prepare_clone(&repo, &base, "agent/calm-quilt", &root.join("sb"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    #[tokio::test]
    async fn collect_fetches_commits_and_wip_back_onto_the_branch() {
        let root = temp_root("ws-collect");
        let repo = init_repo(&root.join("repo"));
        let base = head_commit(&repo).await.unwrap();
        let export = root.join("export");
        prepare_clone(&repo, &base, "agent/calm-quilt", &export)
            .await
            .unwrap();

        // The guest commits once and leaves an untracked file behind.
        commit_file(&export, "fix.txt", "fixed\n");
        std::fs::write(export.join("notes.txt"), "half done\n").unwrap();

        let got = collect(&repo, &export, "calm-quilt", "agent/calm-quilt", &base)
            .await
            .unwrap();
        assert_eq!(got.commits, 2);
        assert!(got.wip_committed);
        let files = git(&repo, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(
            files.contains("fix.txt") && files.contains("notes.txt"),
            "{files}"
        );
        // The claimant's checkout is untouched.
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
        assert!(!repo.join("fix.txt").exists());
    }

    #[tokio::test]
    async fn review_worktree_checks_out_the_collected_branch() {
        let root = temp_root("ws-worktree");
        let repo = init_repo(&root.join("repo"));
        let base = head_commit(&repo).await.unwrap();
        let export = root.join("export");
        prepare_clone(&repo, &base, "agent/calm-quilt", &export)
            .await
            .unwrap();
        commit_file(&export, "fix.txt", "fixed\n");
        collect(&repo, &export, "calm-quilt", "agent/calm-quilt", &base)
            .await
            .unwrap();

        let tree = root.join("review");
        add_review_worktree(&repo, &tree, "agent/calm-quilt")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.join("fix.txt")).unwrap(),
            "fixed\n"
        );
        assert_eq!(
            git(&tree, &["branch", "--show-current"]),
            "agent/calm-quilt"
        );
        // A second checkout of the same branch is refused, not clobbered.
        assert!(
            add_review_worktree(&repo, &root.join("again"), "agent/calm-quilt")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn collect_neutralizes_guest_config_and_hooks() {
        let root = temp_root("ws-hostile");
        let repo = init_repo(&root.join("repo"));
        let base = head_commit(&repo).await.unwrap();
        let export = root.join("export");
        prepare_clone(&repo, &base, "agent/calm-quilt", &export)
            .await
            .unwrap();
        commit_file(&export, "fix.txt", "fixed\n");

        // A hostile guest plants an fsmonitor command and a hook that would
        // leave a marker if either ever ran on the host.
        let marker = root.join("pwned");
        git(
            &export,
            &[
                "config",
                "core.fsmonitor",
                &format!("touch {}", marker.display()),
            ],
        );
        let hook = export.join(".git/hooks/pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::write(export.join("dirty.txt"), "x\n").unwrap();

        collect(&repo, &export, "calm-quilt", "agent/calm-quilt", &base)
            .await
            .unwrap();
        assert!(!marker.exists(), "guest-controlled command ran on the host");
        let config = std::fs::read_to_string(export.join(".git/config")).unwrap();
        assert!(!config.contains("fsmonitor"), "{config}");
    }

    #[tokio::test]
    async fn collect_refuses_symlinks_under_git() {
        let root = temp_root("ws-symlink");
        let repo = init_repo(&root.join("repo"));
        let base = head_commit(&repo).await.unwrap();
        let export = root.join("export");
        prepare_clone(&repo, &base, "agent/calm-quilt", &export)
            .await
            .unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "precious\n").unwrap();
        std::fs::remove_file(export.join(".git/config")).unwrap();
        std::os::unix::fs::symlink(&victim, export.join(".git/config")).unwrap();

        let err = collect(&repo, &export, "calm-quilt", "agent/calm-quilt", &base)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
    }

    #[tokio::test]
    async fn partial_clone_settings_carry_over() {
        let root = temp_root("ws-partial");
        let upstream = init_repo(&root.join("upstream"));
        commit_file(&upstream, "second.txt", "2\n");
        git(&upstream, &["config", "uploadpack.allowfilter", "true"]);
        let url = format!("file://{}", upstream.display());
        let partial = root.join("partial");
        git(
            Path::new("/"),
            &[
                "clone",
                "--quiet",
                "--filter=blob:none",
                &url,
                &partial.to_string_lossy(),
            ],
        );
        let base = head_commit(&partial).await.unwrap();
        let dest = root.join("sb");
        prepare_clone(&partial, &base, "agent/calm-quilt", &dest)
            .await
            .unwrap();
        assert_eq!(
            git(&dest, &["config", "--get", "remote.origin.promisor"]),
            "true"
        );
        assert_eq!(
            git(&dest, &["config", "--get", "extensions.partialclone"]),
            "origin"
        );
        assert_eq!(git(&dest, &["remote", "get-url", "origin"]), url);
    }
}
