//! `devbox ssm-proxy`: a native SSM Session Manager tunnel used as an ssh
//! `ProxyCommand`, replacing the external `session-manager-plugin`.
//!
//! `ssh` invokes this as a ProxyCommand (substituting `%h`/`%p`). We call
//! `ssm:StartSession` for the `AWS-StartSSHSession` document, open the returned
//! WebSocket data channel, and pipe the ssh client's stdin/stdout through it.
//! stdout carries the SSH transport, so **all logging goes to stderr**.

mod channel;
mod message;

use anyhow::{Context, Result, anyhow, bail};
use aws_credential_types::provider::ProvideCredentials;

/// Open a native SSM `AWS-StartSSHSession` tunnel to `target` and pipe
/// stdin/stdout through it, returning once either side closes.
///
/// `region` is the instance's region; `profile` selects AWS credentials when one
/// was resolved (otherwise the default credential chain is used).
///
/// # Errors
///
/// Returns an error if credentials cannot be loaded, `StartSession` fails, the
/// WebSocket cannot be opened, or the data channel errors.
pub(crate) async fn run_proxy(
    target: &str,
    region: &str,
    port: u16,
    profile: Option<&str>,
) -> Result<()> {
    // Ensure rustls uses the aws-lc-rs provider (no-op if already installed).
    let _provider = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()));
    if let Some(profile) = profile {
        loader = loader.profile_name(profile);
    }
    let sdk_config = loader.load().await;
    preflight_credentials(&sdk_config, region, profile).await?;
    let client = aws_sdk_ssm::Client::new(&sdk_config);

    let session = client
        .start_session()
        .target(target)
        .document_name("AWS-StartSSHSession")
        .parameters("portNumber", vec![port.to_string()])
        .send()
        .await
        .context("ssm StartSession failed")?;

    let stream_url = session
        .stream_url()
        .context("StartSession response missing stream_url")?;
    let token_value = session
        .token_value()
        .context("StartSession response missing token_value")?;

    let (ws, _response) = tokio_tungstenite::connect_async(stream_url)
        .await
        .context("failed to open the SSM WebSocket data channel")?;

    channel::run(ws, token_value, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Resolve AWS credentials before `StartSession` so a missing/expired profile
/// fails with an actionable message instead of an opaque dispatch error.
async fn preflight_credentials(
    config: &aws_config::SdkConfig,
    region: &str,
    profile: Option<&str>,
) -> Result<()> {
    let Some(provider) = config.credentials_provider() else {
        bail!("no AWS credentials provider is configured for the SSM tunnel");
    };
    provider.provide_credentials().await.map_err(|e| {
        let source = profile.map_or_else(
            || "the default credential chain".to_string(),
            |p| format!("profile '{p}'"),
        );
        anyhow!(
            "could not load AWS credentials for the SSM tunnel (region {region}, via {source}): {e}. \
             Pass `--profile <name>`, set `AWS_PROFILE`, or refresh your AWS login."
        )
    })?;
    Ok(())
}
