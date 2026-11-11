//! `devbox-agent sandbox`: run sub-agents in Nitro Sandboxes on a claimed box.
//!
//! The claimant, logged in to their box, starts a sub-agent with one command:
//!
//! ```text
//! devbox-agent sandbox run claude fix the flaky reconcile test
//! ```
//!
//! That clones the current repository at `HEAD` onto a new `agent/<name>`
//! branch, packs the clone into the guest's workspace image, and asks the
//! sandbox daemon ([`daemon`], `devbox-agent sandboxd`, root under systemd) to
//! boot a Nitro Sandbox (a hardware-isolated child VM) under Firecracker's
//! jailer to run the harness against the task. The sub-agent never sees the
//! claimant's checkout, credentials, SSH agent or the host's instance metadata.
//! When it stops, its branch is fetched back into the claimant's repository,
//! where it is reviewed and merged with plain git.
//!
//! The CLI does everything it can as the claimant; only starting, stopping,
//! pausing and inspecting the VMM go through the daemon. Records live on the
//! box only (see [`store`]).

mod api;
pub(crate) mod daemon;
mod firecracker;
// The guest init uses AF_VSOCK, which exists on Linux only (as does the guest).
#[cfg(target_os = "linux")]
pub(crate) mod guest;
mod jail;
mod process;
mod relay;
mod store;
mod workspace;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use daemon::{Client, Request, Response};
use firecracker::{CONSOLE_LOG, StopOutcome};
use store::{SandboxMeta, Store, now_unix};

/// `devbox-agent sandbox` subcommands.
#[derive(Subcommand)]
pub(crate) enum SandboxCommand {
    /// Start a sub-agent in a new sandbox with its own clone of the current repo.
    Run(RunArgs),
    /// List this box's sandboxes.
    Ls,
    /// Print a sandbox's console output.
    Logs {
        /// Sandbox name.
        name: String,
        /// Keep printing as output arrives.
        #[arg(short, long)]
        follow: bool,
    },
    /// Show the sub-agent's changes against the commit it started from.
    ///
    /// While it runs, shows what it has pushed so far; once it has exited,
    /// collects its work first.
    Diff {
        /// Sandbox name.
        name: String,
        /// Summarize changed files instead of printing the full diff.
        #[arg(long)]
        stat: bool,
    },
    /// Stop a sandbox and fetch its work onto branch `agent/<name>`.
    Stop {
        /// Sandbox name.
        name: String,
    },
}

/// Arguments for `devbox-agent sandbox run`.
#[derive(Args)]
pub(crate) struct RunArgs {
    /// Harness the sub-agent runs, e.g. `claude` or `codex`.
    harness: String,
    /// The task, as free text (no quoting needed).
    #[arg(required = true, trailing_var_arg = true)]
    prompt: Vec<String>,
    /// Repository to clone (default: the one containing the current directory).
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Sandbox name (default: a random adjective-noun).
    #[arg(long)]
    name: Option<String>,
    /// Guest vCPUs.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=64))]
    vcpus: u8,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u32).range(512..))]
    memory_mib: u32,
    /// Size of the guest's workspace disk in GiB.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=1024))]
    disk_gib: u32,
}

/// Run a `sandbox` subcommand against this box's store and sandbox daemon.
///
/// # Errors
///
/// Returns the subcommand's error, phrased for the person at the terminal.
pub(crate) async fn run(command: SandboxCommand) -> Result<()> {
    let store = Store::from_env();
    let client = Client::new(daemon::socket_path());
    match command {
        SandboxCommand::Run(args) => start(&store, &client, args).await,
        SandboxCommand::Ls => {
            print!("{}", render_list(&store, &client, now_unix()).await?);
            Ok(())
        }
        SandboxCommand::Logs { name, follow } => logs(&store, &name, follow).await,
        SandboxCommand::Diff { name, stat } => diff(&store, &client, &name, stat).await,
        SandboxCommand::Stop { name } => stop(&store, &client, &name).await,
    }
}

