//! `devbox-agent guest`: the init that runs inside a sandbox guest.
//!
//! The guest image starts it as a systemd service (`devbox-guest.service`). It
//! implements the guest's half of the contract the host side already speaks:
//!
//! 1. mount the workspace drive (`devbox.workspace=LABEL=…` on the kernel
//!    command line) at `/workspace`, which holds `repo/` and `task.json`;
//! 2. listen on loopback for the vsock ports in `task.json`: the egress proxy
//!    (`HTTPS_PROXY=http://127.0.0.1:3128`) and model APIs
//!    (`ANTHROPIC_BASE_URL=http://127.0.0.1:8080/anthropic`, …), each connection
//!    carried to the host relay over vsock;
//! 3. point the repo's `devbox` remote at the host inbox over vsock (git's
//!    `ext::` transport, `push.negotiate` off) and push the agent branch as it
//!    moves, so the claimant's live `diff` sees progress;
//! 4. run the harness against the task in a tmux session (so `attach` can join
//!    it), with placeholder credentials only;
//! 5. listen on the control port: `shutdown` stops the harness, `attach R C`
//!    attaches a terminal to it;
//! 6. when the harness exits or `shutdown` arrives: commit what is left as a WIP
//!    commit, push a last time, and power off (which ends the VMM).
//!
//! Linux only, like AF_VSOCK itself (`mod.rs` compiles this module on Linux).
//! vsock is spoken natively through `tokio-vsock`; git reaches the inbox through
//! `devbox-agent guest vsock-pipe <port>` as its `ext::` transport. For
//! tests, the vsock side can be replaced by the host relay's Unix sockets, and
//! the tmux session by a plain child process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::net::{TcpListener, TcpStream};
#[cfg(all(test, target_os = "linux"))]
use tokio::net::{UnixListener, UnixStream};
#[cfg(all(test, target_os = "linux"))]
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Where the workspace drive is mounted.
const WORKSPACE: &str = "/workspace";

/// The tmux session the harness runs in.
const SESSION: &str = "agent";

/// Per-harness launch scripts the image may provide (`<name> <prompt>`).
const HARNESS_DIR: &str = "/etc/devbox/harnesses";

/// The git remote that reaches the host inbox.
const REMOTE: &str = "devbox";

/// How often the branch is pushed if it moved.
const PUSH_INTERVAL: Duration = Duration::from_secs(30);

/// How often the harness is checked for having exited.
const SESSION_POLL: Duration = Duration::from_millis(500);

/// Time budget for each git step.
const GIT_TIMEOUT: Duration = Duration::from_mins(5);

/// What the guest needs from `task.json` (written by `sandbox run`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Task {
    pub(crate) name: String,
    pub(crate) harness: String,
    pub(crate) prompt: String,
    pub(crate) branch: String,
    pub(crate) vsock: Ports,
}

/// Host vsock ports (also the guest's loopback ports).
#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct Ports {
    pub(crate) egress_port: u32,
    pub(crate) model_port: u32,
    pub(crate) git_port: u32,
    pub(crate) control_port: u32,
}

/// How the guest reaches the host relay, and how the host reaches the guest's
/// control port.
#[derive(Debug, Clone)]
pub(crate) enum Upstream {
    /// Real vsock (`tokio-vsock`): host CID for outgoing connections, and a
    /// vsock listener for the control port.
    Vsock,
    /// Tests: the relay's Unix sockets in `relay_dir` (`vsock.sock_<port>`), and
    /// a Unix control socket.
    #[cfg(all(test, target_os = "linux"))]
    Unix {
        relay_dir: PathBuf,
        control_socket: PathBuf,
    },
}

/// How the harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Session {
    /// In a detached tmux session, so `attach` can join it.
    Tmux,
    /// As a plain child process (tests; no `attach`).
    #[cfg(all(test, target_os = "linux"))]
    Direct,
}

