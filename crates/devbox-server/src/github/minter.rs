//! Mint short-lived, **repo-scoped** GitHub App installation tokens.
//!
//! This is the off-box half of the credential model: the GitHub App private key
//! (PEM) lives only on the control plane, read from an **SSM SecureString** via
//! the task role. A devbox host requests a token for a git remote over the agent
//! API; the server signs a short App JWT, discovers the installation covering the
//! repo, and exchanges the JWT for a `metadata:read` token with `contents:read`
//! (fetch) or `contents:write` (push) **scoped to that one repository** (least
//! privilege — a leaked 1h token covers one repo, not the whole installation). The
//! PEM never leaves the server.
//!
//! The installation is **discovered per repository**, not configured: for each
//! repo the server reads `owner/repo` from the remote and asks GitHub which
//! installation covers it (`GET /repos/{owner}/{repo}/installation`). One App
//! therefore serves **any** org that installed it — there are no installation IDs
//! to track, and the App installation is the sole repo authorization boundary
//! (an un-installed repo 404s at GitHub).
//!
//! Configuration is non-secret and supplied via the environment:
//!
//! - `DEVBOX_GITHUB_APP_ID` — the App ID or Client ID (the JWT issuer).
//! - `DEVBOX_GITHUB_KEY_PARAM` — SSM SecureString parameter holding the RSA PEM.
//! - `DEVBOX_GITHUB_API_BASE` — optional; defaults to `https://api.github.com`.
//!
//! When `DEVBOX_GITHUB_APP_ID` / `DEVBOX_GITHUB_KEY_PARAM` are unset the minter is
//! absent ([`Minter::from_env`] returns `Ok(None)`), so a local/dev server boots
//! without AWS and the git-token endpoint reports that minting is unconfigured.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_config::SdkConfig;
use devbox_common::{GitHubRepository, env_non_empty};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use url::Url;

const APP_ID_ENV: &str = "DEVBOX_GITHUB_APP_ID";
const KEY_PARAM_ENV: &str = "DEVBOX_GITHUB_KEY_PARAM";
const API_BASE_ENV: &str = "DEVBOX_GITHUB_API_BASE";
const DEFAULT_API_BASE: &str = "https://api.github.com";

/// GitHub rejects App JWTs older than 10 min; stay well under it.
const JWT_TTL: Duration = Duration::from_secs(540);
/// Back-date `iat` to tolerate clock skew between the server and GitHub.
const JWT_BACKDATE: Duration = Duration::from_secs(60);
/// Per-request timeout for the GitHub API calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Connect timeout for the GitHub API calls.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the one-time SSM key read so a stalled `ssm:GetParameter` can't block
/// server start-up (this runs in `main` before the listener binds).
const KEY_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Serialize)]
struct TokenRequest {
    /// Repository names (within the installation account) the token is scoped to.
    repositories: Vec<String>,
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

/// Mints read-only, repo-scoped installation tokens, discovering the installation
/// per repo so a single App serves every org that installed it. Built once at
/// startup; the PEM-derived signing key is cached for the process lifetime.
///
/// Minting is gated on the remote's host matching `git_host` (the App's GitHub
/// host), so a non-GitHub `origin` never drives a lookup and a GitHub token is
/// never handed to another host's fetch.
pub struct Minter {
    client: reqwest::Client,
    api_base: String,
    issuer: String,
    git_host: String,
    key: EncodingKey,
}

impl Minter {
    /// Build a minter from the environment, or `Ok(None)` when the server is not
    /// configured for GitHub App auth (`DEVBOX_GITHUB_APP_ID` /
    /// `DEVBOX_GITHUB_KEY_PARAM` unset) so local/dev servers boot without AWS.
    ///
    /// `aws_config` is the already-loaded SDK config (region from the task
    /// environment); it builds the SSM client used for the one-time PEM read.
    ///
    /// # Errors
    ///
    /// Returns an error when the server is configured but the SSM key read, PEM
    /// parsing, or HTTP client construction fails.
    pub async fn from_env(aws_config: &SdkConfig) -> Result<Option<Self>> {
        let (Some(issuer), Some(key_param)) =
            (env_non_empty(APP_ID_ENV), env_non_empty(KEY_PARAM_ENV))
        else {
            return Ok(None);
        };
        let api_base = env_non_empty(API_BASE_ENV).unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let pem = tokio::time::timeout(KEY_READ_TIMEOUT, read_key(aws_config, &key_param))
            .await
            .context("GitHub App key read from SSM timed out")??;
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .context("parse GitHub App private key (expected an RSA PEM)")?;
        let client = reqwest::Client::builder()
            .user_agent("devbox-server")
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("build GitHub HTTP client")?;
        let git_host = git_host_from_api_base(&api_base);
        Ok(Some(Self {
            client,
            api_base,
            issuer,
            git_host,
            key,
        }))
    }