/// `sandbox run`.
async fn start(store: &Store, client: &Client, args: RunArgs) -> Result<()> {
    // Fail before any cloning if this host can't run a sandbox at all.
    client.call(&Request::Preflight).await?;
    anyhow::ensure!(
        is_valid_harness(&args.harness),
        "invalid harness {:?} (lowercase letters, digits and '-')",
        args.harness
    );
    let from = match args.repo {
        Some(repo) => repo,
        None => std::env::current_dir().context("read the current directory")?,
    };
    let repo = workspace::repo_root(&from).await?;
    let base = workspace::head_commit(&repo).await?;
    let name = match args.name {
        Some(name) => name,
        None => store.fresh_name()?,
    };
    relay::check_socket_path(&jail::default_vm_dir(&store.dir(&name)?, &name))?;
    let meta = SandboxMeta {
        branch: format!("agent/{name}"),
        name,
        harness: args.harness,
        prompt: args.prompt.join(" "),
        repo,
        base_commit: base,
        created_unix: now_unix(),
        vcpus: args.vcpus,
        memory_mib: args.memory_mib,
        collected: false,
        worktree: None,
    };
    let dir = store.create(&meta)?;

    let started = match prepare(&dir, &meta, args.disk_gib).await {
        Ok(()) => client
            .call(&Request::Start {
                name: meta.name.clone(),
            })
            .await
            .map(drop),
        Err(e) => Err(e),
    };
    if let Err(e) = started {
        // Free the name; a sandbox that never started has nothing to keep.
        std::fs::remove_dir_all(&dir).ok();
        return Err(e);
    }

    println!(
        "started sandbox {name} ({harness}) on branch {branch}\n\
         \n  devbox-agent sandbox logs {name} -f   follow its console\
         \n  devbox-agent sandbox diff {name}      review its changes so far\
         \n  devbox-agent sandbox stop {name}      stop it and collect its work",
        name = meta.name,
        harness = meta.harness,
        branch = meta.branch,
    );
    Ok(())
}

/// Everything `run` does as the claimant before asking the daemon to boot the
/// guest: the clone, the task, the workspace image, and the push inbox.
async fn prepare(dir: &Path, meta: &SandboxMeta, disk_gib: u32) -> Result<()> {
    let stage = dir.join("stage");
    workspace::prepare_clone(
        &meta.repo,
        &meta.base_commit,
        &meta.branch,
        &stage.join("repo"),
    )
    .await?;
    let task = json!({
        "name": meta.name,
        "harness": meta.harness,
        "prompt": meta.prompt,
        "branch": meta.branch,
        "base_commit": meta.base_commit,
        // Host vsock ports the guest image maps to loopback (see `relay`).
        "vsock": {
            "egress_port": relay::EGRESS_PORT,
            "model_port": relay::MODEL_PORT,
            "git_port": relay::GIT_PORT,
            // Guest port the host sends `shutdown` to (see `api`).
            "control_port": api::CONTROL_PORT,
        },
    });
    std::fs::write(
        stage.join("task.json"),
        serde_json::to_vec_pretty(&task).context("serialize task")?,
    )
    .context("write task.json")?;
    firecracker::build_image(&stage, dir, disk_gib).await?;
    std::fs::remove_dir_all(&stage).ok();
    relay::init_inbox(dir).await
}

/// `sandbox stop`.
async fn stop(store: &Store, client: &Client, name: &str) -> Result<()> {
    let meta = store.load(name)?;
    collect_and_report(client, store, meta).await
}