/// The guest init's configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) workspace: PathBuf,
    /// Mount the workspace drive first.
    pub(crate) mount: bool,
    pub(crate) upstream: Upstream,
    pub(crate) session: Session,
    /// Loopback addresses for the egress and model ports (`None`: the task's
    /// port numbers on 127.0.0.1).
    pub(crate) listen: Option<(SocketAddr, SocketAddr)>,
    /// Git remote URL for the inbox (`None`: vsock via
    /// `ext::devbox-agent guest vsock-pipe <port>`).
    pub(crate) remote_url: Option<String>,
    pub(crate) push_interval: Duration,
    /// Command run last to power off (empty: just exit).
    pub(crate) poweroff: Vec<String>,
    /// Where per-harness launch scripts live.
    pub(crate) harness_dir: PathBuf,
}

impl Config {
    /// The configuration inside a real sandbox guest.
    pub(crate) fn guest() -> Self {
        Self {
            workspace: PathBuf::from(WORKSPACE),
            mount: true,
            upstream: Upstream::Vsock,
            session: Session::Tmux,
            listen: None,
            remote_url: None,
            push_interval: PUSH_INTERVAL,
            poweroff: vec!["systemctl".to_owned(), "poweroff".to_owned()],
            harness_dir: PathBuf::from(HARNESS_DIR),
        }
    }
}

/// Why the guest is finishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The harness exited on its own.
    HarnessExited,
    /// The host asked for a shutdown.
    Shutdown,
}

/// `devbox-agent guest`: run the sandbox until the harness exits or the host
/// asks it to stop, then hand the work back and power off.
///
/// # Errors
///
/// Returns an error if the workspace, task or harness can't be set up.
pub(crate) async fn run(config: Config) -> Result<Ending> {
    if config.mount {
        mount_workspace(&config.workspace).await?;
    }
    let task = load_task(&config.workspace.join("task.json"))?;
    let repo = config.workspace.join("repo");
    let remote = match config.remote_url.clone() {
        Some(url) => url,
        None => {
            let agent = std::env::current_exe().context("locate devbox-agent")?;
            format!(
                "ext::{} guest vsock-pipe {}",
                agent.display(),
                task.vsock.git_port
            )
        }
    };
    configure_repo(&repo, &task, &remote).await?;

    let (egress_addr, model_addr) = config.listen.unwrap_or_else(|| {
        (
            loopback(task.vsock.egress_port),
            loopback(task.vsock.model_port),
        )
    });
    let egress = TcpListener::bind(egress_addr)
        .await
        .context("listen for egress")?;
    let model = TcpListener::bind(model_addr)
        .await
        .context("listen for model APIs")?;
    let env = harness_env(&task, egress.local_addr()?, model.local_addr()?);
    let mut bridges = tokio::task::JoinSet::new();
    bridges.spawn(bridge_loop(
        egress,
        config.upstream.clone(),
        task.vsock.egress_port,
    ));
    bridges.spawn(bridge_loop(
        model,
        config.upstream.clone(),
        task.vsock.model_port,
    ));

    let (control_tx, mut control_rx) = mpsc::channel(4);
    match &config.upstream {
        Upstream::Vsock => {
            let listener = vsock::listen(task.vsock.control_port)?;
            bridges.spawn(vsock::control_loop(listener, control_tx, config.session));
        }
        #[cfg(all(test, target_os = "linux"))]
        Upstream::Unix { control_socket, .. } => {
            let listener = bind_control(control_socket)?;
            bridges.spawn(unix_control_loop(listener, control_tx, config.session));
        }
    }

    let command = harness_command_in(&config.harness_dir, &task.harness, &task.prompt)?;
    let mut harness = Harness::start(config.session, &command, &repo, &env).await?;

    let mut pushed = None;
    let mut push_tick = tokio::time::interval(config.push_interval);
    let mut poll = tokio::time::interval(SESSION_POLL);
    let ending = loop {
        tokio::select! {
            _ = push_tick.tick() => {
                pushed = push_if_moved(&repo, &task.branch, pushed).await;
            }
            _ = poll.tick() => {
                if !harness.running().await {
                    break Ending::HarnessExited;
                }
            }
            Some(()) = control_rx.recv() => break Ending::Shutdown,
        }
    };

    if ending == Ending::Shutdown {
        harness.stop().await;
    }
    finish(&repo, &task).await;
    bridges.abort_all();
    if let Some((program, args)) = config.poweroff.split_first() {
        let status = Command::new(program).args(args).status().await;
        if !status.is_ok_and(|s| s.success()) {
            tracing::warn!("power-off command failed");
        }
    }
    Ok(ending)
}

