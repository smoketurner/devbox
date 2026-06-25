//! Mint a short-lived, read-only GitHub App installation token at warm-up.
//!
//! The warming fetch needs a credential, but an installation token lives only an
//! hour, so it can't be baked into the AMI or an env var. Instead the agent reads
//! the GitHub App private key from an **SSM SecureString** parameter (via the host
//! instance profile — no static secret on the box), signs a short JWT, and
//! exchanges it for a fresh `contents:read` installation token. The token is used
//! only for the fetch and never persisted.
//!
//! Configuration is non-secret and supplied via the environment (set by the
//! systemd unit / instance metadata), so an unconfigured box simply skips minting
//! and fetches unauthenticated:
//!
//! - `DEVBOX_GITHUB_APP_ID` — the App ID or Client ID (the JWT issuer).
//! - `DEVBOX_GITHUB_INSTALLATION_ID` — the installation to mint against.
//! - `DEVBOX_GITHUB_KEY_PARAM` — SSM SecureString parameter holding the RSA PEM.
//! - `DEVBOX_GITHUB_API_BASE` — optional; defaults to `https://api.github.com`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_ssm::config::Region;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

const APP_ID_ENV: &str = "DEVBOX_GITHUB_APP_ID";
const INSTALLATION_ENV: &str = "DEVBOX_GITHUB_INSTALLATION_ID";
const KEY_PARAM_ENV: &str = "DEVBOX_GITHUB_KEY_PARAM";
const API_BASE_ENV: &str = "DEVBOX_GITHUB_API_BASE";
const DEFAULT_API_BASE: &str = "https://api.github.com";

/// GitHub rejects App JWTs older than 10 min; stay well under it.
const JWT_TTL: Duration = Duration::from_secs(540);
/// Back-date `iat` to tolerate clock skew between the box and GitHub.
const JWT_BACKDATE: Duration = Duration::from_secs(60);

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Serialize)]
struct TokenRequest {
    permissions: Permissions,
}

#[derive(Serialize)]
struct Permissions {
    contents: &'static str,
    metadata: &'static str,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

/// Non-secret minting configuration, or `None` when the box is not set up for
/// GitHub App auth (the caller then fetches unauthenticated).
struct Config {
    issuer: String,
    installation_id: String,
    key_param: String,
    api_base: String,
}

/// Mint a read-only installation token, or `Ok(None)` when the box is not
/// configured for GitHub App auth.
///
/// # Errors
///
/// Returns an error if the box is configured but the SSM key fetch, JWT signing,
/// or token exchange fails.
pub(crate) async fn installation_token(region: &str) -> Result<Option<String>> {
    let Some(cfg) = config() else {
        return Ok(None);
    };
    let pem = read_key(region, &cfg.key_param).await?;
    let jwt = sign_jwt(&cfg.issuer, &pem)?;
    let token = exchange(&cfg, &jwt).await?;
    Ok(Some(token))
}

/// Read minting configuration from the environment; `None` if any required value
/// is missing or empty.
fn config() -> Option<Config> {
    Some(Config {
        issuer: non_empty(APP_ID_ENV)?,
        installation_id: non_empty(INSTALLATION_ENV)?,
        key_param: non_empty(KEY_PARAM_ENV)?,
        api_base: non_empty(API_BASE_ENV).unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
    })
}

/// Trimmed value of env var `key`, or `None` if unset or blank.
fn non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read the GitHub App private key (PEM) from an SSM SecureString parameter using
/// the host instance profile.
async fn read_key(region: &str, param: &str) -> Result<String> {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .load()
        .await;
    let ssm = aws_sdk_ssm::Client::new(&config);
    let out = ssm
        .get_parameter()
        .name(param)
        .with_decryption(true)
        .send()
        .await
        .with_context(|| format!("ssm:GetParameter {param}"))?;
    out.parameter()
        .and_then(aws_sdk_ssm::types::Parameter::value)
        .map(str::to_string)
        .with_context(|| format!("SSM parameter {param} has no value"))
}

/// Sign the GitHub App JWT (RS256; jsonwebtoken on the aws-lc-rs backend).
fn sign_jwt(issuer: &str, pem: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the unix epoch")?
        .as_secs();
    let claims = Claims {
        iat: now.saturating_sub(JWT_BACKDATE.as_secs()),
        exp: now.saturating_add(JWT_TTL.as_secs()),
        iss: issuer.to_string(),
    };
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .context("parse GitHub App private key (expected an RSA PEM)")?;
    encode(&Header::new(Algorithm::RS256), &claims, &key).context("sign GitHub App JWT")
}

/// Exchange the App JWT for a `contents:read` installation token.
async fn exchange(cfg: &Config, jwt: &str) -> Result<String> {
    let url = format!(
        "{}/app/installations/{}/access_tokens",
        cfg.api_base.trim_end_matches('/'),
        cfg.installation_id
    );
    let client = reqwest::Client::builder()
        .user_agent("devbox-agent")
        .build()
        .context("build HTTP client")?;
    let resp = client
        .post(&url)
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&TokenRequest {
            permissions: Permissions {
                contents: "read",
                metadata: "read",
            },
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub installation-token request failed ({status}): {body}");
    }
    Ok(resp
        .json::<TokenResponse>()
        .await
        .context("parse installation-token response")?
        .token)
}