/// Stop a sandbox through the daemon (which also hands its image back), then
/// fetch its branch home (once) and say how to use it.
async fn collect_and_report(client: &Client, store: &Store, mut meta: SandboxMeta) -> Result<()> {
    if meta.collected {
        println!(
            "sandbox {} was already collected onto {}",
            meta.name, meta.branch
        );
        return Ok(());
    }
    let response = client
        .call(&Request::Stop {
            name: meta.name.clone(),
        })
        .await?;
    match response {
        Response::Stopped {
            outcome: StopOutcome::Clean,
        } => println!("{} powered off cleanly", meta.name),
        Response::Stopped {
            outcome: StopOutcome::Forced,
        } => println!(
            "{} didn't power off when asked; stopped it by signal",
            meta.name
        ),
        _ => {}
    }

    let dir = store.dir(&meta.name)?;
    let export = dir.join("export");
    std::fs::remove_dir_all(&export).ok();
    firecracker::export_repo(&dir, &export).await?;
    let collected = workspace::collect(
        &meta.repo,
        &export.join("repo"),
        &meta.name,
        &meta.branch,
        &meta.base_commit,
    )
    .await;
    std::fs::remove_dir_all(&export).ok();
    let collected = collected?;

    firecracker::discard_image(&dir)?;
    meta.collected = true;
    if collected.commits > 0 {
        // Best effort: the branch is safe in the repo whether or not this works.
        let tree = dir.join("worktree");
        match workspace::add_review_worktree(&meta.repo, &tree, &meta.branch).await {
            Ok(()) => meta.worktree = Some(tree),
            Err(e) => eprintln!("warning: no review checkout: {e:#}"),
        }
    }
    store.save(&meta)?;

    let wip = if collected.wip_committed {
        " (uncommitted changes saved as a WIP commit)"
    } else {
        ""
    };
    println!(
        "collected {} commit(s) from {} onto {}{wip}\n",
        collected.commits, meta.name, meta.branch
    );
    if let Some(tree) = &meta.worktree {
        println!("  cd {}   open, build, test", tree.display());
    }
    println!(
        "  devbox-agent sandbox diff {}   review the diff\n  git merge {}   take it",
        meta.name, meta.branch
    );
    Ok(())
}

/// Names of the sandboxes the daemon reports running.
async fn running(client: &Client) -> Result<BTreeSet<String>> {
    match client.call(&Request::Status).await? {
        Response::Status { running } => Ok(running),
        other => anyhow::bail!("unexpected answer from the sandbox daemon: {other:?}"),
    }
}

/// `sandbox diff`.
///
/// While the sandbox runs, shows what it has pushed to its inbox so far; once
/// it has stopped, collects (if needed) and shows the final branch.
async fn diff(store: &Store, client: &Client, name: &str, stat: bool) -> Result<()> {
    let meta = store.load(name)?;
    let dir = store.dir(name)?;
    let is_running = !meta.collected && running(client).await?.contains(name);
    let target = if is_running {
        let live = format!("refs/sandbox/{name}/live");
        let pushed =
            workspace::fetch_live(&meta.repo, &dir.join(relay::INBOX), &meta.branch, &live).await?;
        anyhow::ensure!(
            pushed,
            "sandbox {name} is running and hasn't pushed anything yet; \
             `devbox-agent sandbox stop {name}` collects its work"
        );
        eprintln!("{name} is still running; showing the commits it has pushed so far");
        live
    } else {
        if !meta.collected {
            collect_and_report(client, store, meta.clone()).await?;
        }
        meta.branch.clone()
    };
    let range = format!("{}...{target}", meta.base_commit);
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(&meta.repo).arg("diff");
    if stat {
        cmd.arg("--stat");
    }
    let status = cmd.arg(&range).status().await.context("run git diff")?;
    anyhow::ensure!(status.success(), "git diff {range} failed");
    Ok(())
}

/// `sandbox logs`.
async fn logs(store: &Store, name: &str, follow: bool) -> Result<()> {
    store.load(name)?;
    let log = store.dir(name)?.join(CONSOLE_LOG);
    anyhow::ensure!(log.is_file(), "sandbox {name} has no console output yet");
    let mut cmd = tokio::process::Command::new("tail");
    cmd.args(["-n", "+1"]);
    if follow {
        cmd.arg("-F");
    }
    let status = cmd
        .arg(&log)
        .stdin(Stdio::null())
        .status()
        .await
        .context("run tail")?;
    // `tail -F` ends on Ctrl-C, which is how following is meant to stop.
    anyhow::ensure!(
        status.success() || follow,
        "reading {} failed",
        log.display()
    );
    Ok(())
}

