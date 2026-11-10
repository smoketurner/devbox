//! `devbox-agent sandbox`: run sub-agents in Nitro Sandboxes on a claimed box.
//!
//! The claimant, logged in to their box, starts a sub-agent with one command:
//!
//! ```text
//! devbox-agent sandbox run claude fix the flaky reconcile test
//! ```
//!
//! That clones the current repository at `HEAD` onto a new `agent/<name>`
//! branch, packs the clone into the guest's workspace image, and boots a Nitro
//! Sandbox (a hardware-isolated child VM) to run the harness against the task.
//! The sub-agent never sees the claimant's checkout, credentials, SSH agent or
//! the host's instance metadata. When it stops, its branch is fetched back into
//! the claimant's repository, where it is reviewed and merged with plain git.
//!
//! Records live on the box only (see [`store`]); nothing here talks to the
//! control plane.

mod api;
mod firecracker;
mod process;
mod relay;
mod store;
mod workspace;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use api::VmState;
use firecracker::{CONSOLE_LOG, Firecracker, StopOutcome};
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
    /// Collects the work first if the sandbox has exited on its own.
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
    /// Pause a running sandbox (its vCPUs stop; memory and state are kept).
    Pause {
        /// Sandbox name.
        name: String,
    },
    /// Resume a paused sandbox.
    Resume {
        /// Sandbox name.
        name: String,
    },
    /// Serve a sandbox's vsock channel (started by `run`; not for direct use).
    #[command(hide = true)]
    Relay {
        /// Sandbox name.
        name: String,
        /// The store the sandbox lives in.
        #[arg(long)]
        store: PathBuf,
    },
}

/// Whether `run` starts the relay process (tests drive the relay in-process).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    Spawn,
    // Only the Linux-only end-to-end tests launch sandboxes.
    #[cfg(all(test, target_os = "linux"))]
    Skip,
}

/// Log file of a sandbox's relay process.
const RELAY_LOG: &str = "relay.log";

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

/// Run a `sandbox` subcommand against this box's store and VMM.
///
/// # Errors
///
/// Returns the subcommand's error, phrased for the person at the terminal.
pub(crate) async fn run(command: SandboxCommand) -> Result<()> {
    let store = Store::from_env();
    let fc = Firecracker::from_env();
    match command {
        SandboxCommand::Run(args) => start(&store, &fc, args, RelayMode::Spawn).await,
        SandboxCommand::Ls => {
            print!("{}", render_list(&store, now_unix()).await?);
            Ok(())
        }
        SandboxCommand::Pause { name } => set_paused(&store, &name, true).await,
        SandboxCommand::Resume { name } => set_paused(&store, &name, false).await,
        SandboxCommand::Logs { name, follow } => logs(&store, &name, follow).await,
        SandboxCommand::Diff { name, stat } => diff(&store, &name, stat).await,
        SandboxCommand::Stop { name } => stop(&store, &name).await,
        SandboxCommand::Relay { name, store: root } => relay::run(&Store::new(root), &name).await,
    }
}

/// How long `run` waits before checking the VMM survived boot.
const BOOT_CHECK: std::time::Duration = std::time::Duration::from_millis(750);

/// The last `n` lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    lines.into_iter().skip(skip).collect::<Vec<_>>().join("\n")
}

