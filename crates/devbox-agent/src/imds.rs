//! IMDSv2 access via the AWS SDK's metadata client.
//!
//! Wraps [`aws_config::imds::Client`], which manages the IMDSv2 session token,
//! endpoint, and retries internally. All three subcommands read instance
//! identity and tags through these helpers.

use anyhow::Result;
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
