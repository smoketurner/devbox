//! Host side of a sandbox's vsock channel.
//!
//! The guest has no network device. Its only way out is vsock: when the guest
//! connects to the host (CID 2) on port N, Firecracker hands the connection to
//! the Unix socket `<sandbox dir>/vsock.sock_N`. This relay, started by
//! `sandbox run` next to the VMM, listens on those sockets. Each port has one
//! fixed meaning:
//!
//! | Port | The guest uses it for | The relay connects it to |
//! |------|-----------------------|--------------------------|
//! | [`EGRESS_PORT`] | `HTTPS_PROXY` | the box's egress proxy, as an opaque byte pipe (the guest speaks `CONNECT` itself, and the proxy's policy decides) |
//! | [`MODEL_PORT`] | model APIs, e.g. `ANTHROPIC_BASE_URL=http://127.0.0.1:8080/anthropic` | the control plane's credential-injecting proxy at `/egress/<integration>/…`, adding this box's web-identity token |
//! | [`GIT_PORT`] | `git push` of its branch, so progress is visible while it runs | `git receive-pack` into the sandbox's own bare repository ([`INBOX`]), never the claimant's |
//!
//! The guest image maps these to loopback ports inside the guest. No credential
//! ever enters the guest: the model API key stays on the control plane, the
//! box's identity token stays in this process, and any credential header the
//! guest sends is dropped. A port whose upstream isn't configured on this box
//! (no `HTTPS_PROXY`, no `DEVBOX_SERVER_URL`) isn't bound, so the guest's
//! connection is refused instead of silently going somewhere else.
//!
//! The relay exits by itself once the VMM is gone or the sandbox is collected.

use std::convert::Infallible;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use devbox_common::env_non_empty;
use futures_util::TryStreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{self, HeaderMap};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::process::Command;

use super::firecracker::{Firecracker, VSOCK};
use super::store::Store;
use crate::control_plane::ControlPlaneClient;

/// Guest port for outbound HTTPS through the egress proxy.
pub(crate) const EGRESS_PORT: u32 = 3128;
/// Guest port for model API calls through the control plane.
pub(crate) const MODEL_PORT: u32 = 8080;
/// Guest port for `git push` into the inbox. It speaks `receive-pack` only, so
/// the guest pushes over git's `ext::` transport and leaves `push.negotiate`
/// off (its default): negotiation opens a fetch connection, which is refused.
pub(crate) const GIT_PORT: u32 = 9418;

/// The sandbox's bare repository that receives the guest's pushes.
pub(crate) const INBOX: &str = "inbox.git";

/// Largest model API request body forwarded (requests carry prompts and images).
const MAX_REQUEST_BODY: usize = 33_554_432;

/// How often the relay checks whether its sandbox is gone.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// How long the relay waits for `run` to record the VMM before giving up.
const STARTUP_GRACE: Duration = Duration::from_mins(1);

/// Time budget for one `git push`.
const PUSH_TIMEOUT: Duration = Duration::from_mins(10);

/// Model API timeouts: connect quickly, but let long streamed responses run.
const MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_READ_TIMEOUT: Duration = Duration::from_mins(5);

/// Env var overriding the egress proxy (`host:port` or `http://host:port`);
/// otherwise the box's own `HTTPS_PROXY` is used.
const EGRESS_ENV: &str = "DEVBOX_SANDBOX_EGRESS_PROXY";

/// Request headers never forwarded upstream: credentials the guest might send
/// (the relay and the control plane supply the real ones), and hop-by-hop or
/// framing headers that describe the guest's connection, not the request.
const DROP_REQUEST_HEADERS: [&str; 12] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response headers never copied back: hop-by-hop headers of the upstream hop.
const DROP_RESPONSE_HEADERS: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The Unix socket Firecracker connects guest port `port` to.
pub(crate) fn socket_path(dir: &Path, port: u32) -> PathBuf {
    dir.join(format!("{VSOCK}_{port}"))
}

