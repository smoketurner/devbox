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
//! # Explicitly not a client of this module
//!
//! The CONNECT forward proxy in [`crate::egress`] is a **separate concern**: it
//! tunnels opaque TLS and carries SSRF / IMDS / RFC1918 / DNS-pin protections
//! with a different trust and protection model. It does not inject credentials
//! and is intentionally **not** a client of this module.

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

/// Build the reqwest client both reverse proxies use: the given `user_agent` and
/// a [`CONNECT_TIMEOUT`] with **no total timeout** (so long clones / streamed
/// responses are not cut off). The user-agent stays proxy-specific; only the
/// builder is shared.
///
/// # Errors
///
/// Returns an error when the HTTP client cannot be constructed.
pub(crate) fn build_client(user_agent: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("build reverse-proxy HTTP client")
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, TRANSFER_ENCODING};

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
}