/// The `sandbox ls` table. If the daemon can't be reached, the records are
/// still listed, with their state shown as unknown.
async fn render_list(store: &Store, client: &Client, now: u64) -> Result<String> {
    let metas = store.list()?;
    if metas.is_empty() {
        return Ok(
            "no sandboxes (start one with `devbox-agent sandbox run <harness> <task>`)\n"
                .to_owned(),
        );
    }
    let live = match running(client).await {
        Ok(live) => Some(live),
        Err(e) => {
            eprintln!("warning: {e:#}");
            None
        }
    };
    let mut out = format!(
        "{:<20} {:<9} {:<8} {:<6} {}\n",
        "NAME", "STATE", "HARNESS", "AGE", "REPO"
    );
    for meta in metas {
        let state = match &live {
            Some(live) => state_label(&meta, live.contains(&meta.name)),
            None => "unknown",
        };
        out.push_str(&format!(
            "{:<20} {:<9} {:<8} {:<6} {}\n",
            meta.name,
            state,
            meta.harness,
            age(now.saturating_sub(meta.created_unix)),
            meta.repo.display(),
        ));
    }
    Ok(out)
}

/// A sandbox's lifecycle state, from its record and whether the daemon reports
/// its VMM running.
fn state_label(meta: &SandboxMeta, running: bool) -> &'static str {
    if running {
        "running"
    } else if meta.collected {
        "collected"
    } else {
        // Finished (or crashed) with work not yet fetched.
        "exited"
    }
}

/// Compact age: `42s`, `5m`, `3h`, `2d`.
fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs.checked_div(60).unwrap_or(0)),
        3_600..86_400 => format!("{}h", secs.checked_div(3_600).unwrap_or(0)),
        _ => format!("{}d", secs.checked_div(86_400).unwrap_or(0)),
    }
}

/// Whether `harness` is a plain token the guest can dispatch on.
fn is_valid_harness(harness: &str) -> bool {
    !harness.is_empty()
        && harness.len() <= 32
        && harness
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Helpers for tests that need scratch directories and real git repos.

    #[cfg(target_os = "linux")]
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh, empty, canonical temp directory for the calling test.
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    pub(crate) fn temp_root(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("devbox-sandbox-{label}-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[cfg(target_os = "linux")]
    /// Run git in `dir` with a fixed identity and no signing; return trimmed stdout.
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[cfg(target_os = "linux")]
    /// Create `dir`, write `name` there and commit it.
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    pub(crate) fn commit_file(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "--quiet", "-m", &format!("add {name}")]);
    }

    #[cfg(target_os = "linux")]
    /// A new repository at `dir` with one commit; returns its canonical path.
    #[expect(
        clippy::unwrap_used,
        reason = "test setup; a failure should fail the test"
    )]
    pub(crate) fn init_repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--quiet"]);
        commit_file(dir, "README.md", "hello\n");
        dir.canonicalize().unwrap()
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::daemon::Client;
    use super::store::{SandboxMeta, Store};
    use super::test_support::temp_root;
    use super::{age, is_valid_harness, render_list, state_label};

    fn meta(name: &str, created_unix: u64, collected: bool) -> SandboxMeta {
        SandboxMeta {
            name: name.to_owned(),
            harness: "claude".to_owned(),
            prompt: "fix it".to_owned(),
            repo: "/workspace/devbox".into(),
            base_commit: "abc".to_owned(),
            branch: format!("agent/{name}"),
            created_unix,
            vcpus: 2,
            memory_mib: 4096,
            collected,
            worktree: None,
        }
    }

    #[tokio::test]
    async fn ls_without_a_daemon_still_lists_records() {
        let root = temp_root("ls");
        let store = Store::new(root.join("store"));
        store.create(&meta("calm-quilt", 100, false)).unwrap();
        store.create(&meta("bold-acorn", 50, true)).unwrap();
        let nowhere = Client::new(root.join("no-daemon.sock"));

        let out = render_list(&store, &nowhere, 400).await.unwrap();
        assert_eq!(out.lines().count(), 3, "{out}");
        let mut lines = out.lines();
        assert!(lines.next().unwrap().starts_with("NAME"), "{out}");
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("bold-acorn") && first.contains("unknown") && first.contains("5m"),
            "{out}"
        );
        assert!(lines.next().unwrap().contains("/workspace/devbox"), "{out}");
    }

    #[tokio::test]
    async fn ls_with_no_sandboxes_says_how_to_start_one() {
        let root = temp_root("ls-empty");
        let store = Store::new(root.join("store"));
        let out = render_list(&store, &Client::new(root.join("x.sock")), 0)
            .await
            .unwrap();
        assert!(out.contains("sandbox run"), "{out}");
    }

    #[test]
    fn state_prefers_running_then_collected() {
        let collected = meta("a", 0, true);
        assert_eq!(state_label(&collected, true), "running");
        assert_eq!(state_label(&collected, false), "collected");
        assert_eq!(state_label(&meta("a", 0, false), false), "exited");
    }

    #[test]
    fn age_is_compact() {
        assert_eq!(age(42), "42s");
        assert_eq!(age(300), "5m");
        assert_eq!(age(7_200), "2h");
        assert_eq!(age(172_800), "2d");
    }

    #[test]
    fn harness_must_be_a_plain_token() {
        assert!(is_valid_harness("claude"));
        assert!(is_valid_harness("claude-code"));
        for bad in ["", "Claude", "a b", "../x", "x;rm"] {
            assert!(!is_valid_harness(bad), "{bad:?}");
        }
    }
}