/// Longest Unix socket path the kernel accepts (`sun_path` is 108 bytes,
/// including the terminating NUL).
const MAX_SOCKET_PATH: usize = 107;

/// Check the sandbox in `dir` can have vsock sockets: Firecracker and the relay
/// both bind `<dir>/vsock.sock_<port>`, which must fit in a Unix socket path.
///
/// # Errors
///
/// Returns an error naming the fix when the path is too long.
pub(crate) fn check_socket_path(dir: &Path) -> Result<()> {
    let longest = socket_path(dir, GIT_PORT);
    let len = longest.as_os_str().len();
    anyhow::ensure!(
        len <= MAX_SOCKET_PATH,
        "sandbox socket path {} is {len} bytes, over the {MAX_SOCKET_PATH}-byte Unix socket \
         limit; point DEVBOX_SANDBOX_DIR at a shorter directory",
        longest.display()
    );
    Ok(())
}

/// The command-line fragment identifying the relay for sandbox `name` in
/// `store` (used to find and stop it by pid).
pub(crate) fn needle(store: &Path, name: &str) -> String {
    format!("relay\0{name}\0--store\0{}", store.display())
}

/// Create the empty bare repository the guest pushes into.
///
/// Its config is written here, by the host, and `receive-pack` can't change it,
/// so it can be fetched from without the precautions a guest-written repository
/// needs. Pushed objects are checked (`receive.fsckObjects`) and refs can't be
/// deleted.
///
/// # Errors
///
/// Returns an error if git fails.
pub(crate) async fn init_inbox(dir: &Path) -> Result<()> {
    let inbox = dir.join(INBOX);
    let status = Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&inbox)
        .status()
        .await
        .context("run git init")?;
    anyhow::ensure!(status.success(), "git init {} failed", inbox.display());
    for (key, value) in [
        ("receive.fsckObjects", "true"),
        ("receive.denyDeletes", "true"),
        ("core.hooksPath", "/dev/null"),
    ] {
        let status = Command::new("git")
            .arg("--git-dir")
            .arg(&inbox)
            .args(["config", key, value])
            .status()
            .await
            .context("run git config")?;
        anyhow::ensure!(status.success(), "git config {key} failed");
    }
    Ok(())
}

/// Credentials the model proxy presents to the control plane.
pub(crate) enum Auth {
    /// This box's web-identity token, minted and refreshed via STS.
    ControlPlane(tokio::sync::Mutex<ControlPlaneClient>),
    /// A fixed token, for tests standing in for the control plane.
    #[cfg(test)]
    Fixed(String),
}

impl Auth {
    async fn token(&self) -> Result<String> {
        match self {
            Self::ControlPlane(client) => client.lock().await.bearer().await,
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
        }
    }
}

/// Where model API calls go.
pub(crate) struct ModelUpstream {
    http: reqwest::Client,
    /// Control-plane base URL, without a trailing slash.
    base_url: String,
    auth: Auth,
}

impl ModelUpstream {
    /// An upstream at `base_url` authenticating with `auth`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client can't be built.
    pub(crate) fn new(base_url: &str, auth: Auth) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("devbox-agent-sandbox")
            .connect_timeout(MODEL_CONNECT_TIMEOUT)
            .read_timeout(MODEL_READ_TIMEOUT)
            .build()
            .context("build model API client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth,
        })
    }
}

/// The relay for one sandbox directory.
pub(crate) struct Relay {
    dir: PathBuf,
    egress: Option<String>,
    model: Option<Arc<ModelUpstream>>,
}

impl Relay {
    /// A relay for `dir` with explicit upstreams (none means that port is unbound).
    pub(crate) fn new(dir: PathBuf, egress: Option<String>, model: Option<ModelUpstream>) -> Self {
        Self {
            dir,
            egress,
            model: model.map(Arc::new),
        }
    }

