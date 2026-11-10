//! `devbox-agent sandboxd`: the root daemon that runs sandbox VMMs.
//!
//! Jailing a VMM needs root (chroot, a dedicated uid, device nodes), and a VMM
//! has to outlive the shell that started it. So the privileged, long-lived half
//! of a sandbox runs here, under systemd (`devbox-sandboxd.service`, baked into
//! the AMI). The `sandbox` CLI does everything it can as the claimant (cloning,
//! building the workspace image, collecting the work) and asks the daemon for
//! the rest over a Unix socket:
//!
//! | Request | The daemon |
//! |---------|------------|
//! | `preflight` | checks the host can run a jailed sandbox |
//! | `start` | builds the jail, starts the relay, boots the VMM under `jailer` |
//! | `stop` | stops the VMM (cleanly if the guest cooperates) and relay, hands the image back, removes the jail |
//! | `status` | reports which sandboxes are running |
//!
//! Requests are one JSON line each way. Only the claimant (the owner of
//! `/workspace`, `DEVBOX_SANDBOX_OWNER_PATH`) and root may call, checked with
//! the peer credentials of the connection. The daemon reads sandbox records
//! from the store but never takes a path from a request: a sandbox is named,
//! and the name is validated before any path is built from it.
//!
//! VMMs run in their own process groups and the unit uses `KillMode=process`,
//! so restarting the daemon leaves running sandboxes alone; on start it finds
//! them again (from each sandbox's `runtime.json`) and resumes their relays.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use devbox_common::env_non_empty;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::api;
use super::firecracker::{CONSOLE_LOG, StopOutcome, stop_vmm};
use super::jail::{self, Jail, Runtime, owner_of};
use super::process::wait_started;
use super::relay::{INBOX, Relay, Upstreams};
use super::store::Store;

/// Default daemon socket (systemd's `RuntimeDirectory=devbox`).
const DEFAULT_SOCKET: &str = "/run/devbox/sandboxd.sock";

/// Default path whose owner is the claimant.
const DEFAULT_OWNER_PATH: &str = "/workspace";

/// Largest request accepted.
const MAX_REQUEST: usize = 65_536;

/// How long `start` waits before checking the VMM survived boot.
const BOOT_CHECK: Duration = Duration::from_millis(750);

/// How long `start` waits for a spawned jailer to exec.
const EXEC_WAIT: Duration = Duration::from_secs(2);

/// How long the CLI waits for an answer (`start` may copy the guest image).
const CLIENT_TIMEOUT: Duration = Duration::from_mins(5);

/// A request from the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Preflight,
    Start { name: String },
    Stop { name: String },
    Status,
}

/// The daemon's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum Response {
    Ok,
    Stopped {
        outcome: StopOutcome,
    },
    /// Names of the sandboxes whose VMM is running.
    Status {
        running: BTreeSet<String>,
    },
    Error {
        message: String,
    },
}

/// The daemon's configuration.
pub(crate) struct Config {
    pub(crate) socket: PathBuf,
    pub(crate) store: Store,
    pub(crate) owner_path: PathBuf,
    pub(crate) jail: Jail,
    pub(crate) upstreams: Upstreams,
}

impl Config {
    /// Configuration from the environment (`DEVBOX_SANDBOXD_SOCKET`,
    /// `DEVBOX_SANDBOX_DIR`, `DEVBOX_SANDBOX_OWNER_PATH`, the jail's
    /// `DEVBOX_SANDBOX_*`, and the box's egress proxy and control plane).
    pub(crate) async fn from_env() -> Self {
        Self {
            socket: socket_path(),
            store: Store::from_env(),
            owner_path: env_non_empty("DEVBOX_SANDBOX_OWNER_PATH")
                .map_or_else(|| PathBuf::from(DEFAULT_OWNER_PATH), PathBuf::from),
            jail: Jail::from_env(),
            upstreams: Upstreams::from_env().await,
        }
    }
}

/// The daemon socket path (`DEVBOX_SANDBOXD_SOCKET`, or the default).
pub(crate) fn socket_path() -> PathBuf {
    env_non_empty("DEVBOX_SANDBOXD_SOCKET")
        .map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from)
}

