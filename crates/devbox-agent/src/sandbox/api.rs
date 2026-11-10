//! Runtime control of a running sandbox: Firecracker's API socket, and the
//! guest's control port over vsock.
//!
//! The VMM boots from its config file and also serves its HTTP API on
//! `<sandbox dir>/api.sock`. That is used to read the VM state, pause and
//! resume, and (on x86_64 only) send Ctrl-Alt-Del.
//!
//! A clean shutdown is asked of the guest itself: the host opens a vsock
//! connection to guest port [`CONTROL_PORT`] through Firecracker's Unix socket
//! (`CONNECT <port>\n`, answered `OK <port>\n`) and sends `shutdown\n`. The
//! guest image's init is expected to listen there, sync and unmount the
//! workspace, and power off, which ends the VMM. A guest that doesn't answer is
//! stopped by signal instead (see [`super::firecracker::Firecracker::stop`]).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use super::firecracker::VSOCK;

/// Firecracker's API socket, inside the sandbox directory.
pub(crate) const API_SOCKET: &str = "api.sock";

/// Guest vsock port the guest init listens on for control requests.
pub(crate) const CONTROL_PORT: u32 = 1024;

/// Bound on one API call or control exchange.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The VM state Firecracker reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VmState {
    Running,
    Paused,
    /// Any other state Firecracker reports (e.g. `Not started`).
    Other(String),
}

/// Call the API at `socket`: `method path` with an optional JSON body. Returns
/// the status and the response body.
///
/// # Errors
///
/// Returns an error if the socket can't be reached or the exchange times out.
pub(crate) async fn call(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<(StatusCode, Bytes)> {
    tokio::time::timeout(CALL_TIMEOUT, call_inner(socket, method, path, body))
        .await
        .with_context(|| format!("Firecracker API {path} timed out"))?
}

async fn call_inner(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<(StatusCode, Bytes)> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("Firecracker API handshake")?;
    let driver = tokio::spawn(connection);

    let payload = match body {
        Some(value) => serde_json::to_vec(value).context("serialize API body")?,
        None => Vec::new(),
    };
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost")
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload)))
        .context("build API request")?;
    let response = sender
        .send_request(request)
        .await
        .with_context(|| format!("Firecracker API {path}"))?;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .context("read API response")?
        .to_bytes();
    driver.abort();
    Ok((status, bytes))
}

/// Fail unless `status` is a success, quoting Firecracker's fault message.
fn expect_success(what: &str, status: StatusCode, body: &Bytes) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let fault = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("fault_message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    anyhow::bail!("{what} failed ({status}): {fault}")
}

/// The VM state of the sandbox in `dir`.
///
/// # Errors
///
/// Returns an error if the API is unreachable or answers unexpectedly.
pub(crate) async fn vm_state(dir: &Path) -> Result<VmState> {
    let (status, body) = call(&dir.join(API_SOCKET), Method::GET, "/", None).await?;
    expect_success("read VM state", status, &body)?;
    let info: Value = serde_json::from_slice(&body).context("parse instance info")?;
    let state = info
        .get("state")
        .and_then(Value::as_str)
        .context("instance info has no state")?;
    Ok(match state {
        "Running" => VmState::Running,
        "Paused" => VmState::Paused,
        other => VmState::Other(other.to_owned()),
    })
}

/// Pause (`true`) or resume (`false`) the sandbox in `dir`.
///
/// # Errors
///
/// Returns an error if Firecracker refuses or can't be reached.
pub(crate) async fn set_paused(dir: &Path, paused: bool) -> Result<()> {
    let state = if paused { "Paused" } else { "Resumed" };
    let (status, body) = call(
        &dir.join(API_SOCKET),
        Method::PATCH,
        "/vm",
        Some(&json!({ "state": state })),
    )
    .await?;
    expect_success(if paused { "pause" } else { "resume" }, status, &body)
}

/// Send Ctrl-Alt-Del to the guest (Firecracker supports this on x86_64 only).
///
/// # Errors
///
/// Returns an error if Firecracker refuses or can't be reached.
pub(crate) async fn send_ctrl_alt_del(dir: &Path) -> Result<()> {
    let (status, body) = call(
        &dir.join(API_SOCKET),
        Method::PUT,
        "/actions",
        Some(&json!({ "action_type": "SendCtrlAltDel" })),
    )
    .await?;
    expect_success("send Ctrl-Alt-Del", status, &body)
}

/// Read one `\n`-terminated line from `stream`, a byte at a time, so nothing
/// after it is consumed (the peer's next bytes may be a terminal session).
///
/// # Errors
///
/// Returns an error on EOF before a newline, a line over `max` bytes, or
/// non-UTF-8 input.
pub(crate) async fn read_line_unbuffered(stream: &mut UnixStream, max: usize) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    loop {
        let byte = stream
            .read_u8()
            .await
            .context("connection closed mid-line")?;
        if byte == b'\n' {
            break;
        }
        anyhow::ensure!(line.len() < max, "line longer than {max} bytes");
        line.push(byte);
    }
    String::from_utf8(line).context("line is not UTF-8")
}