    /// A relay for `dir` with the upstreams this box is configured for.
    pub(crate) async fn from_env(dir: PathBuf) -> Self {
        let egress = [EGRESS_ENV, "HTTPS_PROXY", "https_proxy"]
            .into_iter()
            .find_map(env_non_empty)
            .and_then(|raw| {
                let parsed = proxy_authority(&raw);
                if parsed.is_none() {
                    tracing::warn!(
                        proxy = raw,
                        "unsupported egress proxy; egress port disabled"
                    );
                }
                parsed
            });
        let model = match ControlPlaneClient::new().await {
            Ok(Some(client)) => {
                let base = client.base_url().to_owned();
                ModelUpstream::new(&base, Auth::ControlPlane(tokio::sync::Mutex::new(client)))
                    .map_err(|e| tracing::warn!(error = %format!("{e:#}"), "model port disabled"))
                    .ok()
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "no control-plane client; model port disabled");
                None
            }
        };
        if egress.is_none() {
            tracing::warn!("no egress proxy configured; the guest has no outbound HTTPS");
        }
        if model.is_none() {
            tracing::warn!("DEVBOX_SERVER_URL not set; the guest has no model API access");
        }
        Self::new(dir, egress, model)
    }

    /// Listen on the sandbox's vsock sockets until `should_stop` says so.
    ///
    /// # Errors
    ///
    /// Returns an error if a socket can't be bound.
    pub(crate) async fn serve(self, should_stop: impl Fn() -> bool) -> Result<()> {
        let mut tasks = tokio::task::JoinSet::new();
        let mut bound = Vec::new();

        if let Some(proxy) = self.egress {
            let (listener, path) = bind(&self.dir, EGRESS_PORT)?;
            bound.push(path);
            tasks.spawn(accept_loop(listener, "egress", move |guest| {
                pipe_egress(guest, proxy.clone())
            }));
        }
        if let Some(upstream) = self.model {
            let (listener, path) = bind(&self.dir, MODEL_PORT)?;
            bound.push(path);
            tasks.spawn(accept_loop(listener, "model", move |guest| {
                serve_model(guest, Arc::clone(&upstream))
            }));
        }
        let inbox = self.dir.join(INBOX);
        let (listener, path) = bind(&self.dir, GIT_PORT)?;
        bound.push(path);
        tasks.spawn(accept_loop(listener, "git", move |guest| {
            receive_pack(guest, inbox.clone())
        }));

        while !should_stop() {
            tokio::time::sleep(WATCH_INTERVAL).await;
        }
        tasks.abort_all();
        for path in bound {
            std::fs::remove_file(&path).ok();
        }
        Ok(())
    }
}

/// `devbox-agent sandbox relay <name>`: serve until the sandbox is gone.
///
/// # Errors
///
/// Returns an error if the sandbox is unknown or a socket can't be bound.
pub(crate) async fn run(store: &Store, name: &str) -> Result<()> {
    let dir = store.dir(name)?;
    store.load(name)?;
    let relay = Relay::from_env(dir.clone()).await;
    let started = Instant::now();
    relay
        .serve(|| sandbox_gone(store, name, &dir, started))
        .await
}

/// Whether the relay for `name` should exit: the record is gone or collected,
/// the VMM has exited, or `run` never recorded a VMM.
fn sandbox_gone(store: &Store, name: &str, dir: &Path, started: Instant) -> bool {
    let Ok(meta) = store.load(name) else {
        return true;
    };
    if meta.collected {
        return true;
    }
    match meta.pid {
        Some(_) => !Firecracker::is_running(dir, &meta),
        None => started.elapsed() > STARTUP_GRACE,
    }
}

/// Bind the socket for guest `port`, replacing a stale one.
fn bind(dir: &Path, port: u32) -> Result<(UnixListener, PathBuf)> {
    let path = socket_path(dir, port);
    std::fs::remove_file(&path).ok();
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    Ok((listener, path))
}

