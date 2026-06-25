//! Mint short-lived, read-only GitHub App installation tokens at warm-up.
//!
//! The warming fetch needs a credential, but an installation token lives only an
//! hour, so it can't be baked into the AMI or an env var. Instead the agent reads
//! the GitHub App private key from an **SSM SecureString** parameter (via the host
//! instance profile — no static secret on the box), signs a short App JWT, and
//! exchanges it for a fresh `contents:read` installation token. The token is used
//! only for the fetch and never persisted.
//!
//! The installation is **discovered per repository**, not configured: for each
//! repo the agent reads its `origin` remote, derives `owner/repo`, and asks GitHub
//! which installation covers it (`GET /repos/{owner}/{repo}/installation`). One App
//! therefore freshens repos in **any** org that has installed it — there are no
//! installation IDs to track. Tokens are cached per owner (N repos in one org cost
//! one discovery and one mint).
//!
//! Configuration is non-secret and supplied via the environment (set by the
//! systemd unit / instance metadata), so an unconfigured box simply skips minting
//! and fetches unauthenticated:
//!
//! - `DEVBOX_GITHUB_APP_ID` — the App ID or Client ID (the JWT issuer).
//! - `DEVBOX_GITHUB_KEY_PARAM` — SSM SecureString parameter holding the RSA PEM.
//! - `DEVBOX_GITHUB_API_BASE` — optional; defaults to `https://api.github.com`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_ssm::config::Region;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use url::Url;

const APP_ID_ENV: &str = "DEVBOX_GITHUB_APP_ID";
const KEY_PARAM_ENV: &str = "DEVBOX_GITHUB_KEY_PARAM";
const API_BASE_ENV: &str = "DEVBOX_GITHUB_API_BASE";
const DEFAULT_API_BASE: &str = "https://api.github.com";

/// GitHub rejects App JWTs older than 10 min; stay well under it.
const JWT_TTL: Duration = Duration::from_secs(540);
/// Back-date `iat` to tolerate clock skew between the box and GitHub.
const JWT_BACKDATE: Duration = Duration::from_secs(60);

/// Bound on the one-time SSM key read so a stall can't block warm-up.
const KEY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-request timeout for the GitHub API calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Connect timeout for the GitHub API calls.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

/// Non-secret minting configuration, or `None` when the box is not set up for
/// GitHub App auth (the caller then fetches unauthenticated).
struct Config {
    issuer: String,
    key_param: String,
    api_base: String,
}

/// Mints read-only installation tokens, discovering the installation per repo so a
/// single App serves every org that installed it. Built once per warm-up; caches a
/// token per owner.
///
/// Minting is gated on the remote's host matching `git_host` (the App's GitHub
/// host), so a non-GitHub `origin` never drives a lookup and a GitHub token is never
/// handed to another host's fetch.
pub(crate) struct TokenMinter {
    client: reqwest::Client,
    api_base: String,
    issuer: String,
    git_host: String,
    key: EncodingKey,
    cache: HashMap<String, String>,
}

impl TokenMinter {
    /// Build a minter, or `Ok(None)` when the box is not configured for GitHub App
    /// auth.
    ///
    /// # Errors
    ///
    /// Returns an error if the box is configured but the SSM key fetch, key
    /// parsing, or HTTP client construction fails.
    pub(crate) async fn new(region: &str) -> Result<Option<Self>> {
        let Some(cfg) = config() else {
            return Ok(None);
        };
        let pem = tokio::time::timeout(KEY_READ_TIMEOUT, read_key(region, &cfg.key_param))
            .await
            .context("GitHub App key read timed out")??;
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .context("parse GitHub App private key (expected an RSA PEM)")?;
        let client = reqwest::Client::builder()
            .user_agent("devbox-agent")
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("build HTTP client")?;
        let git_host = git_host_from_api_base(&cfg.api_base);
        Ok(Some(Self {
            client,
            api_base: cfg.api_base,
            issuer: cfg.issuer,
            git_host,
            key,
            cache: HashMap::new(),
        }))
    }

    /// A read-only installation token for `remote`'s owner, cached per owner, or
    /// `Ok(None)` when `remote` is not a repo on the App's GitHub host (the caller
    /// then fetches unauthenticated).
    ///
    /// # Errors
    ///
    /// Returns an error if the App is not installed on the owner (or the repo is not
    /// granted to it) or a GitHub API call fails.
    pub(crate) async fn token_for(&mut self, remote: &str) -> Result<Option<String>> {
        let Some(parsed) = parse_remote(remote) else {
            return Ok(None);
        };
        if parsed.host != self.git_host {
            return Ok(None);
        }
        if let Some(token) = self.cache.get(&parsed.owner) {
            return Ok(Some(token.clone()));
        }
        let jwt = sign_jwt(&self.issuer, &self.key)?;
        let installation_id = self
            .resolve_installation(&jwt, &parsed.owner, &parsed.repo)
            .await?;
        let token = self.exchange(&jwt, installation_id).await?;
        self.cache.insert(parsed.owner, token.clone());
        Ok(Some(token))
    }

    /// Resolve the installation id covering `owner/repo` via the App JWT.
    async fn resolve_installation(&self, jwt: &str, owner: &str, repo: &str) -> Result<u64> {
        let url = format!(
            "{}/repos/{owner}/{repo}/installation",
            self.api_base.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GitHub installation lookup for {owner}/{repo} failed ({status}): {body}"
            );
        }
        Ok(resp
            .json::<Installation>()
            .await
            .context("parse installation response")?
            .id)
    }

    /// Exchange the App JWT for a `contents:read` token on `installation_id`.
    async fn exchange(&self, jwt: &str, installation_id: u64) -> Result<String> {
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base.trim_end_matches('/')
        );
        let resp = self
            .client
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
}

