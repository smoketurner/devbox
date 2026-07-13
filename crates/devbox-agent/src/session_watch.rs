//! Session-watch: archive the session when release asks for it.
//!
//! `devbox release --keep` moves the box to `Archiving` and tags the instance
//! `devbox:archive-session=<session-id>` (the server has no push channel to a
//! box; IMDS tag polling is the same signal path `owner-sync` uses for claims).
//! This service polls for that tag, and on seeing it: packs the session
//! ([`crate::session::pack_session`]), uploads it through a server-minted
//! presigned S3 PUT (the host has no S3 IAM), reports the outcome over the
//! agent channel, and exits — the box is terminated by the control plane once
//! the report lands (or its archive deadline passes).
//!
//! Failures are reported too: a failure report lets the server fail the
//! session immediately instead of waiting out the deadline. The box is
//! terminated either way — this service can lose an archive but never wedge an
//! instance.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use devbox_common::SessionArchiveDoneRequest;

use crate::control_plane::ControlPlaneClient;
use crate::imds;
use crate::session;

/// How often to poll IMDS for the archive-session tag (matches owner-sync).
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Workspace the session is packed from.
const WORKSPACE: &str = "/workspace";

/// Upload timeout for the presigned PUT — archives can be large, so this is
/// far longer than the control-plane API timeout.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Poll for the archive request, then archive, report, and exit.
///
/// # Errors
///
/// Returns an error when the archive request arrived but the control-plane
/// client could not be built (e.g. a transient IMDS failure) — without it
/// neither the upload nor a failure report can happen. The process exits
/// non-zero so systemd's `Restart=on-failure` retries with the tag still set.
pub(crate) async fn run() -> Result<()> {
    tracing::info!("session-watch started; waiting for devbox:archive-session");
    let client = imds::client();
    loop {
        match imds::instance_tag(&client, "devbox:archive-session").await {
            Ok(Some(session_id)) if !session_id.trim().is_empty() => {
                return archive_and_report(&client, session_id.trim()).await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "session-watch tag read failed; will retry"
                );
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Run the archive and report the outcome (both success and failure) so the
/// server resolves the session promptly instead of waiting out its deadline.
async fn archive_and_report(
    imds_client: &aws_config::imds::client::Client,
    session_id: &str,
) -> Result<()> {
    let Some(mut control_plane) = crate::git::control_plane_client().await else {
        // Neither the upload nor a failure report is possible without the
        // client; fail the process so systemd restarts it and the next attempt
        // retries (the archive tag stays set until the box terminates).
        bail!("control-plane client unavailable; cannot archive session {session_id}");
    };

    let report = match archive(imds_client, &mut control_plane, session_id).await {
        Ok(size_bytes) => {
            tracing::info!(session_id, size_bytes, "session archive uploaded");
            SessionArchiveDoneRequest {
                session_id: session_id.to_string(),
                success: true,
                size_bytes: Some(size_bytes),
                error: None,
            }
        }
        Err(e) => {
            tracing::error!(
                session_id,
                error = %format!("{e:#}"),
                "session archive failed"
            );
            SessionArchiveDoneRequest {
                session_id: session_id.to_string(),
                success: false,
                size_bytes: None,
                error: Some(format!("{e:#}")),
            }
        }
    };

    if let Err(e) = control_plane.session_archive_done(&report).await {
        // Exit non-zero so systemd restarts the service and the report is
        // retried: without it the session would sit Pending until the server
        // deadline marks it failed — even when the archive object is already
        // in S3. A restart re-packs and re-uploads (both idempotent) and
        // reports again; the box terminates either way once the server
        // resolves the session or the deadline passes.
        return Err(e.context("report archive outcome"));
    }
    Ok(())
}

/// Pack the session and upload it via the presigned PUT; returns the uploaded
/// size in bytes.
async fn archive(
    imds_client: &aws_config::imds::client::Client,
    control_plane: &mut ControlPlaneClient,
    session_id: &str,
) -> Result<u64> {
    let staging = staging_dir()?;
    let home = claimant_home(imds_client).await?;

    let archive_path =
        session::pack_session(Path::new(WORKSPACE), home.as_deref(), &staging).await?;
    let size_bytes = std::fs::metadata(&archive_path)
        .context("stat session archive")?
        .len();

    let url = control_plane.session_archive_url(session_id).await?;
    upload(&archive_path, &url, size_bytes).await?;

    std::fs::remove_dir_all(&staging).ok();
    Ok(size_bytes)
}

/// IMDS read attempts for the owner tag (with [`POLL_INTERVAL`] between them).
const OWNER_READ_ATTEMPTS: u32 = 3;

/// The claimant's home directory, resolved from the `devbox:owner` tag.
/// `Ok(None)` only when the tag is genuinely absent/empty or the account has
/// no home — repos still archive. A persistent IMDS **error** is propagated
/// (after retries) so the archive fails loudly instead of silently shipping
/// without the home tree.
async fn claimant_home(imds_client: &aws_config::imds::client::Client) -> Result<Option<PathBuf>> {
    let mut attempt = 0u32;
    let owner = loop {
        match imds::instance_tag(imds_client, "devbox:owner").await {
            Ok(owner) => break owner,
            Err(e) => {
                attempt = attempt.saturating_add(1);
                if attempt >= OWNER_READ_ATTEMPTS {
                    return Err(e.context("read devbox:owner for the home archive"));
                }
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "could not read devbox:owner; retrying"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    };
    let Some(owner) = owner
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
    else {
        return Ok(None);
    };
    Ok(crate::owner_sync::user_home(&owner).map(PathBuf::from))
}

/// PUT the archive to the presigned URL, streaming from disk (archives can be
/// large; buffering one in memory risks OOMing the box). The explicit
/// Content-Length keeps the request identity-encoded — S3 rejects chunked
/// uploads without a length.
async fn upload(archive: &Path, url: &str, size_bytes: u64) -> Result<()> {
    let file = tokio::fs::File::open(archive)
        .await
        .with_context(|| format!("open {}", archive.display()))?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let http = reqwest::Client::builder()
        .user_agent("devbox-agent")
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .context("build upload HTTP client")?;
    let resp = http
        .put(url)
        .header(reqwest::header::CONTENT_LENGTH, size_bytes)
        .body(body)
        .send()
        .await
        .context("upload session archive")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("archive upload failed ({status}): {body}");
    }
    Ok(())
}

/// A fresh staging directory for the pack.
fn staging_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("devbox-session-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}