/// Accept connections on `listener` forever, handling each on its own task.
async fn accept_loop<F, Fut>(listener: UnixListener, port: &'static str, handle: F)
where
    F: Fn(UnixStream) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    loop {
        match listener.accept().await {
            Ok((guest, _)) => {
                let connection = handle(guest);
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        tracing::warn!(port, error = %format!("{e:#}"), "guest connection failed");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(port, error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Pipe a guest connection to the egress proxy, byte for byte.
async fn pipe_egress(mut guest: UnixStream, proxy: String) -> Result<()> {
    let mut upstream = TcpStream::connect(&proxy)
        .await
        .with_context(|| format!("connect to egress proxy {proxy}"))?;
    tokio::io::copy_bidirectional(&mut guest, &mut upstream)
        .await
        .context("relay egress bytes")?;
    Ok(())
}

/// Run `git receive-pack` on the inbox with the guest connection as its stdio.
async fn receive_pack(guest: UnixStream, inbox: PathBuf) -> Result<()> {
    let mut child = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "receive-pack"])
        .arg(&inbox)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start git receive-pack")?;
    let mut stdin = child.stdin.take().context("receive-pack stdin")?;
    let mut stdout = child.stdout.take().context("receive-pack stdout")?;
    let (mut from_guest, mut to_guest) = guest.into_split();

    let push = async {
        let upload = tokio::spawn(async move {
            let copied = tokio::io::copy(&mut from_guest, &mut stdin).await;
            drop(stdin);
            copied
        });
        // receive-pack closes its stdout when the push is done.
        tokio::io::copy(&mut stdout, &mut to_guest).await?;
        to_guest.shutdown().await.ok();
        upload.abort();
        child.wait().await
    };
    let status = tokio::time::timeout(PUSH_TIMEOUT, push)
        .await
        .context("git push from the guest timed out")?
        .context("relay git push")?;
    anyhow::ensure!(status.success(), "git receive-pack exited with {status}");
    Ok(())
}

/// Response body type for the model proxy: streamed upstream bytes or a short
/// error message.
type RelayBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// Serve HTTP/1.1 model API requests from one guest connection.
async fn serve_model(guest: UnixStream, upstream: Arc<ModelUpstream>) -> Result<()> {
    let service = service_fn(move |req| {
        let upstream = Arc::clone(&upstream);
        async move { Ok::<_, Infallible>(proxy_model(req, &upstream).await) }
    });
    http1::Builder::new()
        .serve_connection(TokioIo::new(guest), service)
        .await
        .context("serve model API connection")
}

/// Forward one model API request, turning failures into an HTTP error for the guest.
async fn proxy_model(req: Request<Incoming>, upstream: &ModelUpstream) -> Response<RelayBody> {
    match forward_model(req, upstream).await {
        Ok(response) => response,
        Err((status, message)) => {
            tracing::warn!(%status, message, "model API request not forwarded");
            text_response(status, &format!("devbox sandbox relay: {message}\n"))
        }
    }
}

/// `/<integration>/<rest>` → `{base}/egress/<integration>/<rest>`, with the
/// guest's credential headers dropped and the box's token added.
async fn forward_model(
    req: Request<Incoming>,
    upstream: &ModelUpstream,
) -> std::result::Result<Response<RelayBody>, (StatusCode, String)> {
    let (parts, body) = req.into_parts();
    let (integration, rest) = split_integration(parts.uri.path()).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!(
                "expected /<integration>/<path>, e.g. /anthropic/v1/messages; got {}",
                parts.uri.path()
            ),
        )
    })?;
    let mut url = format!("{}/egress/{integration}{rest}", upstream.base_url);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }

    let body = Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
        .map_err(|e| (StatusCode::PAYLOAD_TOO_LARGE, format!("request body: {e}")))?
        .to_bytes();
    let token = upstream.auth.token().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("box identity token: {e:#}"),
        )
    })?;

    let response = upstream
        .http
        .request(parts.method, &url)
        .headers(filter_headers(&parts.headers, &DROP_REQUEST_HEADERS))
        .bearer_auth(token)
        .body(body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("control plane: {e}")))?;

    let mut builder = Response::builder().status(response.status());
    if let Some(headers) = builder.headers_mut() {
        *headers = filter_headers(response.headers(), &DROP_RESPONSE_HEADERS);
    }
    let stream = response
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    builder
        .body(StreamBody::new(stream).boxed_unsync())
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("build response: {e}")))
}

