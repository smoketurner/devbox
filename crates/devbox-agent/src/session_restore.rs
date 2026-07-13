//! Session restore driver: download the archive and unpack it onto the box.
//!
//! Invoked by `owner-sync` right after account provisioning when the
//! `devbox:session-restore` tag names a session (`claim --resume`). The
//! archive is fetched through a server-minted presigned S3 GET — the host has
//! no S3 IAM — and handed to [`crate::session::restore_session`]. Strictly
//! best-effort at the call site: any error here is logged and the claim
//! proceeds on a fresh box.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session;

/// Workspace the session is restored into.
const WORKSPACE: &str = "/workspace";

/// Download timeout for the presigned GET — archives can be large.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Download and restore `session_id` for `owner`.
///
/// # Errors
///
/// Returns an error when the control-plane client is unavailable, the
/// download fails, or the archive cannot be extracted/restored.
pub(crate) async fn run(session_id: &str, owner: &str) -> Result<()> {
    let Some(mut control_plane) = crate::git::control_plane_client().await else {
        bail!("no control-plane client; cannot restore session");
    };
    let url = control_plane.session_restore_url(session_id).await?;

    let staging = staging_dir()?;
    let archive = staging.join("session.tar.gz");
    download(&url, &archive).await?;

    let extracted = staging.join("extracted");
    std::fs::create_dir_all(&extracted)
        .with_context(|| format!("create {}", extracted.display()))?;
    session::extract_archive(&archive, &extracted).await?;

    let home = crate::owner_sync::user_home(owner).map(PathBuf::from);
    session::restore_session(&extracted, Path::new(WORKSPACE), home.as_deref(), owner).await?;

    std::fs::remove_dir_all(&staging).ok();
    Ok(())
}

/// GET the presigned URL to `dest`, streaming to disk (archives can be large;
/// buffering one in memory risks OOMing the box).
async fn download(url: &str, dest: &Path) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent("devbox-agent")
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("build download HTTP client")?;
    let mut resp = http
        .get(url)
        .send()
        .await
        .context("download session archive")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("archive download failed ({status}): {body}");
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    while let Some(chunk) = resp.chunk().await.context("read session archive body")? {
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .with_context(|| format!("write {}", dest.display()))?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .with_context(|| format!("flush {}", dest.display()))?;
    Ok(())
}

/// A fresh staging directory for the restore.
fn staging_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("devbox-restore-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}