/// Shared daemon state.
struct Daemon {
    config: Config,
    /// Stop flags of the running relays, by sandbox name.
    relays: tokio::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Serializes `start` and `stop`, which share uid allocation and the jail.
    lifecycle: tokio::sync::Mutex<()>,
}

/// Serve requests on the configured socket forever.
///
/// # Errors
///
/// Returns an error if the socket can't be bound.
pub(crate) async fn serve(config: Config) -> Result<()> {
    let listener = bind(&config.socket)?;
    let daemon = Arc::new(Daemon {
        config,
        relays: tokio::sync::Mutex::new(HashMap::new()),
        lifecycle: tokio::sync::Mutex::new(()),
    });
    daemon.recover().await;
    tracing::info!(socket = %daemon.config.socket.display(), "sandbox daemon listening");
    loop {
        let (conn, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            if let Err(e) = daemon.connection(conn).await {
                tracing::warn!(error = %format!("{e:#}"), "request failed");
            }
        });
    }
}

/// Bind the daemon socket, replacing a stale one. Anyone may connect; the peer
/// credential check decides who may act.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::remove_file(path).ok();
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("open up {}", path.display()))?;
    Ok(listener)
}

impl Daemon {
    /// Answer one request on `conn`.
    async fn connection(&self, mut conn: UnixStream) -> Result<()> {
        let peer = conn.peer_cred().context("read peer credentials")?.uid();
        let line = api::read_line_unbuffered(&mut conn, MAX_REQUEST)
            .await
            .context("read request")?;

        let request = match self.authorize(peer) {
            Err(e) => Err(e),
            Ok(()) => serde_json::from_str::<Request>(line.trim())
                .map_err(|e| anyhow::anyhow!("malformed request: {e}")),
        };
        let response = match request {
            Ok(request) => self.handle(request).await.unwrap_or_else(|e| error(&e)),
            Err(e) => error(&e),
        };
        reply(&mut conn, &response).await
    }

    /// Allow root and the claimant (the owner of the owner path) only.
    fn authorize(&self, peer: u32) -> Result<()> {
        let (owner, _) = owner_of(&self.config.owner_path)?;
        anyhow::ensure!(
            peer == 0 || peer == owner,
            "uid {peer} may not manage sandboxes on this box (only its claimant, uid {owner})"
        );
        Ok(())
    }

