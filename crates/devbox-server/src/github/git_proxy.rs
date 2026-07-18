//! Git smart-HTTP reverse proxy.
//!
//! A box points its git client at the control plane (`/git/...`); the server
//! authenticates the box by its web-identity token (sent as git basic-auth) and
//! forwards the request to GitHub with a repo-scoped installation token injected, so
//! the box never holds the credential. Fetch is always allowed; push mints a
//! `contents:write` token and is gated on the box being claimed (see the caller).

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE,
    USER_AGENT,
};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::github::Minter;

/// Longest owner/repo path segment accepted.
const MAX_SEGMENT_LEN: usize = 100;

/// Upstream connect timeout. No total timeout — a fetch streams for as long as the
/// clone takes.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A recognized git smart-HTTP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitEndpoint {
    /// `GET .../info/refs?service=git-upload-pack` — fetch ref advertisement.
    AdvertiseFetch,
    /// `POST .../git-upload-pack` — fetch negotiation + packfile.
    Fetch,
    /// `GET .../info/refs?service=git-receive-pack` — push ref advertisement.
    AdvertisePush,
    /// `POST .../git-receive-pack` — push packfile.
    Push,
}

impl GitEndpoint {
    /// Whether this endpoint writes to the repository (push).
    fn is_write(self) -> bool {
        matches!(self, Self::AdvertisePush | Self::Push)
    }

    /// The upstream path and query under `owner/repo` for this endpoint.
    fn upstream_suffix(self) -> &'static str {
        match self {
            Self::AdvertiseFetch => "/info/refs?service=git-upload-pack",
            Self::Fetch => "/git-upload-pack",
            Self::AdvertisePush => "/info/refs?service=git-receive-pack",
            Self::Push => "/git-receive-pack",
        }
    }
}

/// A validated proxy target. Constructed only by [`authorize`], so a value attests
/// the request is a recognized git endpoint with safe path segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitTarget {
    owner: String,
    repo: String,
    endpoint: GitEndpoint,
}

/// Why a proxied git request was refused, mapped to an HTTP status by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitReject {
    /// Not a recognized git smart-HTTP endpoint → 404.
    NotFound(String),
    /// A malformed owner/repo segment → 400.
    BadRequest(String),
}

impl GitTarget {
    /// Whether this request writes to the repository (push).
    pub(crate) fn is_write(&self) -> bool {
        self.endpoint.is_write()
    }

    /// The upstream GitHub URL under `git_base` (e.g. `https://github.com`).
    fn upstream_url(&self, git_base: &str) -> String {
        format!(
            "{}/{}/{}{}",
            git_base.trim_end_matches('/'),
            self.owner,
            self.repo,
            self.endpoint.upstream_suffix()
        )
    }
}

/// Parse and authorize a proxied git request into a [`GitTarget`].
///
/// `rest` is the path tail after `/git/{owner}/{repo}/` (e.g. `info/refs` or
/// `git-upload-pack`); `query` is the raw query string. Fetch and push endpoints are
/// recognized; anything else is [`GitReject::NotFound`]. Owner/repo must be safe
/// single path segments (no traversal into another upstream path). Push
/// authorization (a claimed box) is enforced by the caller.
pub(crate) fn authorize(
    method: &Method,
    owner: &str,
    repo: &str,
    rest: &str,
    query: Option<&str>,
) -> Result<GitTarget, GitReject> {
    if !is_safe_segment(owner) {
        return Err(GitReject::BadRequest(format!(
            "invalid repository owner '{owner}'"
        )));
    }
    if !is_safe_segment(repo) {
        return Err(GitReject::BadRequest(format!(
            "invalid repository name '{repo}'"
        )));
    }

    let endpoint = match (method.as_str(), rest) {
        ("GET", "info/refs") => match service_param(query) {
            Some("git-upload-pack") => GitEndpoint::AdvertiseFetch,
            Some("git-receive-pack") => GitEndpoint::AdvertisePush,
            _ => {
                return Err(GitReject::NotFound(
                    "unsupported or missing git service".to_string(),
                ));
            }
        },
        ("POST", "git-upload-pack") => GitEndpoint::Fetch,
        ("POST", "git-receive-pack") => GitEndpoint::Push,
        _ => {
            return Err(GitReject::NotFound(
                "not a git smart-HTTP endpoint".to_string(),
            ));
        }
    };

    Ok(GitTarget {
        owner: owner.to_string(),
        repo: repo.to_string(),
        endpoint,
    })
}