/// Read minting configuration from the environment; `None` if any required value
/// is missing or empty.
fn config() -> Option<Config> {
    Some(Config {
        issuer: non_empty(APP_ID_ENV)?,
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

/// Sign a GitHub App JWT (RS256; jsonwebtoken on the aws-lc-rs backend). Cheap, so
/// signed fresh per mint to sidestep the 10-minute App-JWT expiry on long runs.
fn sign_jwt(issuer: &str, key: &EncodingKey) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the unix epoch")?
        .as_secs();
    let claims = Claims {
        iat: now.saturating_sub(JWT_BACKDATE.as_secs()),
        exp: now.saturating_add(JWT_TTL.as_secs()),
        iss: issuer.to_string(),
    };
    encode(&Header::new(Algorithm::RS256), &claims, key).context("sign GitHub App JWT")
}

/// A git remote decomposed into the pieces needed to mint a token: the `host` (so
/// minting can be gated on the App's GitHub host) and the `owner`/`repo`.
struct RemoteRef {
    host: String,
    owner: String,
    repo: String,
}

/// Parse `host`, `owner`, and `repo` from a git remote URL.
///
/// Parses the standard `scheme://[user@]host[:port]/owner/repo[.git]` forms with a
/// real URL parser, after normalizing git's scp-like `user@host:owner/repo[.git]`
/// shorthand (which is not a valid URL) into an `ssh://` URL. Returns `None` for a
/// remote with no host or no `owner/repo` path (e.g. a local path).
fn parse_remote(remote: &str) -> Option<RemoteRef> {
    let url = parse_git_url(remote.trim())?;
    let host = url.host_str()?.to_string();
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    let owner = segments.next()?.to_string();
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo).to_string();
    Some(RemoteRef { host, owner, repo })
}

/// The git host whose remotes this App serves, derived from the API base: public
/// GitHub's API lives at `api.github.com` but its remotes at `github.com`, while a
/// GHES install shares one host for both (`https://HOST/api/v3` ↔ `HOST`).
fn git_host_from_api_base(api_base: &str) -> String {
    match Url::parse(api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .as_deref()
    {
        Some("api.github.com") => "github.com".to_string(),
        Some(host) => host.to_string(),
        None => "github.com".to_string(),
    }
}

/// Parse a git remote into a [`Url`], first normalizing git's scp-like shorthand
/// (`[user@]host:owner/repo`) into `ssh://[user@]host/owner/repo`. Per git, a remote
/// is scp-like only when the first colon precedes the first slash; anything else with
/// no `://` (e.g. a local path) is rejected.
fn parse_git_url(remote: &str) -> Option<Url> {
    if remote.contains("://") {
        return Url::parse(remote).ok();
    }
    let (authority, path) = remote.split_once(':')?;
    if authority.is_empty() || authority.contains('/') {
        return None;
    }
    Url::parse(&format!("ssh://{authority}/{path}")).ok()
}

#[cfg(test)]
mod tests {
    use super::{git_host_from_api_base, parse_remote};

    /// `(host, owner, repo)` for a remote, or `None`.
    fn parsed(remote: &str) -> Option<(String, String, String)> {
        parse_remote(remote).map(|r| (r.host, r.owner, r.repo))
    }

    fn github(owner: &str, repo: &str) -> Option<(String, String, String)> {
        Some((
            "github.com".to_string(),
            owner.to_string(),
            repo.to_string(),
        ))
    }

    #[test]
    fn parses_https_with_git_suffix() {
        assert_eq!(
            parsed("https://github.com/smoketurner/devbox.git"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn parses_https_without_git_suffix() {
        assert_eq!(
            parsed("https://github.com/smoketurner/devbox"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn parses_https_with_trailing_slash() {
        assert_eq!(
            parsed("https://github.com/smoketurner/devbox/"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn parses_scp_like_form() {
        assert_eq!(
            parsed("git@github.com:smoketurner/devbox.git"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn parses_ssh_scheme_with_user() {
        assert_eq!(
            parsed("ssh://git@github.com/smoketurner/devbox.git"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn parses_ssh_scheme_with_port() {
        assert_eq!(
            parsed("ssh://git@github.com:22/smoketurner/devbox.git"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn query_string_does_not_leak_into_repo() {
        assert_eq!(
            parsed("https://github.com/smoketurner/devbox.git?ref=main"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn captures_non_github_host_so_minting_can_be_gated() {
        // A non-GitHub remote parses, but its host won't match the App's git host,
        // so `token_for` returns it unauthenticated rather than minting.
        assert_eq!(
            parsed("https://gitlab.com/smoketurner/devbox.git"),
            Some((
                "gitlab.com".to_string(),
                "smoketurner".to_string(),
                "devbox".to_string()
            ))
        );
    }

    #[test]
    fn rejects_url_without_owner_repo() {
        assert_eq!(parsed("https://github.com/"), None);
        assert_eq!(parsed("https://github.com/owner-only"), None);
    }

    #[test]
    fn rejects_non_url_path() {
        assert_eq!(parsed("smoketurner/devbox"), None);
        assert_eq!(parsed(""), None);
    }

    #[test]
    fn git_host_maps_public_api_to_github_com() {
        assert_eq!(
            git_host_from_api_base("https://api.github.com"),
            "github.com"
        );
    }

    #[test]
    fn git_host_uses_ghes_host_directly() {
        assert_eq!(
            git_host_from_api_base("https://ghe.example.com/api/v3"),
            "ghe.example.com"
        );
    }

    #[test]
    fn git_host_falls_back_to_github_com() {
        assert_eq!(git_host_from_api_base("not-a-url"), "github.com");
    }
}
