//! Shared transport layer for the credential-injecting reverse proxies.
//!
//! The control plane runs two credential-injecting **reverse** proxies that are
//! structurally the same hop: authenticate the box, drop any client
//! `Authorization`, inject a server-held credential the box never sees, and
//! stream the upstream response back. This module holds the genuinely-identical
//! transport/header plumbing both share — the response-header safelist, the
//! request-header safelist (with a per-proxy `extra` extension), the streaming
//! [`into_axum_response`] adapter, and the reqwest client builder with a single
//! shared [`CONNECT_TIMEOUT`] and no total timeout — so a long clone or a
//! streamed LLM response is never cut off mid-flight.
//!
//! # What is shared vs. per-proxy
//!
//! Only the transport/header plumbing lives here. Each proxy keeps its own
//! credential-injection strategy and its own request grammar layered on top:
//!
//! - [`crate::github::git_proxy`] mints a **per-request repo-scoped read/write
//!   GitHub App token** ([`crate::github::GitHubApp::token_for`]), gates push on
//!   a claimed box, parses the git smart-HTTP endpoint grammar
//!   (`info/refs?service=…`, `git-upload-pack`, `git-receive-pack`), enforces
//!   owner/repo path-segment safety, and preserves the `git-protocol` header for
//!   protocol v2 (its only extra forwardable request header).
//! - [`crate::egress_proxy`] injects a **static header** read from SSM or an
//!   **AWS SigV4** signature for a single fixed host per named upstream.
//!
//! None of that credential-minting or request-grammar logic belongs here; this
//! module is deliberately the thin, security-boring floor beneath both.
//!
//! # Design inspiration: exe.dev's integration model
//!
//! This split mirrors exe.dev's "integration" model, where every way of reaching
//! an outside service is one typed integration behind a shared credential-
//! injecting proxy edge: an HTTP Proxy integration injects request headers,
//! Object Storage (S3) signs each request with AWS SigV4, an LLM integration
//! fronts managed providers, and GitHub is just **one typed integration** rather
//! than a bespoke subsystem. In every case the secret is held off-VM and
//! injected at the network edge, so the VM can use the integration but can never
//! read the credential. We follow the same shape: the shared edge (this module)
//! is uniform, and git-token-minting reads as one credential strategy alongside
//! static-header and SigV4. We do **not** over-claim unification: git's
//! smart-HTTP grammar, claim-gating, and per-request repo-scoped read/write
//! token minting stay git-specific and are layered on top of this shared floor.
//!
//! # SSRF / IMDS / RFC1918 guard on the shared transport
//!
//! The reqwest client [`build_client`] returns installs a custom DNS resolver
//! ([`GuardedResolver`]) that screens every upstream name through the shared
//! [`crate::net_guard`] IP-classification guard. For each connection reqwest asks
//! the resolver for the name's addresses; the resolver resolves the name, drops
//! every forbidden IP (loopback, RFC1918, the `169.254.169.254` IMDS endpoint,
//! and the rest), and hands reqwest ONLY the validated public addresses. If none
//! remain it returns an error so the connection fails **fail-closed** (surfacing
//! as a `forward()` error -> `AppError::Internal` -> HTTP 5xx, never a 2xx, and
//! no credential is injected). Because reqwest then dials exactly one of those
//! validated addresses with no independent second resolution, a name cannot
//! rebind to a forbidden IP between the check and the dial (no DNS-rebind
//! window), while reqwest still performs TLS SNI and certificate verification
//! against the ORIGINAL upstream hostname (the resolver only overrides which IP
//! is dialed, never the server name).
//!
//! This guard covers **both** reverse proxies, because both are clients of
//! [`build_client`]: the credential-injecting [`crate::egress_proxy`] and the git
//! reverse proxy [`crate::github::git_proxy`]. The `github.com` upstream is lower
//! risk but gets the same fail-closed guard for free, with no behavior change for
//! public GitHub IPs.
//!
//! # Relationship to the CONNECT forward proxy
//!
//! The CONNECT forward proxy in [`crate::egress`] is still a **separate concern**:
//! it tunnels opaque TLS, does not inject credentials, and is intentionally
//! **not** a client of this module. What the two now share is the IP-classification
//! logic: both the CONNECT proxy and this reverse-proxy transport screen resolved
//! addresses through the one [`crate::net_guard`] guard, each applying it at its
//! own connect point.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE,
    USER_AGENT,
};
use axum::http::{HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};