/// Mount the drive named on the kernel command line at `workspace`.
async fn mount_workspace(workspace: &Path) -> Result<()> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").context("read /proc/cmdline")?;
    let spec = cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("devbox.workspace="))
        .context("no devbox.workspace= on the kernel command line")?;
    std::fs::create_dir_all(workspace).context("create the workspace mount point")?;
    let status = Command::new("mount")
        .arg(spec)
        .arg(workspace)
        .status()
        .await
        .context("run mount")?;
    anyhow::ensure!(
        status.success(),
        "mount {spec} {} failed",
        workspace.display()
    );
    Ok(())
}

/// Read and parse `task.json`.
///
/// # Errors
///
/// Returns an error if the file is missing or malformed.
pub(crate) fn load_task(path: &Path) -> Result<Task> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Give the repo an identity, and the `devbox` remote that reaches the inbox.
async fn configure_repo(repo: &Path, task: &Task, remote: &str) -> Result<()> {
    let email = format!("sandbox+{}@devbox.invalid", task.name);
    let name = format!("devbox sandbox {}", task.name);
    for (key, value) in [
        ("user.name", name.as_str()),
        ("user.email", email.as_str()),
        ("commit.gpgsign", "false"),
        ("protocol.ext.allow", "always"),
        // Negotiation opens a fetch connection, which the inbox port refuses.
        ("push.negotiate", "false"),
    ] {
        git(repo, &["config", key, value]).await?;
    }
    // A fresh remote each boot, so a stale URL from the image is never used.
    let _ignored = git(repo, &["remote", "remove", REMOTE]).await;
    git(repo, &["remote", "add", REMOTE, remote]).await
}

fn loopback(port: u32) -> SocketAddr {
    let port = u16::try_from(port).unwrap_or(0);
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// The environment the harness runs with. Credentials are placeholders: the
/// host relay drops them and the control plane injects the real ones.
fn harness_env(task: &Task, egress: SocketAddr, model: SocketAddr) -> BTreeMap<String, String> {
    let proxy = format!("http://{egress}");
    let model = format!("http://{model}");
    BTreeMap::from([
        ("HTTPS_PROXY".to_owned(), proxy.clone()),
        ("HTTP_PROXY".to_owned(), proxy.clone()),
        ("https_proxy".to_owned(), proxy.clone()),
        ("http_proxy".to_owned(), proxy),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
        ("no_proxy".to_owned(), "127.0.0.1,localhost".to_owned()),
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            format!("{model}/anthropic"),
        ),
        (
            "ANTHROPIC_API_KEY".to_owned(),
            "devbox-relay-placeholder".to_owned(),
        ),
        ("OPENAI_BASE_URL".to_owned(), format!("{model}/openai/v1")),
        (
            "OPENAI_API_KEY".to_owned(),
            "devbox-relay-placeholder".to_owned(),
        ),
        ("DEVBOX_SANDBOX".to_owned(), task.name.clone()),
    ])
}

