//! Minimal, dependency-free IMDSv2 client.
//!
//! Talks plain HTTP/1.1 to the link-local Instance Metadata Service
//! (`169.254.169.254:80`). It is used by the `principals` fast path — which sshd
//! runs as `nobody` on every authentication — so it must stay tiny, have no TLS
//! or AWS-SDK weight, and fail closed on any error.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Link-local metadata endpoint (IMDSv2 over plain HTTP).
const IMDS_ADDR: &str = "169.254.169.254:80";

/// Per-call connect/read/write timeout. IMDS is local, so this is generous.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Acquire an IMDSv2 session token (valid for 60 seconds).
///
/// # Errors
///
/// Returns an error if the metadata service is unreachable or rejects the
/// token request.
pub(crate) fn fetch_token() -> Result<String> {
    let req = "PUT /latest/api/token HTTP/1.1\r\n\
        Host: 169.254.169.254\r\n\
        X-aws-ec2-metadata-token-ttl-seconds: 60\r\n\
        Content-Length: 0\r\n\
        Connection: close\r\n\r\n";
    let (status, body) = request(req)?;
    if status != 200 {
        bail!("IMDS token request returned HTTP {status}");
    }
    Ok(body.trim().to_string())
}

/// Fetch a metadata path. Returns `Ok(None)` on 404 (path absent),
/// `Ok(Some(value))` on 200.
///
/// # Errors
///
/// Returns an error on transport failure or any non-200/404 status.
pub(crate) fn get(token: &str, path: &str) -> Result<Option<String>> {
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
        Host: 169.254.169.254\r\n\
        X-aws-ec2-metadata-token: {token}\r\n\
        Connection: close\r\n\r\n"
    );
    let (status, body) = request(&req)?;
    match status {
        200 => Ok(Some(body.trim().to_string())),
        404 => Ok(None),
        other => bail!("IMDS GET {path} returned HTTP {other}"),
    }
}

/// Fetch an instance tag via IMDS. Requires `InstanceMetadataTags=enabled` on
/// the instance. Returns `None` when the tag is absent.
///
/// # Errors
///
/// Returns an error on transport failure or an unexpected status.
pub(crate) fn instance_tag(token: &str, key: &str) -> Result<Option<String>> {
    get(token, &format!("/latest/meta-data/tags/instance/{key}"))
}

/// Send one HTTP/1.1 request and return `(status_code, body)`.
fn request(raw: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(IMDS_ADDR).context("connect to IMDS")?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    stream
        .write_all(raw.as_bytes())
        .context("write IMDS request")?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).context("read IMDS response")?;
    let text = String::from_utf8_lossy(&buf);

    let mut sections = text.splitn(2, "\r\n\r\n");
    let head = sections.next().unwrap_or("");
    let body = sections.next().unwrap_or("").to_string();

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("parse IMDS status line")?;

    Ok((status, body))
}
