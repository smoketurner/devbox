//! `devbox ssm-proxy`: a native SSM Session Manager tunnel used as an ssh
//! `ProxyCommand`, replacing the external `session-manager-plugin`.
//!
//! `ssh` invokes this as a ProxyCommand (substituting `%h`/`%p`). We call
//! `ssm:StartSession` for the `AWS-StartSSHSession` document, open the returned
//! WebSocket data channel, and pipe the ssh client's stdin/stdout through it.
//! stdout carries the SSH transport, so **all logging goes to stderr**.

mod channel;
mod message;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use aws_credential_types::provider::ProvideCredentials;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Cap on a single inbound WebSocket message; SSH frames are tiny, so this
/// bounds the memory a malicious agent can force the client to buffer.
const MAX_WS_MESSAGE_SIZE: usize = 256 * 1024;
/// Times an empty `ResumeSession` is retried before concluding the session has
/// genuinely ended — guards a live session against a transient empty response.
const EMPTY_RESUME_RETRIES: u32 = 3;
/// Delay between empty-`ResumeSession` retries.
#[cfg(not(test))]
const EMPTY_RESUME_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const EMPTY_RESUME_DELAY: Duration = Duration::from_millis(1);
/// Maximum consecutive reconnect attempts before giving up on a session.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;
/// Base delay for reconnect backoff.
#[cfg(not(test))]
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Tiny base delay under test so the budget path is cheap to exercise.
#[cfg(test)]
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(1);
/// Cap on the reconnect backoff delay.
#[cfg(not(test))]
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// Tiny cap under test.
#[cfg(test)]
const RECONNECT_MAX_DELAY: Duration = Duration::from_millis(2);
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
    // Pin the WebSocket TLS to the aws-lc-rs provider explicitly, rather than
    // relying on the ambient process-default crypto provider.
    let connector = aws_lc_rs_connector()?;

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

    // Buffer stdout so a burst of small agent frames coalesces into few writes.
    let mut state = channel::SessionState::new(tokio::io::BufWriter::new(tokio::io::stdout()));
    let mut input = tokio::io::stdin();
    let mut attempt: u32 = 0;
    let mut established = false;

    loop {
        if !stream_url.starts_with("wss://") {
            bail!("refusing to open a non-TLS SSM stream URL");
        }
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(MAX_WS_MESSAGE_SIZE);
        ws_config.max_frame_size = Some(MAX_WS_MESSAGE_SIZE);
        let connect = tokio_tungstenite::connect_async_tls_with_config(
            stream_url.as_str(),
            Some(ws_config),
            false,
            Some(connector.clone()),
        )
        .await;
        let ws = match connect {
            Ok((ws, _response)) => ws,
            Err(e) => {
                eprintln!("devbox ssm-proxy: failed to open data channel: {e}");
                match resume_session(&client, &session_id).await? {
                    Some((url, tok)) => (stream_url, token) = (url, tok),
                    // An empty ResumeSession after a working connection means the
                    // session genuinely ended — exit cleanly. But if no data
                    // channel ever opened, there is no tunnel: fail rather than
                    // exiting zero, which would mask a broken `devbox ssh`.
                    None if established => return Ok(()),
                    None => bail!(
                        "could not open the SSM data channel and the session could \
                         not be resumed: {e}"
                    ),
                }
                reconnect_backoff(&mut attempt).await?;
                continue;
            }
        };

        established = true;
        let started = Instant::now();
        match channel::run_connection(ws, &token, &mut state, &mut input).await? {
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

/// Build a tokio-tungstenite TLS connector pinned to the aws-lc-rs rustls
/// provider and the webpki root store, so the data channel never depends on the
/// ambient process-default crypto provider.
fn aws_lc_rs_connector() -> Result<tokio_tungstenite::Connector> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configure rustls protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// Fetch a fresh stream URL and token for an existing session via
/// `ssm:ResumeSession`. An empty response is retried a bounded number of times
/// (it can be transient); `Ok(None)` is returned only if it stays empty, meaning
/// the session has genuinely ended.
async fn resume_session(
    client: &aws_sdk_ssm::Client,
    session_id: &str,
) -> Result<Option<(String, String)>> {
    for _ in 0..EMPTY_RESUME_RETRIES {
        let resumed = client
            .resume_session()
            .session_id(session_id)
            .send()
            .await
            .context("ssm ResumeSession failed")?;
        let stream_url = resumed.stream_url().unwrap_or_default();
        let token = resumed.token_value().unwrap_or_default();
        if !stream_url.is_empty() && !token.is_empty() {
            return Ok(Some((stream_url.to_string(), token.to_string())));
        }
        tokio::time::sleep(EMPTY_RESUME_DELAY).await;
    }
    Ok(None)
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
        bail!(
            "no AWS credentials are configured for the SSM tunnel; \
             pass `--profile <name>`, set `AWS_PROFILE`, or refresh your AWS login"
        );
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconnect_backoff_exhausts_budget() {
        let mut attempt = 0u32;
        for _ in 0..MAX_RECONNECT_ATTEMPTS {
            reconnect_backoff(&mut attempt)
                .await
                .expect("within budget");
        }
        // The next attempt exceeds the budget and must fail rather than loop.
        assert!(reconnect_backoff(&mut attempt).await.is_err());
    }
}
