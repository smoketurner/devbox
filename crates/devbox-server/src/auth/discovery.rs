//! OIDC discovery: resolve the IdP's endpoints from its well-known document.
//!
//! The server reads only `AUTH_OIDC_ISSUER`; every endpoint it needs — the JWKS
//! URI (bearer-token verification) and the dashboard authorize / token /
//! end-session endpoints — is published by the issuer's OIDC discovery document
//! at `{issuer}/.well-known/openid-configuration` and fetched once at startup.
//! This is the same discovery the CLI performs (`devbox-cli`'s `auth` module),
//! so there is a single source of truth for endpoints (the issuer) and no
//! hardcoded defaults to drift out of sync.

use serde::Deserialize;

use super::AuthError;

/// The endpoints the server consumes from the OIDC discovery document.
///
/// Only `issuer` and `jwks_uri` are required: the server verifies every bearer
/// token against `jwks_uri`, so API auth (the always-on path) cannot work
/// without it. The other three drive the dashboard login flow and are optional
/// here — `end_session_endpoint` is itself optional in the OIDC spec (it comes
/// from RP-Initiated Logout, not Core discovery), and an API-only deployment
/// never needs any of them. They are validated when the dashboard OIDC config is
/// actually built (see `build_oidc_config`), so a missing field fails the
/// dashboard rather than blocking server boot.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcEndpoints {
    /// The issuer identifier; must equal the issuer the document was fetched from.
    pub issuer: String,
    /// JWKS URI for the signing keys that verify Vouch tokens.
    pub jwks_uri: String,
    /// OAuth authorization endpoint (dashboard login redirect).
    pub authorization_endpoint: Option<String>,
    /// OAuth token endpoint (dashboard authorization-code exchange).
    pub token_endpoint: Option<String>,
    /// RP-Initiated Logout end-session endpoint (dashboard sign-out).
    pub end_session_endpoint: Option<String>,
}

/// Fetch and validate the issuer's OIDC discovery document.
///
/// # Errors
///
/// Returns [`AuthError::Invalid`] when the request fails, the response is not
/// 2xx, the body cannot be parsed into [`OidcEndpoints`] (e.g. it omits the
/// required `issuer` or `jwks_uri`), or the document's `issuer` does not match
/// `issuer` (an OIDC issuer mix-up guard).
pub async fn discover(client: &reqwest::Client, issuer: &str) -> Result<OidcEndpoints, AuthError> {
    let url = format!("{issuer}/.well-known/openid-configuration");

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AuthError::Invalid(format!("fetch OIDC discovery {url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(AuthError::Invalid(format!(
            "OIDC discovery {url} returned {}",
            resp.status()
        )));
    }

    let endpoints: OidcEndpoints = resp
        .json()
        .await
        .map_err(|e| AuthError::Invalid(format!("parse OIDC discovery {url}: {e}")))?;

    if endpoints.issuer != issuer {
        return Err(AuthError::Invalid(format!(
            "OIDC discovery issuer mismatch: configured {issuer}, document {}",
            endpoints.issuer
        )));
    }

    Ok(endpoints)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::routing::get;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    /// Bind an ephemeral port and return `(listener, base_url)` so a test can
    /// build a router whose served document references its own base URL.
    async fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, format!("http://127.0.0.1:{}", addr.port()))
    }

    /// Spawn the router on `listener` in the background.
    fn spawn(listener: TcpListener, router: axum::Router) {
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
    }

    /// A well-formed discovery document whose `issuer` equals `base`.
    fn discovery_doc(base: &str) -> Value {
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "jwks_uri": format!("{base}/oauth/jwks"),
            "end_session_endpoint": format!("{base}/oauth/logout"),
        })
    }

    /// Serve `doc` at the well-known path and return its base URL.
    async fn serve_doc(doc: Value) -> String {
        let (listener, base) = bind().await;
        let router = axum::Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let doc = doc.clone();
                async move { Json(doc) }
            }),
        );
        spawn(listener, router);
        base
    }

    #[tokio::test]
    async fn discover_parses_all_endpoints() {
        let (listener, base) = bind().await;
        let doc = discovery_doc(&base);
        let router = axum::Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let doc = doc.clone();
                async move { Json(doc) }
            }),
        );
        spawn(listener, router);

        let endpoints = discover(&reqwest::Client::new(), &base).await.unwrap();
        assert_eq!(endpoints.issuer, base);
        assert_eq!(
            endpoints.authorization_endpoint.as_deref(),
            Some(format!("{base}/oauth/authorize").as_str())
        );
        assert_eq!(
            endpoints.token_endpoint.as_deref(),
            Some(format!("{base}/oauth/token").as_str())
        );
        assert_eq!(endpoints.jwks_uri, format!("{base}/oauth/jwks"));
        assert_eq!(
            endpoints.end_session_endpoint.as_deref(),
            Some(format!("{base}/oauth/logout").as_str())
        );
    }

    #[tokio::test]
    async fn discover_allows_missing_optional_endpoints() {
        // A minimal document with only the required fields parses: API bearer
        // auth needs `jwks_uri`, and the dashboard endpoints are optional so an
        // issuer that omits `end_session_endpoint` does not block server boot.
        let (listener, base) = bind().await;
        let doc = json!({
            "issuer": base,
            "jwks_uri": format!("{base}/oauth/jwks"),
        });
        let router = axum::Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let doc = doc.clone();
                async move { Json(doc) }
            }),
        );
        spawn(listener, router);

        let endpoints = discover(&reqwest::Client::new(), &base).await.unwrap();
        assert_eq!(endpoints.jwks_uri, format!("{base}/oauth/jwks"));
        assert!(endpoints.authorization_endpoint.is_none());
        assert!(endpoints.token_endpoint.is_none());
        assert!(endpoints.end_session_endpoint.is_none());
    }

    #[tokio::test]
    async fn discover_rejects_issuer_mismatch() {
        // The document advertises a different issuer than the one it was fetched
        // from — an OIDC mix-up guard must reject it.
        let base = serve_doc(discovery_doc("https://evil.example")).await;

        let err = discover(&reqwest::Client::new(), &base).await.unwrap_err();
        assert!(
            matches!(&err, AuthError::Invalid(msg) if msg.contains("issuer mismatch")),
            "expected issuer-mismatch error, got: {err}"
        );
    }

    #[tokio::test]
    async fn discover_rejects_non_success_status() {
        let (listener, base) = bind().await;
        let router = axum::Router::new().route(
            "/.well-known/openid-configuration",
            get(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        spawn(listener, router);

        let err = discover(&reqwest::Client::new(), &base).await.unwrap_err();
        assert!(
            matches!(&err, AuthError::Invalid(msg) if msg.contains("returned")),
            "expected non-success error, got: {err}"
        );
    }

    #[tokio::test]
    async fn discover_rejects_missing_field() {
        // A document lacking the required jwks_uri must fail to parse rather than
        // silently producing a half-built config (without it no bearer token can
        // be verified).
        let base = serve_doc(json!({
            "issuer": "PLACEHOLDER",
            "authorization_endpoint": "x/authorize",
            "token_endpoint": "x/token",
            "end_session_endpoint": "x/logout",
        }))
        .await;

        let err = discover(&reqwest::Client::new(), &base).await.unwrap_err();
        assert!(
            matches!(&err, AuthError::Invalid(msg) if msg.contains("parse OIDC discovery")),
            "expected parse error, got: {err}"
        );
    }
}