/// The command for `harness` running `prompt`. A launch script in `dir`
/// (`/etc/devbox/harnesses/<harness>` in the image) wins over the built-in ones.
///
/// # Errors
///
/// Returns an error for an unknown harness.
fn harness_command_in(dir: &Path, harness: &str, prompt: &str) -> Result<Vec<String>> {
    let script = dir.join(harness);
    if script.is_file() {
        return Ok(vec![
            script.to_string_lossy().into_owned(),
            prompt.to_owned(),
        ]);
    }
    let owned = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    match harness {
        // The sandbox is the boundary, so the harness runs without its own
        // permission prompts.
        "claude" => Ok(owned(&["claude", "--dangerously-skip-permissions", prompt])),
        "codex" => Ok(owned(&[
            "codex",
            "--dangerously-bypass-approvals-and-sandbox",
            prompt,
        ])),
        "shell" => Ok(owned(&["bash", "-l"])),
        other => anyhow::bail!(
            "unknown harness {other:?}; built in: claude, codex, shell \
             (or add {})",
            dir.join(other).display()
        ),
    }
}

/// The running harness.
enum Harness {
    Tmux,
    #[cfg(all(test, target_os = "linux"))]
    Direct(Child),
}

impl Harness {
    async fn start(
        session: Session,
        command: &[String],
        repo: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        match session {
            Session::Tmux => {
                let mut tmux = Command::new("tmux");
                tmux.args([
                    "new-session",
                    "-d",
                    "-s",
                    SESSION,
                    "-x",
                    "200",
                    "-y",
                    "50",
                    "-c",
                ])
                .arg(repo);
                for (key, value) in env {
                    tmux.arg("-e").arg(format!("{key}={value}"));
                }
                let status = tmux
                    .arg("--")
                    .args(command)
                    .status()
                    .await
                    .context("start tmux")?;
                anyhow::ensure!(status.success(), "tmux new-session failed");
                Ok(Self::Tmux)
            }
            #[cfg(all(test, target_os = "linux"))]
            Session::Direct => {
                let (program, args) = command.split_first().context("empty harness command")?;
                let child = Command::new(program)
                    .args(args)
                    .current_dir(repo)
                    .envs(env)
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("start {program}"))?;
                Ok(Self::Direct(child))
            }
        }
    }

    async fn running(&mut self) -> bool {
        match self {
            Self::Tmux => Command::new("tmux")
                .args(["has-session", "-t", SESSION])
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success()),
            #[cfg(all(test, target_os = "linux"))]
            Self::Direct(child) => matches!(child.try_wait(), Ok(None)),
        }
    }

    async fn stop(&mut self) {
        match self {
            Self::Tmux => {
                let _status = Command::new("tmux")
                    .args(["kill-session", "-t", SESSION])
                    .status()
                    .await;
            }
            #[cfg(all(test, target_os = "linux"))]
            Self::Direct(child) => {
                let _killed = child.kill().await;
            }
        }
    }
}

/// Accept on a loopback listener and carry each connection to host port `port`.
async fn bridge_loop(listener: TcpListener, upstream: Upstream, port: u32) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let upstream = upstream.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge(conn, &upstream, port).await {
                tracing::warn!(port, error = %format!("{e:#}"), "bridge failed");
            }
        });
    }
}

/// Carry one loopback connection to host port `port`.
async fn bridge(mut conn: TcpStream, upstream: &Upstream, port: u32) -> Result<()> {
    match upstream {
        Upstream::Vsock => {
            let mut host = vsock::connect_host(port).await?;
            tokio::io::copy_bidirectional(&mut conn, &mut host)
                .await
                .context("relay bytes")?;
            Ok(())
        }
        #[cfg(all(test, target_os = "linux"))]
        Upstream::Unix { relay_dir, .. } => {
            let mut host = UnixStream::connect(super::relay::socket_path(relay_dir, port))
                .await
                .context("connect to the relay")?;
            tokio::io::copy_bidirectional(&mut conn, &mut host)
                .await
                .context("relay bytes")?;
            Ok(())
        }
    }
}

/// Bind the Unix control socket (tests stand it in for the vsock control port).
#[cfg(all(test, target_os = "linux"))]
fn bind_control(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::remove_file(path).ok();
    UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))
}