/// `sandbox run`.
async fn start(
    store: &Store,
    fc: &Firecracker,
    args: RunArgs,
    relay_mode: RelayMode,
) -> Result<()> {
    fc.preflight()?;
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
    relay::check_socket_path(&store.dir(&name)?)?;
    let mut meta = SandboxMeta {
        branch: format!("agent/{name}"),
        name,
        harness: args.harness,
        prompt: args.prompt.join(" "),
        repo,
        base_commit: base,
        created_unix: now_unix(),
        vcpus: args.vcpus,
        memory_mib: args.memory_mib,
        pid: None,
        collected: false,
        relay_pid: None,
        worktree: None,
    };
    let dir = store.create(&meta)?;

    let launched = launch(store, fc, &dir, &meta, args.disk_gib, relay_mode).await;
    let (pid, relay_pid) = match launched {
        Ok(pids) => pids,
        Err(e) => {
            // Free the name; a sandbox that never started has nothing to keep.
            std::fs::remove_dir_all(&dir).ok();
            return Err(e);
        }
    };
    meta.pid = Some(pid);
    meta.relay_pid = relay_pid;
    store.save(&meta)?;

    // A VMM that rejects its config or the device exits at once; say so here
    // rather than printing "started" for a sandbox that never booted.
    tokio::time::sleep(BOOT_CHECK).await;
    if !Firecracker::is_running(&dir, &meta) {
        let console = std::fs::read_to_string(dir.join(CONSOLE_LOG)).unwrap_or_default();
        anyhow::bail!(
            "sandbox {} exited while booting; console output:\n{}",
            meta.name,
            tail_lines(&console, 20)
        );
    }

    println!(
        "started sandbox {name} ({harness}) on branch {branch}\n\
         \n  devbox-agent sandbox logs {name} -f   follow its console\
         \n  devbox-agent sandbox stop {name}      stop it and collect its work\
         \n  devbox-agent sandbox diff {name}      review the changes",
        name = meta.name,
        harness = meta.harness,
        branch = meta.branch,
    );
    Ok(())
}

/// Stage the clone and task, start the relay, then boot the guest. Returns the
/// VMM and relay pids. Nothing is left running if it fails.
async fn launch(
    store: &Store,
    fc: &Firecracker,
    dir: &Path,
    meta: &SandboxMeta,
    disk_gib: u32,
    relay_mode: RelayMode,
) -> Result<(u32, Option<u32>)> {
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
    relay::init_inbox(dir).await?;

    // The relay listens before the guest boots, so its first connection lands.
    let relay_pid = match relay_mode {
        RelayMode::Spawn => Some(spawn_relay(store, dir, &meta.name).await?),
        #[cfg(all(test, target_os = "linux"))]
        RelayMode::Skip => None,
    };
    let pid = match fc.launch(dir, meta, &stage, disk_gib).await {
        Ok(pid) => pid,
        Err(e) => {
            if let Some(relay_pid) = relay_pid {
                let needle = relay::needle(store.root(), &meta.name);
                process::terminate(relay_pid, &needle, RELAY_GRACE)
                    .await
                    .ok();
            }
            return Err(e);
        }
    };
    std::fs::remove_dir_all(&stage).ok();
    Ok((pid, relay_pid))
}