/// Split `/<integration>/<rest>` into the integration name and `/<rest>`.
fn split_integration(path: &str) -> Option<(&str, &str)> {
    let path = path.strip_prefix('/')?;
    let (integration, rest) = match path.find('/') {
        Some(slash) => path.split_at(slash),
        None => (path, ""),
    };
    let valid = !integration.is_empty()
        && integration
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    valid.then_some((integration, rest))
}

/// `headers` without the (lowercase) names in `drop`.
fn filter_headers(headers: &HeaderMap, drop: &[&str]) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !drop.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// A plain-text response.
fn text_response(status: StatusCode, message: &str) -> Response<RelayBody> {
    let mut response = Response::new(
        Full::new(Bytes::from(message.to_owned()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// `host:port` from an `http://host:port[/]` (or bare `host:port`) proxy URL.
/// HTTPS proxies and URLs with credentials aren't supported.
fn proxy_authority(raw: &str) -> Option<String> {
    let rest = match raw.split_once("://") {
        Some(("http" | "HTTP", rest)) => rest,
        Some(_) => return None,
        None => raw,
    };
    let authority = rest.split('/').next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let valid = !host.is_empty() && !authority.contains('@') && port.parse::<u16>().is_ok();
    valid.then(|| authority.to_owned())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod pure_tests {
    use super::{check_socket_path, proxy_authority, split_integration};

    #[test]
    fn integration_paths_split_and_validate() {
        assert_eq!(
            split_integration("/anthropic/v1/messages"),
            Some(("anthropic", "/v1/messages"))
        );
        assert_eq!(split_integration("/openai"), Some(("openai", "")));
        for bad in ["", "/", "//v1", "/Anthropic/v1", "/../etc", "v1/messages"] {
            assert_eq!(split_integration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn socket_paths_must_fit_in_sun_path() {
        let ok = std::path::Path::new("/workspace/.sandboxes/calm-quilt");
        assert!(check_socket_path(ok).is_ok());
        let long = std::path::PathBuf::from(format!("/{}", "d".repeat(100)));
        let err = check_socket_path(&long).unwrap_err();
        assert!(format!("{err:#}").contains("DEVBOX_SANDBOX_DIR"), "{err:#}");
    }

    #[test]
    fn proxy_urls_reduce_to_host_and_port() {
        assert_eq!(
            proxy_authority("http://cp.example.internal:3128").as_deref(),
            Some("cp.example.internal:3128")
        );
        assert_eq!(
            proxy_authority("http://10.0.0.5:3128/").as_deref(),
            Some("10.0.0.5:3128")
        );
        assert_eq!(proxy_authority("proxy:8080").as_deref(), Some("proxy:8080"));
        for bad in [
            "https://p:3128",
            "http://user:pw@p:3128",
            "http://p",
            "p:notaport",
        ] {
            assert_eq!(proxy_authority(bad), None, "{bad:?}");
        }
    }
}

// Unix-socket paths, git and python3 (the stand-in guest's git transport):
// Linux only, like the backend.
#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixStream};

    use super::{
        Auth, EGRESS_PORT, GIT_PORT, INBOX, MODEL_PORT, ModelUpstream, Relay, init_inbox,
        socket_path,
    };
    use crate::sandbox::test_support::{git, init_repo, temp_root};

    /// Start `relay` in the background; the returned flag stops it.
    async fn start(relay: Relay, dir: &Path, port: u32) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        tokio::spawn(async move { relay.serve(|| flag.load(Ordering::Relaxed)).await });
        let socket = socket_path(dir, port);
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop
    }

    #[tokio::test]
    async fn egress_port_pipes_bytes_to_the_proxy_and_unbound_ports_refuse() {
        let dir = temp_root("relay-egress");
        init_inbox(&dir).await.unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut conn, _) = proxy.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let n = conn.read(&mut buf).await.unwrap();
            conn.write_all(buf.get(..n).unwrap()).await.unwrap();
        });

        let stop = start(
            Relay::new(dir.clone(), Some(proxy_addr), None),
            &dir,
            EGRESS_PORT,
        )
        .await;
        let mut guest = UnixStream::connect(socket_path(&dir, EGRESS_PORT))
            .await
            .unwrap();
        guest.write_all(b"CONNECT x:443").await.unwrap();
        let mut echoed = vec![0u8; 13];
        guest.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"CONNECT x:443");

        // No control plane configured: the model port isn't there at all.
        assert!(
            UnixStream::connect(socket_path(&dir, MODEL_PORT))
                .await
                .is_err()
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn model_port_swaps_guest_credentials_for_the_box_token() {
        let dir = temp_root("relay-model");
        init_inbox(&dir).await.unwrap();

        // A stand-in control plane that echoes what it received.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", upstream.local_addr().unwrap());
        tokio::spawn(async move {
            let (conn, _) = upstream.accept().await.unwrap();
            let service = service_fn(|req: Request<Incoming>| async move {
                let header = |name: &str| {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_owned()
                };
                let summary = format!(
                    "path={} auth={} x-api-key={} version={}",
                    req.uri(),
                    header("authorization"),
                    header("x-api-key"),
                    header("anthropic-version"),
                );
                let body = req.into_body().collect().await.unwrap().to_bytes();
                let text = format!("{summary} body={}", String::from_utf8_lossy(&body));
                Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(text))))
            });
            http1::Builder::new()
                .serve_connection(TokioIo::new(conn), service)
                .await
                .ok();
        });

        let model = ModelUpstream::new(&base, Auth::Fixed("box-token".to_owned())).unwrap();
        let stop = start(Relay::new(dir.clone(), None, Some(model)), &dir, MODEL_PORT).await;

        let mut guest = UnixStream::connect(socket_path(&dir, MODEL_PORT))
            .await
            .unwrap();
        guest
            .write_all(
                b"POST /anthropic/v1/messages?beta=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                  x-api-key: guest-placeholder\r\nAuthorization: Bearer guest\r\n\
                  anthropic-version: 2023-06-01\r\nContent-Length: 2\r\n\
                  Connection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
        let mut response = String::new();
        guest.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains(
                "path=/egress/anthropic/v1/messages?beta=1 auth=Bearer box-token \
                 x-api-key=- version=2023-06-01 body={}"
            ),
            "{response}"
        );

        let mut bad = UnixStream::connect(socket_path(&dir, MODEL_PORT))
            .await
            .unwrap();
        bad.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        bad.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        stop.store(true, Ordering::Relaxed);
    }

    /// A git `ext::` transport for the stand-in guest: bridge stdio to a Unix socket.
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

    #[tokio::test]
    async fn git_port_receives_pushes_into_the_inbox() {
        let root = temp_root("relay-git");
        let dir = root.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        init_inbox(&dir).await.unwrap();
        let stop = start(Relay::new(dir.clone(), None, None), &dir, GIT_PORT).await;

        let guest = init_repo(&root.join("guest"));
        let bridge = root.join("bridge.py");
        std::fs::write(&bridge, BRIDGE).unwrap();
        let remote = format!(
            "ext::python3 {} {}",
            bridge.display(),
            socket_path(&dir, GIT_PORT).display()
        );
        let pushed = tokio::task::spawn_blocking(move || {
            git(
                &guest,
                &[
                    "-c",
                    "protocol.ext.allow=always",
                    "push",
                    "--quiet",
                    &remote,
                    "HEAD:refs/heads/agent/calm-quilt",
                ],
            );
            git(&guest, &["rev-parse", "HEAD"])
        })
        .await
        .unwrap();

        let inbox = dir.join(INBOX);
        assert_eq!(
            git(&inbox, &["rev-parse", "refs/heads/agent/calm-quilt"]),
            pushed
        );
        stop.store(true, Ordering::Relaxed);
    }
}