    async fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::Preflight => {
                self.config.jail.preflight()?;
                Ok(Response::Ok)
            }
            Request::Start { name } => {
                self.start(&name).await?;
                Ok(Response::Ok)
            }
            Request::Stop { name } => Ok(Response::Stopped {
                outcome: self.stop(&name).await?,
            }),
            Request::Status => Ok(Response::Status {
                running: self.running(),
            }),
        }
    }

    /// The claimant's uid and gid.
    fn owner(&self) -> Result<(u32, u32)> {
        owner_of(&self.config.owner_path)
    }

    /// The sandbox directory for `name`, which must belong to the claimant.
    fn sandbox_dir(&self, name: &str) -> Result<PathBuf> {
        let dir = self.config.store.dir(name)?;
        let meta = std::fs::symlink_metadata(&dir)
            .with_context(|| format!("no sandbox named {name:?}"))?;
        let (owner, _) = self.owner()?;
        anyhow::ensure!(
            meta.is_dir() && meta.uid() == owner,
            "sandbox directory {} is not the claimant's",
            dir.display()
        );
        Ok(dir)
    }

    /// The live runtime record of `name`, if its VMM is running.
    fn live(&self, name: &str) -> Option<(PathBuf, Runtime)> {
        let dir = self.config.store.dir(name).ok()?;
        let runtime = jail::load_runtime(&dir)?;
        Jail::is_running(name, &runtime).then_some((dir, runtime))
    }

    /// `start`: jail, relay, VMM.
    async fn start(&self, name: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        self.config.jail.preflight()?;
        let dir = self.sandbox_dir(name)?;
        anyhow::ensure!(
            self.live(name).is_none(),
            "sandbox {name} is already running"
        );
        let meta = self.config.store.load(name)?;
        let owner = self.owner()?;

        let taken: Vec<u32> = self
            .config
            .store
            .list()?
            .iter()
            .filter_map(|m| self.live(&m.name).map(|(_, runtime)| runtime.jail_uid))
            .collect();
        let jail_uid = self.config.jail.free_uid(&taken)?;

        let launched = self.launch(&dir, &meta, owner, jail_uid).await;
        if let Err(e) = launched {
            self.stop_relay(name).await;
            jail::teardown(&dir, owner).ok();
            return Err(e);
        }
        Ok(())
    }

    async fn launch(
        &self,
        dir: &Path,
        meta: &super::store::SandboxMeta,
        owner: (u32, u32),
        jail_uid: u32,
    ) -> Result<()> {
        let vm_dir = self.config.jail.prepare(dir, meta, jail_uid).await?;
        // The relay listens before the guest boots, so its first connection lands.
        let relay = Relay::new(
            vm_dir,
            dir.join(INBOX),
            owner,
            self.config.upstreams.clone(),
        )
        .listen()?;
        let pid = self.config.jail.spawn(dir, &meta.name, jail_uid)?;
        // Don't judge liveness from /proc until jailer has exec'd.
        wait_started(pid, &jail::needle(&meta.name), EXEC_WAIT).await;
        let runtime = Runtime { pid, jail_uid };
        jail::save_runtime(dir, &runtime)?;
        self.watch_relay(&meta.name, runtime, relay).await;

        // A VMM that rejects its config or the device exits at once; say so
        // rather than reporting a sandbox that never booted as started.
        tokio::time::sleep(BOOT_CHECK).await;
        if !Jail::is_running(&meta.name, &runtime) {
            let console = std::fs::read_to_string(dir.join(CONSOLE_LOG)).unwrap_or_default();
            anyhow::bail!(
                "sandbox {} exited while booting; console output:\n{}",
                meta.name,
                tail_lines(&console, 20)
            );
        }
        Ok(())
    }

    /// Run `relay` until the VMM in `runtime` exits or the sandbox is stopped.
    async fn watch_relay(&self, name: &str, runtime: Runtime, relay: super::relay::Listening) {
        let stop = Arc::new(AtomicBool::new(false));
        self.relays
            .lock()
            .await
            .insert(name.to_owned(), Arc::clone(&stop));
        let name = name.to_owned();
        tokio::spawn(async move {
            relay
                .run_until(|| stop.load(Ordering::Relaxed) || !Jail::is_running(&name, &runtime))
                .await;
        });
    }

    async fn stop_relay(&self, name: &str) {
        if let Some(stop) = self.relays.lock().await.remove(name) {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// `stop`: VMM, relay, jail. Safe to repeat; always hands the image back.
    async fn stop(&self, name: &str) -> Result<StopOutcome> {
        let _guard = self.lifecycle.lock().await;
        let dir = self.sandbox_dir(name)?;
        let outcome = match jail::load_runtime(&dir) {
            Some(runtime) => {
                let vm_dir = self.config.jail.vm_dir(&dir, name);
                stop_vmm(&vm_dir, runtime.pid, &jail::needle(name))
                    .await
                    .with_context(|| format!("stop sandbox {name}"))?
            }
            None => StopOutcome::NotRunning,
        };
        self.stop_relay(name).await;
        jail::teardown(&dir, self.owner()?)?;
        Ok(outcome)
    }

    /// Names of the sandboxes whose VMM is running.
    fn running(&self) -> BTreeSet<String> {
        self.config
            .store
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|meta| self.live(&meta.name).is_some())
            .map(|meta| meta.name)
            .collect()
    }

    /// Resume the relays of sandboxes that kept running across a daemon restart.
    async fn recover(&self) {
        let Ok(owner) = self.owner() else {
            return;
        };
        for meta in self.config.store.list().unwrap_or_default() {
            let Some((dir, runtime)) = self.live(&meta.name) else {
                continue;
            };
            let vm_dir = self.config.jail.vm_dir(&dir, &meta.name);
            match Relay::new(
                vm_dir,
                dir.join(INBOX),
                owner,
                self.config.upstreams.clone(),
            )
            .listen()
            {
                Ok(relay) => {
                    tracing::info!(sandbox = meta.name, "resumed relay for a running sandbox");
                    self.watch_relay(&meta.name, runtime, relay).await;
                }
                Err(e) => tracing::warn!(
                    sandbox = meta.name,
                    error = %format!("{e:#}"),
                    "could not resume relay"
                ),
            }
        }
    }
}