/// End to end, on Linux: the CLI against an in-process daemon, with a stand-in
/// `jailer` and VMM (Python) in place of the real ones.
#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod e2e {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::daemon::{self, Client, Config};
    use super::jail::{Jail, owner_of};
    use super::relay::Upstreams;
    use super::store::{Store, now_unix};
    use super::test_support::{git, init_repo, temp_root};
    use super::{RunArgs, diff, render_list, start, stop};

    /// Stands in for jailer: finds the chroot jailer would have built and execs
    /// the VMM there (as the same user: tests can't chroot or change uid), with
    /// `--id <name>` first, as jailer does.
    const FAKE_JAILER: &str = r#"#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
split = args.index("--")
opts, rest = args[:split], args[split + 1:]
def opt(key):
    return opts[opts.index(key) + 1]
name, exe, base = opt("--id"), opt("--exec-file"), opt("--chroot-base-dir")
root = os.path.join(base, os.path.basename(exe), name, "root")
os.environ["FAKE_CHROOT"] = root
os.chdir(root)
os.execv(sys.executable, [sys.executable, exe, "--id", name] + rest)
"#;

    /// Stands in for the VMM of a guest that is still working: writes an
    /// untracked file into the workspace image (as a sub-agent would), serves a
    /// minimal API and the guest control port, and powers off when asked.
    const FAKE_VMM: &str = r#"#!/usr/bin/env python3
import os, socket, socketserver, subprocess, sys, threading
from http.server import BaseHTTPRequestHandler

root = os.environ["FAKE_CHROOT"]
sandbox = os.path.normpath(os.path.join(root, "..", "..", "..", ".."))
args = sys.argv[1:]
api = root + args[args.index("--api-sock") + 1]
with open(f"{sandbox}/guest.txt", "w") as f:
    f.write("from the guest\n")
subprocess.run(["debugfs", "-w", "-R", f"write {sandbox}/guest.txt /repo/guest.txt",
                f"{root}/workspace.img"], capture_output=True)
with open(f"{sandbox}/booted", "w") as f:
    f.write("yes\n")
class Api(BaseHTTPRequestHandler):
    def address_string(self):
        return "vmm"
    def log_message(self, *a):
        pass
    def reply(self, code, body=b""):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
    def do_PUT(self):
        self.body()
        self.reply(400, b'{"fault_message":"not supported"}')

class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

threading.Thread(target=Server(api, Api).serve_forever, daemon=True).start()
control = socket.socket(socket.AF_UNIX)
control.bind(f"{root}/vsock.sock")
control.listen()
while True:
    conn, _ = control.accept()
    stream = conn.makefile("rwb")
    if stream.readline().strip() != b"CONNECT 1024":
        conn.close()
        continue
    stream.write(b"OK 1073741824\n")
    stream.flush()
    line = stream.readline().strip()
    if line == b"shutdown":
        with open(f"{sandbox}/clean-shutdown", "w") as f:
            f.write("yes\n")
        os._exit(0)