/// The value of the `service` query parameter, if present.
fn service_param(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| kv.strip_prefix("service="))
}

/// Whether `segment` is a safe single owner/repo path segment: non-empty, bounded,
/// not `.`/`..`, and alphanumeric plus `-_.` (`.` allows a `repo.git` suffix; `/` is
/// excluded so a segment can't escape into another upstream path).
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_LEN
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Extract the git basic-auth password (the box's web-identity token) from an
/// `Authorization: Basic` header; the username is ignored. `None` for a missing,
/// non-Basic, or malformed header.
pub(crate) fn basic_auth_password(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let encoded = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_user, password) = decoded.split_once(':')?;
    let password = password.trim();
    (!password.is_empty()).then(|| password.to_string())
}

/// The `Authorization` header value injecting a GitHub installation token as git
/// basic-auth (`x-access-token:<token>`).
pub(crate) fn basic_auth_header(token: &str) -> String {
    let raw = format!("x-access-token:{token}");
    format!("Basic {}", STANDARD.encode(raw))
}

/// Whether an incoming request header is forwarded upstream. Excludes
/// `Authorization` (the proxy injects its own) and length/hop-by-hop headers; keeps
/// `git-protocol` so protocol v2 survives.
fn is_forwardable_request_header(name: &HeaderName) -> bool {
    name == ACCEPT
        || name == ACCEPT_ENCODING
        || name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == USER_AGENT
        || name.as_str() == "git-protocol"
}

/// Whether an upstream response header is forwarded back. Keeps content framing and
/// cache directives; drops length/transfer headers so axum re-frames the stream.
fn is_forwardable_response_header(name: &HeaderName) -> bool {
    name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == CACHE_CONTROL
        || name.as_str() == "expires"
        || name.as_str() == "pragma"
}

/// Reverse proxy for git traffic. Owns its own HTTP client (no total timeout, so
/// long clones stream) and the [`Minter`] used to inject a repo-scoped token per
/// request.
pub struct GitProxy {
    client: reqwest::Client,
    minter: Arc<Minter>,
}

impl GitProxy {
    /// Build a proxy over `minter`.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be constructed.
    pub fn new(minter: Arc<Minter>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("devbox-server-git-proxy")
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("build git-proxy HTTP client")?;
        Ok(Self { client, minter })
    }

    /// Mint a token scoped to `target` (write for push, read otherwise) and forward
    /// the request to GitHub, returning the upstream response to stream back.
    ///
    /// # Errors
    ///
    /// Propagates token-minting failures (including an App not installed on the
    /// repo) and upstream transport failures.
    pub(crate) async fn forward(
        &self,
        target: &GitTarget,
        method: &Method,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<reqwest::Response> {
        let token = self
            .minter
            .mint(&target.owner, &target.repo, target.is_write())
            .await
            .context("mint repo-scoped token for git proxy")?;

        let url = target.upstream_url(&self.minter.git_base());
        let mut builder = self
            .client
            .request(method.clone(), &url)
            .header(AUTHORIZATION, basic_auth_header(&token));

        for (name, value) in headers {
            if is_forwardable_request_header(name) {
                builder = builder.header(name.clone(), value.clone());
            }
        }
        if method == Method::POST {
            builder = builder.body(body);
        }

        builder
            .send()
            .await
            .with_context(|| format!("forward git request to {url}"))
    }
}

/// Convert an upstream GitHub response into a streaming axum response: the status,
/// the safelisted headers, and the body streamed (never buffered).
pub(crate) fn into_axum_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_forwardable_response_header(name) {
            builder = builder.header(name.clone(), value.clone());
        }
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn target(method: &str, rest: &str, query: Option<&str>) -> Result<GitTarget, GitReject> {
        let method = if method == "POST" {
            Method::POST
        } else {
            Method::GET
        };
        authorize(&method, "smoketurner", "devbox", rest, query)
    }

    #[test]
    fn fetch_advertisement_is_a_read() {
        let t = target("GET", "info/refs", Some("service=git-upload-pack")).unwrap();
        assert!(!t.is_write());
        assert_eq!(
            t.upstream_url("https://github.com"),
            "https://github.com/smoketurner/devbox/info/refs?service=git-upload-pack"
        );
    }

    #[test]
    fn fetch_upload_pack_is_a_read() {
        let t = target("POST", "git-upload-pack", None).unwrap();
        assert!(!t.is_write());
        assert_eq!(
            t.upstream_url("https://github.com/"),
            "https://github.com/smoketurner/devbox/git-upload-pack"
        );
    }

