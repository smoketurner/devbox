//! `devbox ssm-proxy`: a native SSM Session Manager tunnel used as an ssh
//! `ProxyCommand`, replacing the external `session-manager-plugin`.
//!
//! `ssh` invokes this as a ProxyCommand (substituting `%h`/`%p`). We call
//! `ssm:StartSession` for the `AWS-StartSSHSession` document, open the returned
//! WebSocket data channel, and pipe the ssh client's stdin/stdout through it.
//! stdout carries the SSH transport, so **all logging goes to stderr**.

mod channel;
mod message;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use aws_credential_types::provider::ProvideCredentials;

/// Maximum consecutive reconnect attempts before giving up on a session.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;
/// Base delay for reconnect backoff.
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Cap on the reconnect backoff delay.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// A connection that stays up at least this long resets the reconnect budget,
/// so a long-lived session survives any number of well-spaced blips.
const HEALTHY_CONNECTION: Duration = Duration::from_secs(60);

/// Open a native SSM `AWS-StartSSHSession` tunnel to `target` and pipe
/// stdin/stdout through it, transparently resuming across transient connection
/// drops so a live SSH session is never lost to a network blip.
///
/// `region` is the instance's region; `profile` selects AWS credentials when one
/// was resolved (otherwise the default credential chain is used).
///
/// # Errors
///
/// Returns an error if credentials cannot be loaded, `StartSession` fails, or the
/// session cannot be resumed after [`MAX_RECONNECT_ATTEMPTS`] reconnect attempts.
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

    let session_id = session
        .session_id()
        .context("StartSession response missing session_id")?
        .to_string();
    let mut stream_url = session
        .stream_url()
        .context("StartSession response missing stream_url")?
        .to_string();
    let mut token = session
        .token_value()
        .context("StartSession response missing token_value")?
        .to_string();

    let mut state = channel::SessionState::new(tokio::io::stdout());
    let mut input = tokio::io::stdin();
    let mut attempt: u32 = 0;

    loop {
        let ws = match tokio_tungstenite::connect_async(stream_url.as_str()).await {
            Ok((ws, _response)) => ws,
            Err(e) => {
                eprintln!("devbox ssm-proxy: failed to open data channel: {e}");
                match resume_session(&client, &session_id).await? {
                    Some((url, tok)) => (stream_url, token) = (url, tok),
                    None => return Ok(()),
                }
                reconnect_backoff(&mut attempt).await?;
                continue;
            }
        };

        let started = Instant::now();
        match channel::run_connection(ws, &token, &mut state, &mut input).await {
            channel::Outcome::Closed => return Ok(()),
            channel::Outcome::Dropped => {
                eprintln!("devbox ssm-proxy: connection dropped; resuming session");
                if started.elapsed() >= HEALTHY_CONNECTION {
                    attempt = 0;
                }
                match resume_session(&client, &session_id).await? {
                    Some((url, tok)) => (stream_url, token) = (url, tok),
                    // An empty ResumeSession means the session genuinely ended
                    // (e.g. a normal logout that closed the WebSocket) — exit
                    // cleanly rather than reporting an error.
                    None => return Ok(()),
                }
                reconnect_backoff(&mut attempt).await?;
            }
        }
    }
}

/// Fetch a fresh stream URL and token for an existing session via
/// `ssm:ResumeSession`. `Ok(None)` means the session has ended (empty response).
async fn resume_session(
    client: &aws_sdk_ssm::Client,
    session_id: &str,
) -> Result<Option<(String, String)>> {
    let resumed = client
        .resume_session()
        .session_id(session_id)
        .send()
        .await
        .context("ssm ResumeSession failed")?;
    let stream_url = resumed.stream_url().unwrap_or_default();
    let token = resumed.token_value().unwrap_or_default();
    if stream_url.is_empty() || token.is_empty() {
        return Ok(None);
    }
    Ok(Some((stream_url.to_string(), token.to_string())))
}

/// Sleep with exponential backoff before the next reconnect; error out once the
/// attempt budget is exhausted.
async fn reconnect_backoff(attempt: &mut u32) -> Result<()> {
    *attempt = attempt.saturating_add(1);
    if *attempt > MAX_RECONNECT_ATTEMPTS {
        bail!("gave up after {MAX_RECONNECT_ATTEMPTS} reconnect attempts");
    }
    let shift = attempt.saturating_sub(1).min(5);
    let delay = RECONNECT_INITIAL_DELAY
        .saturating_mul(2u32.saturating_pow(shift))
        .min(RECONNECT_MAX_DELAY);
    tokio::time::sleep(delay).await;
    Ok(())
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