/// Upstream connect timeout shared by both reverse proxies. There is
/// deliberately **no total timeout**, so a long streamed response (a git clone,
/// a streamed LLM completion) is not cut off mid-flight.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether an incoming request header is forwarded upstream.
///
/// The base safelist keeps content-negotiation and framing headers
/// (`Accept`, `Accept-Encoding`, `Content-Type`, `Content-Encoding`,
/// `User-Agent`) and, critically, **always drops `Authorization`** — the proxy
/// injects its own server-held credential — along with length/hop-by-hop headers
/// so reqwest re-frames the body. `extra` lists additional lowercased header
/// names a specific proxy also forwards (the git proxy passes `&["git-protocol"]`
/// so protocol v2 survives; the egress proxy passes `&[]`). `Authorization` is
/// dropped unconditionally — it is short-circuited before `extra` is consulted,
/// so it stays dropped even if a caller mistakenly lists it as an extra.
pub(crate) fn is_forwardable_request_header(name: &HeaderName, extra: &[&str]) -> bool {
    if name == AUTHORIZATION {
        return false;
    }
    name == ACCEPT
        || name == ACCEPT_ENCODING
        || name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == USER_AGENT
        || extra.iter().any(|allowed| name.as_str() == *allowed)
}

/// Whether an upstream response header is forwarded back. Keeps content framing
/// and cache directives; drops length/transfer headers so axum re-frames the
/// stream. Identical across both reverse proxies.
pub(crate) fn is_forwardable_response_header(name: &HeaderName) -> bool {
    name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == CACHE_CONTROL
        || name.as_str() == "expires"
        || name.as_str() == "pragma"
}

/// Convert an upstream response into a streaming axum response: the status, the
/// safelisted headers ([`is_forwardable_response_header`]), and the body streamed
/// (never buffered), with a `BAD_GATEWAY` fallback if the response cannot be
/// built.
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

/// A reqwest [`reqwest::dns::Resolve`] that applies the shared
/// [`crate::net_guard`] SSRF / IMDS / RFC1918 guard to every upstream name.
///
/// reqwest invokes this per-connection with the exact hostname it is about to
/// dial. We resolve the name, drop every forbidden address, and return only the
/// validated public ones. When none remain we return an error so the connection
/// fails fail-closed. This pins the dialed IP to a validated address (there is no
/// independent second resolution, so no DNS-rebind window) while reqwest still
/// does TLS SNI / certificate verification against the original hostname — a
/// custom `dns_resolver` only overrides which IP is dialed, never the server name.
struct GuardedResolver;

