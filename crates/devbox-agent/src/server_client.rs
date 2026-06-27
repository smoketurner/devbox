//! Obtain GitHub tokens from the devbox-server control plane.
//!
//! The GitHub App private key no longer lives on the box. Instead the agent
//! authenticates to devbox-server with an **AWS web-identity token** (IAM
//! Outbound Identity Federation, `sts:GetWebIdentityToken`) — a short-lived,
//! AWS-signed OIDC JWT asserting this instance's identity, with no static secret
//! to steal — and asks the server to mint a short-lived, **repo-scoped**,
//! read-only GitHub token per git remote. The server holds the key and mints;
//! the box only ever holds a 1 h scoped token.
//!
//! Configuration is non-secret and supplied via the environment:
//!
//! - `DEVBOX_SERVER_URL` — the control-plane base URL. Also the **audience** the
//!   web-identity token is minted for; it must equal the server's
//!   `DEVBOX_AGENT_AUDIENCE` (trailing slashes are trimmed on both sides). When
//!   unset the agent is not configured for
//!   server-backed minting ([`ServerTokenClient::new`] returns `Ok(None)`) and
//!   callers fetch unauthenticated.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_sts::config::Region;
use devbox_common::{GitTokenRequest, GitTokenResponse, env_non_empty};

const SERVER_URL_ENV: &str = "DEVBOX_SERVER_URL";

/// Signing algorithm requested from `GetWebIdentityToken` (the AWS issuer
/// advertises only `RS256`/`ES384`; the server's `token_algorithm` accepts ES384).
const SIGNING_ALGORITHM: &str = "ES384";

/// Web-identity token lifetime requested from STS, in seconds (range 60–3600).
const WEB_IDENTITY_TTL_SECS: i32 = 900;

/// Refresh a cached web-identity token this long before its expiry, to tolerate
/// clock skew and request latency.
const REFRESH_SKEW_SECS: u64 = 60;

/// Per-request timeout for the control-plane HTTP calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Connect timeout for the control-plane HTTP calls.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the STS token call so a stall can't block warm-up.
const STS_TIMEOUT: Duration = Duration::from_secs(20);

/// Fetches short-lived, repo-scoped GitHub tokens from devbox-server, presenting
/// an AWS web-identity token for auth. Built once per run; caches the web-identity
/// JWT (refreshed near expiry) and one GitHub token per git remote.
///
/// GitHub tokens are cached **per remote**, not per owner: each is scoped to a
/// single repository, so it cannot be reused across remotes.
pub(crate) struct ServerTokenClient {
    http: reqwest::Client,
    sts: aws_sdk_sts::Client,
    git_token_url: String,
    /// The audience the web-identity token is minted for (the control-plane
    /// resource); equals the server's expected `DEVBOX_AGENT_AUDIENCE`.
    audience: String,
    /// Cached web-identity JWT and its unix expiry (seconds).
    web_identity: Option<(String, u64)>,
    /// Cached repo-scoped GitHub tokens, keyed by git remote URL.
    cache: HashMap<String, String>,
}

impl ServerTokenClient {
    /// Build a client, or `Ok(None)` when the box is not configured for
    /// server-backed minting (`DEVBOX_SERVER_URL` unset).
    ///
    /// # Errors
    ///
    /// Returns an error when the box is configured but the region cannot be read
    /// from IMDS (needed to bind the STS client) or the HTTP client cannot build.
    pub(crate) async fn new() -> Result<Option<Self>> {
        let Some(server_url) = env_non_empty(SERVER_URL_ENV) else {
            return Ok(None);
        };
        // Resolve the region from IMDS so the STS client is bound even when
        // AWS_REGION is unset (the SDK default chain has no IMDS region fallback).
        let region = crate::imds::region()
            .await
            .context("read region from IMDS for the STS client")?;
        let config = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region))
            .load()
            .await;
        let sts = aws_sdk_sts::Client::new(&config);
        let http = reqwest::Client::builder()
            .user_agent("devbox-agent")
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("build control-plane HTTP client")?;
        let base = server_url.trim_end_matches('/').to_string();
        let git_token_url = format!("{base}/api/v1/agent/git-token");
        Ok(Some(Self {
            http,
            sts,
            git_token_url,
            audience: base,
            web_identity: None,
            cache: HashMap::new(),
        }))
    }

    /// A read-only, repo-scoped token for `remote`, cached per remote, or
    /// `Ok(None)` when the server reports `remote` is not a repo on the App's
    /// GitHub host (the caller then fetches unauthenticated).
    ///
    /// # Errors
    ///
    /// Returns an error when the web-identity mint fails or the control-plane
    /// request fails (including when the App is not installed on the repo, which
    /// the server surfaces as an error).
    pub(crate) async fn token_for(&mut self, remote: &str) -> Result<Option<String>> {
        if let Some(token) = self.cache.get(remote) {
            return Ok(Some(token.clone()));
        }
        let jwt = self.web_identity_token().await?;
        let resp = self
            .http
            .post(&self.git_token_url)
            .bearer_auth(&jwt)
            .json(&GitTokenRequest {
                remote: remote.to_string(),
            })
            .send()
            .await
            .with_context(|| format!("POST {}", self.git_token_url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("git-token request failed ({status}): {body}");
        }
        let parsed: GitTokenResponse = resp.json().await.context("parse git-token response")?;
        match parsed.token {
            Some(token) => {
                self.cache.insert(remote.to_string(), token.clone());
                Ok(Some(token))
            }
            None => Ok(None),
        }
    }

    /// A valid web-identity JWT, minting a fresh one via STS when none is cached or
    /// the cached one is within [`REFRESH_SKEW_SECS`] of expiry.
    async fn web_identity_token(&mut self) -> Result<String> {
        let now = unix_now()?;
        if let Some((token, exp)) = &self.web_identity
            && *exp > now.saturating_add(REFRESH_SKEW_SECS)
        {
            return Ok(token.clone());
        }
        let (token, exp) = self.fetch_web_identity(now).await?;
        self.web_identity = Some((token.clone(), exp));
        Ok(token)
    }

    /// Mint a fresh web-identity token from STS, returning it with its unix expiry.
    async fn fetch_web_identity(&self, now: u64) -> Result<(String, u64)> {
        let out = tokio::time::timeout(
            STS_TIMEOUT,
            self.sts
                .get_web_identity_token()
                .audience(&self.audience)
                .signing_algorithm(SIGNING_ALGORITHM)
                .duration_seconds(WEB_IDENTITY_TTL_SECS)
                .send(),
        )
        .await
        .context("sts:GetWebIdentityToken timed out")?
        .context("sts:GetWebIdentityToken")?;

        let token = out
            .web_identity_token()
            .map(str::to_string)
            .context("GetWebIdentityToken response had no token")?;
        // Prefer the server-asserted expiration; fall back to the requested TTL.
        let exp = out
            .expiration()
            .and_then(|ts| u64::try_from(ts.secs()).ok())
            .unwrap_or_else(|| now.saturating_add(default_ttl_secs()));
        Ok((token, exp))
    }
}

/// The requested TTL as `u64` seconds (for the fallback expiry computation).
fn default_ttl_secs() -> u64 {
    u64::try_from(WEB_IDENTITY_TTL_SECS).unwrap_or(900)
}

/// Current unix time in seconds.
fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::default_ttl_secs;

    #[test]
    fn default_ttl_matches_requested_lifetime() {
        // The u64 fallback used for expiry math must equal the i32 TTL requested
        // from STS, so a missing `expiration` doesn't over- or under-state expiry.
        assert_eq!(default_ttl_secs(), 900);
    }
}