/// Serve control requests on a Unix socket (tests).
#[cfg(all(test, target_os = "linux"))]
async fn unix_control_loop(listener: UnixListener, shutdown: mpsc::Sender<()>, session: Session) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        spawn_control(conn, shutdown.clone(), session);
    }
}

/// Handle one control connection on its own task.
fn spawn_control<S>(conn: S, shutdown: mpsc::Sender<()>, session: Session)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = control(conn, &shutdown, session).await {
            tracing::warn!(error = %format!("{e:#}"), "control request failed");
        }
    });
}

/// A parsed control request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ControlRequest {
    Shutdown,
    Attach { rows: u16, cols: u16 },
}

/// Parse one control line.
pub(crate) fn parse_control(line: &str) -> Option<ControlRequest> {
    let mut words = line.split_whitespace();
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some("shutdown"), None, None, None) => Some(ControlRequest::Shutdown),
        (Some("attach"), Some(rows), Some(cols), None) => Some(ControlRequest::Attach {
            rows: rows.parse().ok().filter(|r| *r > 0)?,
            cols: cols.parse().ok().filter(|c| *c > 0)?,
        }),
        _ => None,
    }
}

/// Serve one control request: `shutdown` ends the run; `attach R C` attaches
/// a terminal to the harness's tmux session.
async fn control<S>(conn: S, shutdown: &mpsc::Sender<()>, session: Session) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read control request")?;
    match parse_control(&line) {
        Some(ControlRequest::Shutdown) => {
            shutdown.send(()).await.ok();
            Ok(())
        }
        Some(ControlRequest::Attach { rows, cols }) => {
            anyhow::ensure!(
                session == Session::Tmux,
                "this guest has no attachable session"
            );
            attach(reader.into_inner(), rows, cols).await
        }
        None => anyhow::bail!("unknown control request {:?}", line.trim()),
    }
}

/// The command that attaches a terminal of `rows`x`cols` to the harness
/// session. `script` gives tmux the pty it needs.
pub(crate) fn attach_command(rows: u16, cols: u16) -> Vec<String> {
    vec![
        "script".to_owned(),
        "-q".to_owned(),
        "-e".to_owned(),
        "-c".to_owned(),
        format!("stty rows {rows} cols {cols}; exec tmux attach -t {SESSION}"),
        "/dev/null".to_owned(),
    ]
}

async fn attach<S>(conn: S, rows: u16, cols: u16) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let command = attach_command(rows, cols);
    let (program, args) = command.split_first().context("empty attach command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start the attach terminal")?;
    let mut stdin = child.stdin.take().context("attach stdin")?;
    let mut stdout = child.stdout.take().context("attach stdout")?;
    let (mut read, mut write) = tokio::io::split(conn);
    let input = tokio::spawn(async move { tokio::io::copy(&mut read, &mut stdin).await });
    tokio::io::copy(&mut stdout, &mut write).await.ok();
    input.abort();
    child.wait().await.context("wait for the attach terminal")?;
    Ok(())
}

/// `devbox-agent guest vsock-pipe <port>`: connect stdin and stdout to host
/// vsock port `port`. git runs it as the `ext::` transport for the inbox remote.
///
/// # Errors
///
/// Returns an error if the host port can't be reached.
pub(crate) async fn vsock_pipe(port: u32) -> Result<()> {
    vsock::pipe_stdio(port).await
}

/// The vsock side, through `tokio-vsock`.
mod vsock {
    use anyhow::{Context, Result};
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use tokio_vsock::{VMADDR_CID_ANY, VMADDR_CID_HOST, VsockAddr, VsockListener, VsockStream};

    use super::{Session, spawn_control};