impl GuardedResolver {
    fn new() -> Self {
        Self
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        // Own the host so the returned future is 'static.
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Port 0 here is fine: reqwest replaces the port of each returned
            // SocketAddr with the URL's explicit port, or the scheme default.
            let addrs = crate::net_guard::resolve_public_addrs(&host, 0)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if addrs.is_empty() {
                // Fail-closed: the name resolves only to forbidden addresses
                // (or not at all after filtering), so refuse the connection
                // rather than dial a private/IMDS/loopback host.
                return Err(format!(
                    "refusing to connect to {host}: resolves only to non-public addresses"
                )
                .into());
            }
            let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// Build the reqwest client both reverse proxies use: the given `user_agent`, a
/// [`CONNECT_TIMEOUT`] with **no total timeout** (so long clones / streamed
/// responses are not cut off), and the shared [`GuardedResolver`] so every
/// upstream connection is screened by the [`crate::net_guard`] SSRF / IMDS /
/// RFC1918 guard. The user-agent stays proxy-specific; the builder (and now the
/// guard) is shared by both reverse proxies.
///
/// Installing the resolver here means reqwest dials exactly one validated public
/// address with no independent re-resolution (closing the DNS-rebind window)
/// while still performing TLS SNI / certificate verification against the original
/// upstream hostname — `dns_resolver` overrides only the dialed address, never
/// the TLS server name.
///
/// # Errors
///
/// Returns an error when the HTTP client cannot be constructed.
pub(crate) fn build_client(user_agent: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .dns_resolver(Arc::new(GuardedResolver::new()))
        .build()
        .context("build reverse-proxy HTTP client")
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::net::IpAddr;

    use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, TRANSFER_ENCODING};
    use reqwest::dns::Resolve as _;

    use super::*;

    #[test]
    fn response_safelist_admits_framing_and_cache_headers() {
        assert!(is_forwardable_response_header(&CONTENT_TYPE));
        assert!(is_forwardable_response_header(&CONTENT_ENCODING));
        assert!(is_forwardable_response_header(&CACHE_CONTROL));
        assert!(is_forwardable_response_header(&HeaderName::from_static(
            "expires"
        )));
        assert!(is_forwardable_response_header(&HeaderName::from_static(
            "pragma"
        )));
    }

    #[test]
    fn response_safelist_drops_length_and_transfer_headers() {
        // Length/transfer headers are dropped so axum re-frames the stream.
        assert!(!is_forwardable_response_header(&CONTENT_LENGTH));
        assert!(!is_forwardable_response_header(&TRANSFER_ENCODING));
    }

    #[test]
    fn request_safelist_admits_base_and_always_drops_authorization() {
        // Base content-negotiation/framing headers are forwarded; Authorization is
        // dropped regardless of `extra` (the proxy injects its own credential).
        assert!(is_forwardable_request_header(&ACCEPT, &[]));
        assert!(is_forwardable_request_header(&CONTENT_TYPE, &[]));
        assert!(is_forwardable_request_header(&USER_AGENT, &[]));
        assert!(!is_forwardable_request_header(&AUTHORIZATION, &[]));
        assert!(!is_forwardable_request_header(
            &AUTHORIZATION,
            &["git-protocol", "authorization"]
        ));
    }

    #[test]
    fn request_safelist_honors_the_extra_list() {
        let git_protocol = HeaderName::from_static("git-protocol");
        // git passes &["git-protocol"] so protocol v2 survives; egress passes &[].
        assert!(is_forwardable_request_header(
            &git_protocol,
            &["git-protocol"]
        ));
        assert!(!is_forwardable_request_header(&git_protocol, &[]));
    }

    #[test]
    fn build_client_succeeds() {
        // Construct only: the builder must return Ok and yield a usable client.
        let client = build_client("devbox-server-test-proxy").unwrap();
        // Touch the client so it is not optimized to a mere construction check.
        drop(client.get("https://example.invalid/"));
    }

    #[tokio::test]
    async fn guarded_resolver_rejects_forbidden_literal_fail_closed() {
        // A literal forbidden IP (loopback) resolves to itself via lookup_host
        // (no network), is dropped by the shared guard, and since nothing remains
        // the resolver returns Err — reqwest would fail the connection before any
        // request is sent or credential injected (fail-closed, surfaces as 5xx).
        let resolver = GuardedResolver::new();
        for forbidden in [
            "127.0.0.1",
            "10.0.0.5",
            "169.254.169.254",
            "[::ffff:127.0.0.1]",
            "[::7f00:1]",
        ] {
            let name: reqwest::dns::Name = forbidden.parse().unwrap();
            let result = resolver.resolve(name).await;
            assert!(
                result.is_err(),
                "forbidden literal {forbidden} must be rejected fail-closed"
            );
        }
    }

    #[tokio::test]
    async fn guarded_resolver_allows_public_literal() {
        // A public literal IP is kept and handed back (with port 0, which reqwest
        // later overrides from the URL / scheme default).
        let resolver = GuardedResolver::new();
        let name: reqwest::dns::Name = "140.82.121.3".parse().unwrap();
        let addrs: Vec<_> = resolver.resolve(name).await.unwrap().collect();
        assert_eq!(addrs.len(), 1);
        assert_eq!(
            addrs.first().unwrap().ip(),
            "140.82.121.3".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn guard_is_shared_with_the_connect_proxy() {
        // The reverse-proxy transport screens IPs through the SAME
        // crate::net_guard::is_forbidden_ip the CONNECT proxy uses — there is one
        // definition, not a forked copy. Assert the shared verdict directly on the
        // addresses that matter (IMDS, RFC1918, loopback, public).
        use crate::net_guard::is_forbidden_ip;
        assert!(is_forbidden_ip("169.254.169.254".parse().unwrap()));
        assert!(is_forbidden_ip("10.0.0.5".parse().unwrap()));
        assert!(is_forbidden_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_forbidden_ip("140.82.121.3".parse().unwrap()));
    }
}