/// Open a connection to the guest's control port through Firecracker's vsock
/// socket in `vm_dir`, past the `CONNECT`/`OK` handshake.
///
/// # Errors
///
/// Returns an error if the VMM is gone or the guest isn't listening.
pub(crate) async fn open_control(vm_dir: &Path) -> Result<UnixStream> {
    tokio::time::timeout(CALL_TIMEOUT, async {
        let mut stream = UnixStream::connect(vm_dir.join(VSOCK))
            .await
            .context("connect to the sandbox vsock")?;
        stream
            .write_all(format!("CONNECT {CONTROL_PORT}\n").as_bytes())
            .await
            .context("open the guest control port")?;
        let reply = read_line_unbuffered(&mut stream, 64)
            .await
            .context("read the vsock handshake")?;
        anyhow::ensure!(
            reply.starts_with("OK "),
            "the guest is not listening on its control port"
        );
        Ok(stream)
    })
    .await
    .context("connecting to the guest control port timed out")?
}

/// Ask the guest to power off cleanly through its control port.
///
/// # Errors
///
/// Returns an error if the guest doesn't accept the connection (no listener on
/// [`CONTROL_PORT`], or the VMM is gone).
pub(crate) async fn request_guest_shutdown(vm_dir: &Path) -> Result<()> {
    let mut stream = open_control(vm_dir).await?;
    stream
        .write_all(b"shutdown\n")
        .await
        .context("send shutdown")?;
    stream.shutdown().await.ok();
    Ok(())
}

// Unix-socket paths of a scratch dir: Linux only, like the backend.
#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::{
        API_SOCKET, CONTROL_PORT, VmState, request_guest_shutdown, send_ctrl_alt_del, set_paused,
        vm_state,
    };
    use crate::sandbox::firecracker::VSOCK;
    use crate::sandbox::test_support::temp_root;

    /// A stand-in Firecracker API holding a VM state; returns the requests it saw.
    fn serve_api(dir: &std::path::Path) -> Arc<Mutex<Vec<String>>> {
        let listener = UnixListener::bind(dir.join(API_SOCKET)).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new("Running".to_owned()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (conn, _) = listener.accept().await.unwrap();
                let (log, state) = (Arc::clone(&log), Arc::clone(&state));
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let (log, state) = (Arc::clone(&log), Arc::clone(&state));
                        async move {
                            let line = format!("{} {}", req.method(), req.uri());
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let body = String::from_utf8_lossy(&body).into_owned();
                            log.lock().unwrap().push(format!("{line} {body}"));
                            let response = match line.as_str() {
                                "GET /" => {
                                    let s = state.lock().unwrap().clone();
                                    Response::new(Full::new(Bytes::from(format!(
                                        r#"{{"id":"x","state":"{s}"}}"#
                                    ))))
                                }
                                "PATCH /vm" => {
                                    let next = if body.contains("Paused") {
                                        "Paused"
                                    } else {
                                        "Running"
                                    };
                                    *state.lock().unwrap() = next.to_owned();
                                    let mut r = Response::new(Full::new(Bytes::new()));
                                    *r.status_mut() = StatusCode::NO_CONTENT;
                                    r
                                }
                                _ => {
                                    let mut r = Response::new(Full::new(Bytes::from(
                                        r#"{"fault_message":"unsupported on aarch64"}"#,
                                    )));
                                    *r.status_mut() = StatusCode::BAD_REQUEST;
                                    r
                                }
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    http1::Builder::new()
                        .serve_connection(TokioIo::new(conn), service)
                        .await
                        .ok();
                });
            }
        });
        seen
    }

    #[tokio::test]
    async fn state_pause_and_resume_go_through_the_api_socket() {
        let dir = temp_root("api");
        let seen = serve_api(&dir);

        assert_eq!(vm_state(&dir).await.unwrap(), VmState::Running);
        set_paused(&dir, true).await.unwrap();
        assert_eq!(vm_state(&dir).await.unwrap(), VmState::Paused);
        set_paused(&dir, false).await.unwrap();
        assert_eq!(vm_state(&dir).await.unwrap(), VmState::Running);

        let err = send_ctrl_alt_del(&dir).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("unsupported on aarch64"),
            "{err:#}"
        );
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&r#"PATCH /vm {"state":"Paused"}"#.to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&r#"PATCH /vm {"state":"Resumed"}"#.to_owned()),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn api_calls_fail_cleanly_without_a_vmm() {
        let dir = temp_root("api-none");
        assert!(vm_state(&dir).await.is_err());
        assert!(request_guest_shutdown(&dir).await.is_err());
    }

    #[tokio::test]
    async fn shutdown_request_speaks_the_firecracker_vsock_handshake() {
        let dir = temp_root("api-control");
        let listener = UnixListener::bind(dir.join(VSOCK)).unwrap();
        let guest = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let mut conn = BufReader::new(conn);
            let mut line = String::new();
            conn.read_line(&mut line).await.unwrap();
            assert_eq!(line, format!("CONNECT {CONTROL_PORT}\n"));
            conn.get_mut().write_all(b"OK 1073741824\n").await.unwrap();
            let mut command = String::new();
            conn.read_line(&mut command).await.unwrap();
            command
        });
        request_guest_shutdown(&dir).await.unwrap();
        assert_eq!(guest.await.unwrap(), "shutdown\n");
    }
}