    /// Connect to host vsock port `port`.
    pub(super) async fn connect_host(port: u32) -> Result<VsockStream> {
        VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, port))
            .await
            .with_context(|| format!("connect to host vsock port {port}"))
    }

    /// Listen on guest vsock port `port`.
    pub(super) fn listen(port: u32) -> Result<VsockListener> {
        VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
            .with_context(|| format!("listen on vsock port {port}"))
    }

    /// Serve control requests arriving on the vsock control port.
    pub(super) async fn control_loop(
        listener: VsockListener,
        shutdown: mpsc::Sender<()>,
        session: Session,
    ) {
        loop {
            match listener.accept().await {
                Ok((conn, _)) => spawn_control(conn, shutdown.clone(), session),
                Err(e) => {
                    tracing::warn!(error = %e, "control accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Pipe stdin/stdout to host vsock port `port` until both sides finish.
    pub(super) async fn pipe_stdio(port: u32) -> Result<()> {
        let host = connect_host(port).await?;
        let (mut from_host, mut to_host) = host.into_split();
        let up = async {
            tokio::io::copy(&mut tokio::io::stdin(), &mut to_host).await?;
            // EOF from git: half-close so the host side sees the end of input.
            to_host.shutdown().await?;
            anyhow::Ok(())
        };
        let down = async {
            let mut stdout = tokio::io::stdout();
            tokio::io::copy(&mut from_host, &mut stdout).await?;
            stdout.flush().await?;
            anyhow::Ok(())
        };
        let (up, down) = tokio::join!(up, down);
        up.context("copy to host")?;
        down.context("copy from host")
    }
}

/// Push the agent branch to the inbox if HEAD moved since `last`. Returns the
/// commit now known to be pushed.
async fn push_if_moved(repo: &Path, branch: &str, last: Option<String>) -> Option<String> {
    let head = git_output(repo, &["rev-parse", "HEAD"]).await.ok()?;
    if last.as_deref() == Some(head.as_str()) {
        return last;
    }
    match push(repo, branch).await {
        Ok(()) => Some(head),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "push to the host inbox failed");
            last
        }
    }
}

async fn push(repo: &Path, branch: &str) -> Result<()> {
    git(
        repo,
        &[
            "push",
            "--quiet",
            "--no-verify",
            REMOTE,
            &format!("+HEAD:refs/heads/{branch}"),
        ],
    )
    .await
}

/// Commit what the harness left uncommitted, and push a last time.
async fn finish(repo: &Path, task: &Task) {
    let dirty = git_output(repo, &["status", "--porcelain", "--untracked-files=all"])
        .await
        .is_ok_and(|status| !status.is_empty());
    if dirty {
        let message = format!("WIP: uncommitted changes from sandbox {}", task.name);
        let committed = async {
            git(repo, &["add", "--all"]).await?;
            git(repo, &["commit", "--quiet", "--no-verify", "-m", &message]).await
        }
        .await;
        if let Err(e) = committed {
            tracing::warn!(error = %format!("{e:#}"), "could not commit leftover changes");
        }
    }
    if let Err(e) = push(repo, &task.branch).await {
        tracing::warn!(error = %format!("{e:#}"), "final push failed; the host collects from the disk");
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<()> {
    crate::git::run_git(repo, None, args, GIT_TIMEOUT).await
}

async fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    crate::git::git_output(repo, args, GIT_TIMEOUT).await
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::{ControlRequest, attach_command, harness_command_in, parse_control};
    use crate::sandbox::test_support::temp_root;

    #[cfg(target_os = "linux")]
    /// A git `ext::` transport standing in for `vsock-pipe`: stdio to a Unix socket.
    const BRIDGE: &str = r#"
import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
def up():
    while True:
        data = os.read(0, 65536)
        if not data:
            s.shutdown(socket.SHUT_WR)
            return
        s.sendall(data)
threading.Thread(target=up, daemon=True).start()
while True:
    data = s.recv(65536)
    if not data:
        break
    os.write(1, data)
"#;

    #[cfg(target_os = "linux")]
    /// A harness that records its environment, commits the task, then idles.
    const HARNESS: &str = r#"#!/bin/sh
env > ../harness.env
echo "$1" > task.txt
git add task.txt
git commit -qm "do: $1"
exec sleep 60
"#;

    #[cfg(target_os = "linux")]
    struct Guest {
        root: std::path::PathBuf,
        relay_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: tokio::task::JoinHandle<anyhow::Result<super::Ending>>,
    }

    /// A host relay (egress to an echoing proxy, git into an inbox) and a guest
    /// wired to it over the relay's Unix sockets, running `harness`.
    #[cfg(target_os = "linux")]
    async fn guest(label: &str, harness: &str) -> Guest {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use super::{Config, Session, Upstream};
        use crate::sandbox::jail::owner_of;
        use crate::sandbox::relay::{GIT_PORT, INBOX, Relay, Upstreams, init_inbox, socket_path};
        use crate::sandbox::test_support::{git, init_repo};

        let root = temp_root(label);
        let host = root.join("host");
        std::fs::create_dir_all(&host).unwrap();
        init_inbox(&host).await.unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = proxy.accept().await {
                let mut buf = [0u8; 64];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                conn.write_all(buf.get(..n).unwrap_or_default()).await.ok();
            }
        });
        let upstreams = Upstreams {
            egress: Some(proxy_addr),
            model: None,
        };
        let listening = Relay::new(
            host.clone(),
            host.join(INBOX),
            owner_of(&host).unwrap(),
            upstreams,
        )
        .listen()
        .unwrap();
        let relay_stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&relay_stop);
        tokio::spawn(async move { listening.run_until(|| flag.load(Ordering::Relaxed)).await });

        let workspace = root.join("ws");
        let repo = init_repo(&workspace.join("repo"));
        git(&repo, &["checkout", "--quiet", "-b", "agent/calm-quilt"]);
        std::fs::write(
            workspace.join("task.json"),
            r#"{"name":"calm-quilt","harness":"fake","prompt":"add a test","branch":"agent/calm-quilt",
               "base_commit":"x","vsock":{"egress_port":3128,"model_port":8080,"git_port":9418,"control_port":1024}}"#,
        )
        .unwrap();
        let harnesses = root.join("harnesses");
        std::fs::create_dir_all(&harnesses).unwrap();
        std::fs::write(harnesses.join("fake"), harness).unwrap();
        std::fs::set_permissions(
            harnesses.join("fake"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let bridge = root.join("bridge.py");
        std::fs::write(&bridge, BRIDGE).unwrap();

        let any = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let config = Config {
            workspace,
            mount: false,
            upstream: Upstream::Unix {
                relay_dir: host.clone(),
                control_socket: root.join("control.sock"),
            },
            session: Session::Direct,
            listen: Some((any, any)),
            remote_url: Some(format!(
                "ext::python3 {} {}",
                bridge.display(),
                socket_path(&host, GIT_PORT).display()
            )),
            push_interval: Duration::from_millis(200),
            poweroff: vec![
                "touch".to_owned(),
                root.join("powered-off").to_string_lossy().into_owned(),
            ],
            harness_dir: harnesses,
        };
        let handle = tokio::spawn(super::run(config));
        Guest {
            root,
            relay_stop,
            handle,
        }
    }

    /// Wait up to 10 s for `check` to hold.
    #[cfg(target_os = "linux")]
    async fn eventually(check: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if check() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        check()
    }

    #[cfg(target_os = "linux")]
    fn inbox_head(root: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(root.join("host/inbox.git"))
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/agent/calm-quilt",
            ])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn guest_pushes_progress_relays_egress_and_finishes_on_shutdown() {
        use std::sync::atomic::Ordering;

        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

        use crate::sandbox::test_support::git;

        let g = guest("guest-shutdown", HARNESS).await;
        // The harness's commit reaches the host inbox through the git port.
        assert!(
            eventually(|| inbox_head(&g.root).is_some()).await,
            "nothing pushed"
        );
        let inbox = g.root.join("host/inbox.git");
        let files = git(&inbox, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(files.contains("task.txt"), "{files}");

        // The harness saw loopback proxy and model URLs and placeholder keys.
        let env = std::fs::read_to_string(g.root.join("ws/harness.env")).unwrap();
        assert!(
            env.contains("ANTHROPIC_API_KEY=devbox-relay-placeholder"),
            "{env}"
        );
        assert!(
            env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:"),
            "{env}"
        );
        let proxy = env
            .lines()
            .find_map(|l| l.strip_prefix("HTTPS_PROXY=http://"))
            .unwrap()
            .to_owned();
        // Its egress port carries bytes through the host relay to the proxy.
        let mut conn = tokio::net::TcpStream::connect(&proxy).await.unwrap();
        conn.write_all(b"CONNECT x:443").await.unwrap();
        let mut echoed = vec![0u8; 13];
        conn.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"CONNECT x:443");

        // Something left uncommitted, then the host asks for a shutdown.
        std::fs::write(g.root.join("ws/repo/notes.txt"), "half done\n").unwrap();
        let control = tokio::net::UnixStream::connect(g.root.join("control.sock"))
            .await
            .unwrap();
        let mut control = tokio::io::BufReader::new(control);
        control.get_mut().write_all(b"shutdown\n").await.unwrap();
        let mut rest = String::new();
        control.read_line(&mut rest).await.ok();

        let ending = tokio::time::timeout(std::time::Duration::from_secs(20), g.handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ending, super::Ending::Shutdown);
        let files = git(&inbox, &["ls-tree", "--name-only", "agent/calm-quilt"]);
        assert!(files.contains("notes.txt"), "WIP not pushed: {files}");
        assert!(g.root.join("powered-off").exists(), "no power-off");
        g.relay_stop.store(true, Ordering::Relaxed);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn guest_finishes_when_the_harness_exits() {
        use std::sync::atomic::Ordering;

        use crate::sandbox::test_support::git;

        let harness = "#!/bin/sh\necho \"$1\" > done.txt\n";
        let g = guest("guest-exit", harness).await;
        let ending = tokio::time::timeout(std::time::Duration::from_secs(20), g.handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ending, super::Ending::HarnessExited);
        // Its uncommitted output became a WIP commit and was pushed.
        let files = git(
            &g.root.join("host/inbox.git"),
            &["ls-tree", "--name-only", "agent/calm-quilt"],
        );
        assert!(files.contains("done.txt"), "{files}");
        assert!(g.root.join("powered-off").exists());
        g.relay_stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn harnesses_map_to_no_prompt_commands_and_scripts_win() {
        let dir = temp_root("guest-harness");
        assert_eq!(
            harness_command_in(&dir, "claude", "fix it").unwrap(),
            ["claude", "--dangerously-skip-permissions", "fix it"]
        );
        assert_eq!(
            harness_command_in(&dir, "codex", "fix it").unwrap(),
            [
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "fix it"
            ]
        );
        let err = harness_command_in(&dir, "aider", "x").unwrap_err();
        assert!(format!("{err:#}").contains("unknown harness"), "{err:#}");
        std::fs::write(dir.join("aider"), "#!/bin/sh\n").unwrap();
        let script = dir.join("aider").to_string_lossy().into_owned();
        assert_eq!(
            harness_command_in(&dir, "aider", "x").unwrap(),
            [script.as_str(), "x"]
        );
    }

    #[test]
    fn control_lines_parse_strictly() {
        assert_eq!(parse_control("shutdown\n"), Some(ControlRequest::Shutdown));
        assert_eq!(
            parse_control("attach 40 120\n"),
            Some(ControlRequest::Attach {
                rows: 40,
                cols: 120
            })
        );
        for bad in [
            "",
            "shutdown now",
            "attach 0 80",
            "attach 40",
            "attach x y",
            "rm -rf /",
        ] {
            assert_eq!(parse_control(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn attach_runs_tmux_attach_in_a_sized_pty() {
        let command = attach_command(40, 120);
        assert_eq!(command.first().map(String::as_str), Some("script"));
        assert!(
            command.contains(&"stty rows 40 cols 120; exec tmux attach -t agent".to_owned()),
            "{command:?}"
        );
    }
}
