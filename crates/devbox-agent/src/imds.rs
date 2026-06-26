//! IMDSv2 access via the AWS SDK's metadata client.
//!
//! Wraps [`aws_config::imds::Client`], which manages the IMDSv2 session token,
//! endpoint, and retries internally. All three subcommands read instance
//! identity and tags through these helpers.

use anyhow::{Context, Result};
use aws_config::imds::client::Client;
use aws_config::imds::client::error::ImdsError;

/// Build an IMDS client. Token handling is managed internally and lazily, so
/// this is cheap and synchronous; the client is used from within a runtime.
pub(crate) fn client() -> Client {
    Client::builder().build()
}

/// Fetch a metadata `path`. Returns `Ok(None)` when IMDS responds `404` (the
/// path is absent — e.g. an unset tag) and `Ok(Some(value))` on success.
///
/// # Errors
///
/// Returns an error on transport failure or any non-404 error response.
pub(crate) async fn get(client: &Client, path: &str) -> Result<Option<String>> {
    match client.get(path).await {
        Ok(value) => Ok(Some(String::from(value).trim().to_string())),
        Err(ImdsError::ErrorResponse(ctx)) if ctx.response().status().as_u16() == 404 => Ok(None),
        Err(e) => Err(anyhow::anyhow!("IMDS get {path}: {e}")),
    }
}

/// Fetch an instance tag (requires `InstanceMetadataTags=enabled` on the Launch
/// Template). Returns `None` when the tag is absent.
///
/// # Errors
///
/// Returns an error on transport failure or an unexpected error response.
pub(crate) async fn instance_tag(client: &Client, key: &str) -> Result<Option<String>> {
    get(client, &format!("/latest/meta-data/tags/instance/{key}")).await
}

/// Resolve the AWS region: `AWS_REGION` -> `AWS_DEFAULT_REGION` -> IMDS
/// `placement/region`. The IMDS fallback is needed because the Rust SDK's
/// default region chain checks only env + profile (no IMDS) and AWS_REGION is
/// unset inside the `devbox-warmup.service` systemd unit.
///
/// # Errors
///
/// Returns an error on IMDS transport failure or if none of the sources
/// provides a region.
pub(crate) async fn region() -> Result<String> {
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    let client = client();
    get(&client, "/latest/meta-data/placement/region")
        .await?
        .context("region unavailable from IMDS")
}