/// Write `response` as one JSON line.
async fn reply(conn: &mut UnixStream, response: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(response).context("serialize response")?;
    line.push(b'\n');
    conn.write_all(&line).await.context("write response")
}

fn error(e: &anyhow::Error) -> Response {
    Response::Error {
        message: format!("{e:#}"),
    }
}

/// The last `n` lines of `text`.
pub(crate) fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    lines.into_iter().skip(skip).collect::<Vec<_>>().join("\n")
}

/// The CLI's handle on the daemon.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    socket: PathBuf,
}

impl Client {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Send `request` and return the daemon's answer. An error response
    /// becomes an `Err` carrying the daemon's message.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon is unreachable or answers with an error.
    pub(crate) async fn call(&self, request: &Request) -> Result<Response> {
        let response = tokio::time::timeout(CLIENT_TIMEOUT, self.exchange(request))
            .await
            .context("the sandbox daemon did not answer in time")??;
        match response {
            Response::Error { message } => Err(anyhow::anyhow!(message)),
            other => Ok(other),
        }
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "the sandbox daemon isn't reachable at {} (check `systemctl status \
                 devbox-sandboxd`; this AMI may predate it)",
                self.socket.display()
            )
        })
    }

    async fn exchange(&self, request: &Request) -> Result<Response> {
        let conn = self.connect().await?;
        let (read, mut write) = conn.into_split();
        let mut line = serde_json::to_vec(request).context("serialize request")?;
        line.push(b'\n');
        write.write_all(&line).await.context("send request")?;
        let mut reply = String::new();
        BufReader::new(read)
            .read_line(&mut reply)
            .await
            .context("read the daemon's answer")?;
        serde_json::from_str(reply.trim()).context("parse the daemon's answer")
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Request, Response, tail_lines};
    use crate::sandbox::firecracker::StopOutcome;

    #[test]
    fn protocol_is_one_tagged_json_object_each_way() {
        let start = serde_json::to_string(&Request::Start {
            name: "calm-quilt".to_owned(),
        })
        .unwrap();
        assert_eq!(start, r#"{"op":"start","name":"calm-quilt"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"status"}"#).unwrap(),
            Request::Status
        );
        let stopped = serde_json::to_string(&Response::Stopped {
            outcome: StopOutcome::Clean,
        })
        .unwrap();
        assert_eq!(stopped, r#"{"result":"stopped","outcome":"clean"}"#);
        let status = Response::Status {
            running: BTreeSet::from(["calm-quilt".to_owned()]),
        };
        let round: Response =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(round, status);
        assert!(
            serde_json::from_str::<Request>(r#"{"op":"start","name":"x","path":"/etc"}"#).is_ok()
        );
    }

    #[test]
    fn only_the_claimant_and_root_may_call() {
        use std::collections::HashMap;

        use super::{Config, Daemon};
        use crate::sandbox::jail::{Jail, owner_of};
        use crate::sandbox::relay::Upstreams;
        use crate::sandbox::store::Store;
        use crate::sandbox::test_support::temp_root;

        let root = temp_root("daemon-auth");
        let (owner, gid) = owner_of(&root).unwrap();
        let daemon = Daemon {
            config: Config {
                socket: root.join("d.sock"),
                store: Store::new(root.join("store")),
                owner_path: root.clone(),
                jail: Jail::rooted_at(&root, owner, gid),
                upstreams: Upstreams::default(),
            },
            relays: tokio::sync::Mutex::new(HashMap::new()),
            lifecycle: tokio::sync::Mutex::new(()),
        };
        assert!(daemon.authorize(owner).is_ok());
        assert!(daemon.authorize(0).is_ok());
        let other = owner.checked_add(1).unwrap();
        let err = daemon.authorize(other).unwrap_err();
        assert!(
            format!("{err:#}").contains("may not manage sandboxes"),
            "{err:#}"
        );
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
        assert_eq!(tail_lines("", 5), "");
    }
}