    /// Mint a repo-scoped, read-only installation token for `remote`.
    ///
    /// Returns `Ok(None)` when `remote` is not a repository on the App's GitHub
    /// host — the caller then fetches unauthenticated, matching the prior on-box
    /// behavior. Returns an error when the host matches but the App is not
    /// installed on the repo (GitHub 404) or an API call fails.
    ///
    /// # Errors
    ///
    /// Propagates GitHub API failures (installation lookup, token exchange) and
    /// JWT signing errors.
    pub async fn mint_for_remote(
        &self,
        remote: &str,
    ) -> Result<Option<(GitHubRepository, String)>> {
        let Some(parsed) = parse_remote(remote) else {
            return Ok(None);
        };
        if parsed.host != self.git_host {
            return Ok(None);
        }
        let jwt = sign_jwt(&self.issuer, &self.key)?;
        let installation_id = self
            .resolve_installation(&jwt, &parsed.owner, &parsed.repo)
            .await?;
        let token = self
            .exchange(&jwt, installation_id, &parsed.repo, false)
            .await?;
        Ok(Some((
            GitHubRepository {
                owner: parsed.owner,
                repo: parsed.repo,
            },
            token,
        )))
    }

    /// The base URL of the git host this App serves (e.g. `https://github.com`),
    /// used by the git reverse proxy to build the upstream URL.
    pub(crate) fn git_base(&self) -> String {
        format!("https://{}", self.git_host)
    }

    /// Mint a token scoped to `owner/repo`, discovering the installation per repo.
    /// `write` requests `contents:write` (push); otherwise `contents:read`. Like
    /// [`Self::mint_for_remote`] but for an already-split repository (no remote URL
    /// to parse or host to gate).
    ///
    /// # Errors
    ///
    /// Propagates JWT signing, installation-lookup, and token-exchange failures
    /// (including a 404 when the App is not installed on the repo).
    pub(crate) async fn mint(&self, owner: &str, repo: &str, write: bool) -> Result<String> {
        // The REST API rejects git's `repo.git` remote spelling, which the git
        // reverse proxy forwards verbatim (remote-URL callers come pre-stripped
        // via `parse_remote`).
        let repo = bare_repo(repo);
        let jwt = sign_jwt(&self.issuer, &self.key)?;
        let installation_id = self.resolve_installation(&jwt, owner, repo).await?;
        self.exchange(&jwt, installation_id, repo, write).await
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

    /// Exchange the App JWT for an installation token on `installation_id`, scoped to
    /// the single repository `repo`. `write` grants `contents:write` (push);
    /// otherwise `contents:read`.
    async fn exchange(
        &self,
        jwt: &str,
        installation_id: u64,
        repo: &str,
        write: bool,
    ) -> Result<String> {
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
                repositories: vec![repo.to_string()],
                permissions: Permissions {
                    contents: if write { "write" } else { "read" },
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

/// Read the GitHub App private key (PEM) from an SSM SecureString parameter.
///
/// Uses the already-loaded SDK config (the task role on ECS, region from the task
/// environment), so no IMDS region lookup is needed as on the host.
async fn read_key(aws_config: &SdkConfig, param: &str) -> Result<String> {
    let ssm = aws_sdk_ssm::Client::new(aws_config);
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
/// signed fresh per mint to sidestep the 10-minute App-JWT expiry.
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
    // Hostnames are case-insensitive, but the `url` crate only lowercases hosts for
    // special schemes (https), not ssh — so an scp-like remote like
    // `git@GitHub.com:owner/repo` keeps its uppercase host. Lowercase it here so the
    // comparison against the (already-lowercased) App host in `mint_for_remote`
    // matches.
    let host = url.host_str()?.to_lowercase();
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    let owner = segments.next()?.to_string();
    let repo = bare_repo(segments.next()?).to_string();
    Some(RemoteRef { host, owner, repo })
}

/// The GitHub repository name for a repo path segment: strips the `.git` suffix
/// git remotes conventionally carry, which the REST API does not accept.
fn bare_repo(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
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
    use super::{bare_repo, git_host_from_api_base, parse_remote};

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
    fn bare_repo_strips_git_suffix() {
        // The git reverse proxy hands `mint` the raw path segment from the
        // rewritten remote, so both spellings must resolve to the same repo.
        assert_eq!(bare_repo("devbox.git"), "devbox");
        assert_eq!(bare_repo("devbox"), "devbox");
        assert_eq!(bare_repo("devbox.git.git"), "devbox.git");
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
    fn parses_scp_like_form() {
        assert_eq!(
            parsed("git@github.com:smoketurner/devbox.git"),
            github("smoketurner", "devbox")
        );
    }

    #[test]
    fn scp_like_host_is_lowercased() {
        // git preserves the host case verbatim and the `url` crate does not
        // lowercase ssh hosts, so an uppercase scp-like remote must be normalized
        // here to match the App's lowercased git host (else minting is skipped).
        assert_eq!(
            parsed("git@GitHub.com:smoketurner/devbox.git"),
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
        // so `mint_for_remote` returns None rather than minting.
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