/// How long a relay gets to exit on SIGTERM.
const RELAY_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Start `devbox-agent sandbox relay <name>` detached and wait until it listens.
async fn spawn_relay(store: &Store, dir: &Path, name: &str) -> Result<u32> {
    let exe = std::env::current_exe().context("locate the devbox-agent binary")?;
    let log_path = dir.join(RELAY_LOG);
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;
    let log_err = log.try_clone().context("duplicate relay log handle")?;
    // Its own process group, like the VMM, so it outlives this command.
    let child = std::process::Command::new(exe)
        .args(["sandbox", "relay", name, "--store"])
        .arg(store.root())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .context("start the sandbox relay")?;
    let pid = child.id();

    let ready = relay::socket_path(dir, relay::GIT_PORT);
    for _ in 0..50 {
        if ready.exists() {
            return Ok(pid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    process::terminate(pid, &relay::needle(store.root(), name), RELAY_GRACE)
        .await
        .ok();
    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
    anyhow::bail!(
        "the sandbox relay did not start; its output:\n{}",
        tail_lines(&output, 20)
    )
}

/// `sandbox stop`.
async fn stop(store: &Store, name: &str) -> Result<()> {
    let meta = store.load(name)?;
    let dir = store.dir(name)?;
    match Firecracker::stop(&dir, &meta).await? {
        StopOutcome::Clean => println!("{name} powered off cleanly"),
        StopOutcome::Forced => {
            println!("{name} didn't power off when asked; stopped it by signal");
        }
        StopOutcome::NotRunning => {}
    }
    if let Some(relay_pid) = meta.relay_pid {
        // The relay also exits by itself once the VMM is gone; this is prompter.
        if let Err(e) =
            process::terminate(relay_pid, &relay::needle(store.root(), name), RELAY_GRACE).await
        {
            eprintln!("warning: {e:#}");
        }
    }
    collect_and_report(store, meta).await
}

/// Fetch a stopped sandbox's branch home (once) and say how to use it.
async fn collect_and_report(store: &Store, mut meta: SandboxMeta) -> Result<()> {
    if meta.collected {
        println!(
            "sandbox {} was already collected onto {}",
            meta.name, meta.branch
        );
        return Ok(());
    }
    let dir = store.dir(&meta.name)?;
    let export = dir.join("export");
    std::fs::remove_dir_all(&export).ok();
    Firecracker::export_repo(&dir, &export).await?;
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

    Firecracker::discard_image(&dir)?;
    meta.collected = true;
    meta.pid = None;
    meta.relay_pid = None;
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

/// `sandbox diff`.
///
/// While the sandbox runs, shows what it has pushed to its inbox so far; once
/// it has stopped, collects (if needed) and shows the final branch.
async fn diff(store: &Store, name: &str, stat: bool) -> Result<()> {
    let meta = store.load(name)?;
    let dir = store.dir(name)?;
    let target = if !meta.collected && Firecracker::is_running(&dir, &meta) {
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
            collect_and_report(store, meta.clone()).await?;
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

/// `sandbox pause` / `sandbox resume`.
async fn set_paused(store: &Store, name: &str, paused: bool) -> Result<()> {
    let meta = store.load(name)?;
    let dir = store.dir(name)?;
    anyhow::ensure!(
        Firecracker::is_running(&dir, &meta),
        "sandbox {name} is not running"
    );
    api::set_paused(&dir, paused).await?;
    println!("{name} {}", if paused { "paused" } else { "resumed" });
    Ok(())
}

/// Whether a sandbox's VMM is up, and if so whether its guest is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Stopped,
    Running,
    Paused,
}

/// The liveness of the sandbox in `dir`.
async fn liveness(dir: &Path, meta: &SandboxMeta) -> Liveness {
    if !Firecracker::is_running(dir, meta) {
        return Liveness::Stopped;
    }
    match api::vm_state(dir).await {
        Ok(VmState::Paused) => Liveness::Paused,
        // Unreachable API or another state: the VMM is still up.
        _ => Liveness::Running,
    }
}

/// The `sandbox ls` table.
async fn render_list(store: &Store, now: u64) -> Result<String> {
    let metas = store.list()?;
    if metas.is_empty() {
        return Ok(
            "no sandboxes (start one with `devbox-agent sandbox run <harness> <task>`)\n"
                .to_owned(),
        );
    }
    let mut out = format!(
        "{:<20} {:<9} {:<8} {:<6} {}\n",
        "NAME", "STATE", "HARNESS", "AGE", "REPO"
    );
    for meta in metas {
        let dir = store.dir(&meta.name)?;
        let state = state_label(&meta, liveness(&dir, &meta).await);
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

/// A sandbox's lifecycle state, from its record and its VMM's liveness.
fn state_label(meta: &SandboxMeta, live: Liveness) -> &'static str {
    match live {
        Liveness::Running => "running",
        Liveness::Paused => "paused",
        Liveness::Stopped if meta.collected => "collected",
        // Stopped on its own (or crashed) with work not yet fetched.
        Liveness::Stopped => "exited",
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
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    use super::firecracker::Firecracker;
    #[cfg(target_os = "linux")]
    use super::store::now_unix;
    use super::store::{SandboxMeta, Store};
    use super::test_support::temp_root;
    #[cfg(target_os = "linux")]
    use super::test_support::{git, init_repo};
    use super::{Liveness, age, is_valid_harness, render_list, state_label, tail_lines};
    #[cfg(target_os = "linux")]
    use super::{RelayMode, RunArgs, diff, set_paused, start, stop};

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
            pid: None,
            collected,
            relay_pid: None,
            worktree: None,
        }
    }

    #[tokio::test]
    async fn ls_shows_state_age_and_repo() {
        let store = Store::new(temp_root("ls"));
        store.create(&meta("calm-quilt", 100, false)).unwrap();
        store.create(&meta("bold-acorn", 50, true)).unwrap();

        let out = render_list(&store, 400).await.unwrap();
        assert_eq!(out.lines().count(), 3, "{out}");
        let mut lines = out.lines();
        assert!(lines.next().unwrap().starts_with("NAME"), "{out}");
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("bold-acorn") && first.contains("collected") && first.contains("5m"),
            "{out}"
        );
        let second = lines.next().unwrap();
        assert!(
            second.contains("exited") && second.contains("/workspace/devbox"),
            "{out}"
        );
    }

    #[cfg(target_os = "linux")]
    /// A stand-in VMM: writes an untracked file into the guest's workspace image
    /// (as a sub-agent would), then waits to be stopped.
    const FAKE_VMM: &str = r#"#!/usr/bin/env python3
import json, os, socket, socketserver, subprocess, sys, threading
from http.server import BaseHTTPRequestHandler

args = sys.argv[1:]
api = args[args.index("--api-sock") + 1]
d = os.path.dirname(args[args.index("--config-file") + 1])
with open(f"{d}/guest.txt", "w") as f:
    f.write("from the guest\n")
subprocess.run(["debugfs", "-w", "-R", f"write {d}/guest.txt /repo/guest.txt",
                f"{d}/workspace.img"], capture_output=True)
state = {"s": "Running"}

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
    def do_GET(self):
        self.reply(200, json.dumps({"id": "x", "state": state["s"]}).encode())
    def do_PATCH(self):
        state["s"] = "Paused" if "Paused" in self.body() else "Running"
        self.reply(204)
    def do_PUT(self):
        self.body()
        self.reply(400, b'{"fault_message":"not supported"}')

class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

threading.Thread(target=Server(api, Api).serve_forever, daemon=True).start()
control = socket.socket(socket.AF_UNIX)
control.bind(f"{d}/vsock.sock")
control.listen()
while True:
    conn, _ = control.accept()
    stream = conn.makefile("rwb")
    # A paused guest can't answer its control port.
    if stream.readline().strip() != b"CONNECT 1024" or state["s"] == "Paused":
        conn.close()
        continue
    stream.write(b"OK 1073741824\n")
    stream.flush()
    if stream.readline().strip() == b"shutdown":
        with open(f"{d}/clean-shutdown", "w") as f:
            f.write("yes\n")
        os._exit(0)
"#;

    // Needs /proc, e2fsprogs and GNU `timeout`: Linux only, like the backend.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_then_stop_collects_guest_work_onto_the_agent_branch() {
        let root = temp_root("e2e");
        for file in ["nitro_enclaves", "vmlinux", "rootfs.ext4"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let vmm = root.join("firecracker");
        std::fs::write(&vmm, FAKE_VMM).unwrap();
        std::fs::set_permissions(&vmm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fc = Firecracker::rooted_at(&root);
        let store = Store::new(root.join("store"));
        let repo = init_repo(&root.join("repo"));

        let args = RunArgs {
            harness: "claude".to_owned(),
            prompt: vec!["add".to_owned(), "a".to_owned(), "test".to_owned()],
            repo: Some(repo.clone()),
            name: Some("calm-quilt".to_owned()),
            vcpus: 2,
            memory_mib: 4096,
            disk_gib: 1,
        };
        start(&store, &fc, args, RelayMode::Skip).await.unwrap();

        let meta = store.load("calm-quilt").unwrap();
        assert_eq!(meta.prompt, "add a test");
        let dir = store.dir("calm-quilt").unwrap();
        // The VMM is up once it has written into the image.
        for _ in 0..50 {
            if dir.join("guest.txt").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(Firecracker::is_running(&dir, &meta));
        assert!(
            render_list(&store, now_unix())
                .await
                .unwrap()
                .contains("running")
        );
        set_paused(&store, "calm-quilt", true).await.unwrap();
        assert!(
            render_list(&store, now_unix())
                .await
                .unwrap()
                .contains("paused")
        );
        assert!(!dir.join("stage").exists(), "staging copy must be removed");

        // Nothing pushed yet: a live diff says so.
        let err = diff(&store, "calm-quilt", true).await.unwrap_err();
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
        diff(&store, "calm-quilt", true).await.unwrap();
        let live = git(
            &repo,
            &["ls-tree", "--name-only", "refs/sandbox/calm-quilt/live"],
        );
        assert!(live.contains("progress.txt"), "{live}");
        assert!(Firecracker::is_running(
            &dir,
            &store.load("calm-quilt").unwrap()
        ));

        // Stopping a paused guest resumes it, then asks it to power off.
        stop(&store, "calm-quilt").await.unwrap();
        assert!(
            dir.join("clean-shutdown").exists(),
            "guest was not asked to shut down"
        );

        let meta = store.load("calm-quilt").unwrap();
        assert!(meta.collected && meta.pid.is_none());
        assert!(
            !dir.join("workspace.img").exists(),
            "image is discarded once collected"
        );
        let files = git(&repo, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(files.contains("guest.txt"), "{files}");
        let tree = meta.worktree.clone().unwrap();
        assert_eq!(tree, dir.join("worktree"));
        assert_eq!(
            std::fs::read_to_string(tree.join("guest.txt")).unwrap(),
            "from the guest\n"
        );
        assert!(
            !repo.join("guest.txt").exists(),
            "claimant checkout untouched"
        );
        // Stopping again is harmless.
        stop(&store, "calm-quilt").await.unwrap();
    }

    #[tokio::test]
    async fn ls_with_no_sandboxes_says_how_to_start_one() {
        let store = Store::new(temp_root("ls-empty"));
        assert!(
            render_list(&store, 0)
                .await
                .unwrap()
                .contains("sandbox run")
        );
    }

    #[test]
    fn state_prefers_running_then_collected() {
        assert_eq!(
            state_label(&meta("a", 0, true), Liveness::Running),
            "running"
        );
        assert_eq!(state_label(&meta("a", 0, true), Liveness::Paused), "paused");
        assert_eq!(
            state_label(&meta("a", 0, true), Liveness::Stopped),
            "collected"
        );
        assert_eq!(
            state_label(&meta("a", 0, false), Liveness::Stopped),
            "exited"
        );
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
        assert_eq!(tail_lines("", 5), "");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_reports_a_vmm_that_dies_during_boot() {
        let root = temp_root("boot-fail");
        for file in ["nitro_enclaves", "vmlinux", "rootfs.ext4"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let vmm = root.join("firecracker");
        std::fs::write(
            &vmm,
            "#!/bin/sh\necho 'enclave device rejected the VM' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&vmm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::new(root.join("store"));
        let repo = init_repo(&root.join("repo"));
        let args = RunArgs {
            harness: "claude".to_owned(),
            prompt: vec!["x".to_owned()],
            repo: Some(repo),
            name: Some("calm-quilt".to_owned()),
            vcpus: 1,
            memory_mib: 512,
            disk_gib: 1,
        };
        let err = start(
            &store,
            &Firecracker::rooted_at(&root),
            args,
            RelayMode::Skip,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exited while booting") && msg.contains("rejected the VM"),
            "{msg}"
        );
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