    #[test]
    fn push_advertisement_is_a_write() {
        let t = target("GET", "info/refs", Some("service=git-receive-pack")).unwrap();
        assert!(t.is_write());
        assert_eq!(
            t.upstream_url("https://github.com"),
            "https://github.com/smoketurner/devbox/info/refs?service=git-receive-pack"
        );
    }

    #[test]
    fn push_receive_pack_is_a_write() {
        let t = target("POST", "git-receive-pack", None).unwrap();
        assert!(t.is_write());
        assert_eq!(
            t.upstream_url("https://github.com"),
            "https://github.com/smoketurner/devbox/git-receive-pack"
        );
    }

    #[test]
    fn missing_or_unknown_service_is_not_found() {
        assert!(matches!(
            target("GET", "info/refs", None).unwrap_err(),
            GitReject::NotFound(_)
        ));
        assert!(matches!(
            target("GET", "info/refs", Some("service=git-bogus-pack")).unwrap_err(),
            GitReject::NotFound(_)
        ));
    }

    #[test]
    fn wrong_method_for_endpoint_is_not_found() {
        // upload-pack is POST-only; a GET is not a valid smart-HTTP endpoint.
        assert!(matches!(
            target("GET", "git-upload-pack", None).unwrap_err(),
            GitReject::NotFound(_)
        ));
        // info/refs is GET-only.
        assert!(matches!(
            target("POST", "info/refs", Some("service=git-upload-pack")).unwrap_err(),
            GitReject::NotFound(_)
        ));
    }

    #[test]
    fn unrelated_path_is_not_found() {
        assert!(matches!(
            target("GET", "objects/info/packs", None).unwrap_err(),
            GitReject::NotFound(_)
        ));
    }

    #[test]
    fn traversal_segments_are_rejected() {
        let method = Method::GET;
        let q = Some("service=git-upload-pack");
        for bad in ["..", ".", "", "a/b", "own er", "up%2e%2e"] {
            assert!(
                matches!(
                    authorize(&method, bad, "devbox", "info/refs", q),
                    Err(GitReject::BadRequest(_))
                ),
                "owner '{bad}' must be rejected"
            );
            assert!(
                matches!(
                    authorize(&method, "smoketurner", bad, "info/refs", q),
                    Err(GitReject::BadRequest(_))
                ),
                "repo '{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn dot_git_suffix_is_a_safe_segment() {
        // Git may or may not include the `.git` suffix; both must pass.
        let t = authorize(
            &Method::GET,
            "smoketurner",
            "devbox.git",
            "info/refs",
            Some("service=git-upload-pack"),
        )
        .unwrap();
        assert_eq!(
            t.upstream_url("https://github.com"),
            "https://github.com/smoketurner/devbox.git/info/refs?service=git-upload-pack"
        );
    }

    #[test]
    fn basic_auth_password_round_trips() {
        let mut headers = HeaderMap::new();
        let encoded = STANDARD.encode("x-devbox:the-web-identity-token");
        headers.insert(AUTHORIZATION, format!("Basic {encoded}").parse().unwrap());
        assert_eq!(
            basic_auth_password(&headers).as_deref(),
            Some("the-web-identity-token")
        );
    }

    #[test]
    fn basic_auth_password_rejects_non_basic_and_empty() {
        let mut bearer = HeaderMap::new();
        bearer.insert(AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(basic_auth_password(&bearer), None);

        let mut empty_pass = HeaderMap::new();
        empty_pass.insert(
            AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("user:"))
                .parse()
                .unwrap(),
        );
        assert_eq!(basic_auth_password(&empty_pass), None);

        assert_eq!(basic_auth_password(&HeaderMap::new()), None);
    }

    #[test]
    fn injected_header_encodes_x_access_token() {
        let header = basic_auth_header("ghs_exampletoken");
        let encoded = header.strip_prefix("Basic ").unwrap();
        let decoded = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
        assert_eq!(decoded, "x-access-token:ghs_exampletoken");
    }

    #[test]
    fn request_header_forwarding_drops_authorization() {
        // Authorization must never be forwarded (the proxy injects its own); the
        // git-protocol header must be kept so protocol v2 survives.
        assert!(!is_forwardable_request_header(&AUTHORIZATION));
        assert!(is_forwardable_request_header(&HeaderName::from_static(
            "git-protocol"
        )));
        assert!(is_forwardable_request_header(&CONTENT_TYPE));
    }
}