"#;

    /// Kills every process whose command line mentions the test's root when the
    /// test ends, so a failing test doesn't leave a stand-in VMM running.
    struct Reap(PathBuf);

    impl Drop for Reap {
        fn drop(&mut self) {
            let _status = std::process::Command::new("pkill")
                .arg("-f")
                .arg(&self.0)
                .status();
        }
    }

    /// Stands in for the VMM of a guest that finishes its task by itself:
    /// writes its work into the workspace image, then powers off.
    const FAKE_VMM_DONE: &str = r#"#!/usr/bin/env python3
import os, subprocess
root = os.environ["FAKE_CHROOT"]
sandbox = os.path.normpath(os.path.join(root, "..", "..", "..", ".."))
with open(f"{sandbox}/guest.txt", "w") as f:
    f.write("done by the guest\n")
subprocess.run(["debugfs", "-w", "-R", f"write {sandbox}/guest.txt /repo/guest.txt",
                f"{root}/workspace.img"], capture_output=True)
# Stay up past the daemon's boot check, then power off like a finished guest.
import time
time.sleep(1.5)
"#;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A host stand-in under `root` with `vmm` as the VMM, and a daemon serving
    /// it in the background. Returns the store and a client for the daemon.
    async fn host(root: &Path, vmm: &str) -> (Store, Client) {
        for file in ["nitro_enclaves", "vmlinuz", "initrd.img", "rootfs.ext4"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        write_executable(&root.join("jailer"), FAKE_JAILER);
        write_executable(&root.join("firecracker"), vmm);
        let (uid, gid) = owner_of(root).unwrap();
        let store = Store::new(root.join("store"));
        let socket = root.join("sandboxd.sock");
        let config = Config {
            socket: socket.clone(),
            store: store.clone(),
            owner_path: root.to_path_buf(),
            jail: Jail::rooted_at(root, uid, gid),
            upstreams: Upstreams::default(),
        };
        tokio::spawn(daemon::serve(config));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (store, Client::new(socket))
    }

    fn run_args(repo: PathBuf) -> RunArgs {
        RunArgs {
            harness: "claude".to_owned(),
            prompt: vec!["add".to_owned(), "a".to_owned(), "test".to_owned()],
            repo: Some(repo),
            name: Some("calm-quilt".to_owned()),
            vcpus: 2,
            memory_mib: 4096,
            disk_gib: 1,
        }
    }

    async fn ls(store: &Store, client: &Client) -> String {
        render_list(store, client, now_unix()).await.unwrap()
    }

    #[tokio::test]
    async fn run_live_diff_stop_and_collect_through_the_daemon() {
        let root = temp_root("e2e");
        let _reap = Reap(root.clone());
        let (store, client) = host(&root, FAKE_VMM).await;
        let repo = init_repo(&root.join("repo"));

        start(&store, &client, run_args(repo.clone()))
            .await
            .unwrap();
        let dir = store.dir("calm-quilt").unwrap();
        let meta = store.load("calm-quilt").unwrap();
        assert_eq!(meta.prompt, "add a test");
        for _ in 0..50 {
            if dir.join("booted").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // The VMM runs in the chroot jailer built, next to the relay's sockets.
        let chroot = dir.join("j/firecracker/calm-quilt/root");
        assert!(chroot.join("vm.json").is_file() && chroot.join("vsock.sock_9418").exists());
        assert!(!dir.join("stage").exists(), "staging copy must be removed");
        assert!(ls(&store, &client).await.contains("running"));

        // Nothing pushed yet: a live diff says so.
        let err = diff(&store, &client, "calm-quilt", true).await.unwrap_err();
        assert!(format!("{err:#}").contains("hasn't pushed"), "{err:#}");
        // The guest pushes a commit to its inbox (as it would over vsock): a
        // live diff now fetches it, without stopping the sandbox.
        let pusher = root.join("pusher");
        git(
            &root,
            &[
                "clone",
                "--quiet",
                &repo.to_string_lossy(),
                &pusher.to_string_lossy(),
            ],
        );
        std::fs::write(pusher.join("progress.txt"), "halfway\n").unwrap();
        git(&pusher, &["add", "progress.txt"]);
        git(&pusher, &["commit", "--quiet", "-m", "progress"]);
        let inbox = dir.join("inbox.git");
        git(
            &pusher,
            &[
                "push",
                "--quiet",
                &inbox.to_string_lossy(),
                "HEAD:refs/heads/agent/calm-quilt",
            ],
        );
        diff(&store, &client, "calm-quilt", true).await.unwrap();
        let live = git(
            &repo,
            &["ls-tree", "--name-only", "refs/sandbox/calm-quilt/live"],
        );
        assert!(live.contains("progress.txt"), "{live}");
        assert!(ls(&store, &client).await.contains("running"));

        // Stopping asks the guest to power off.
        stop(&store, &client, "calm-quilt").await.unwrap();
        assert!(
            dir.join("clean-shutdown").exists(),
            "guest was not asked to shut down"
        );
        assert!(!dir.join("j").exists(), "the jail is removed");
        assert!(!dir.join("runtime.json").exists());

        let meta = store.load("calm-quilt").unwrap();
        assert!(meta.collected);
        assert!(
            !dir.join("workspace.img").exists(),
            "image is discarded once collected"
        );
        let files = git(&repo, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(files.contains("guest.txt"), "{files}");
        let tree = meta.worktree.clone().unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.join("guest.txt")).unwrap(),
            "from the guest\n"
        );
        assert!(
            !repo.join("guest.txt").exists(),
            "claimant checkout untouched"
        );
        assert!(ls(&store, &client).await.contains("collected"));
        // Stopping again is harmless.
        stop(&store, &client, "calm-quilt").await.unwrap();
    }

    #[tokio::test]
    async fn a_guest_that_finishes_shows_exited_and_diff_collects_it() {
        let root = temp_root("e2e-done");
        let _reap = Reap(root.clone());
        let (store, client) = host(&root, FAKE_VMM_DONE).await;
        let repo = init_repo(&root.join("repo"));

        start(&store, &client, run_args(repo.clone()))
            .await
            .unwrap();
        let mut exited = false;
        for _ in 0..100 {
            if ls(&store, &client).await.contains("exited") {
                exited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(exited, "{}", ls(&store, &client).await);

        // diff on a finished sandbox collects it first, then shows the branch.
        diff(&store, &client, "calm-quilt", true).await.unwrap();
        let files = git(&repo, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(files.contains("guest.txt"), "{files}");
        assert!(ls(&store, &client).await.contains("collected"));
        let dir = store.dir("calm-quilt").unwrap();
        assert!(!dir.join("j").exists(), "the jail is removed");
    }

    #[tokio::test]
    async fn run_reports_a_vmm_that_dies_during_boot_and_leaves_nothing() {
        let root = temp_root("boot-fail");
        let _reap = Reap(root.clone());
        let (store, client) = host(
            &root,
            "#!/bin/sh\necho 'enclave device rejected the VM' >&2\nexit 1\n",
        )
        .await;
        let repo = init_repo(&root.join("repo"));
        let err = start(&store, &client, run_args(repo)).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exited while booting") && msg.contains("rejected the VM"),
            "{msg}"
        );
        assert!(!store.dir("calm-quilt").unwrap().exists(), "{msg}");
    }

    #[tokio::test]
    async fn run_stops_at_the_daemons_preflight() {
        let root = temp_root("e2e-preflight");
        let _reap = Reap(root.clone());
        let (store, client) = host(&root, FAKE_VMM).await;
        std::fs::remove_file(root.join("jailer")).unwrap();
        let repo = init_repo(&root.join("repo"));
        let err = start(&store, &client, run_args(repo)).await.unwrap_err();
        assert!(format!("{err:#}").contains("jailer"), "{err:#}");
        assert!(store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_daemon_run_says_how_to_check_it() {
        let root = temp_root("e2e-nodaemon");
        let store = Store::new(root.join("store"));
        let repo = init_repo(&root.join("repo"));
        let client = Client::new(root.join("absent.sock"));
        let err = start(&store, &client, run_args(repo)).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("systemctl status devbox-sandboxd"),
            "{err:#}"
        );
    }
}
