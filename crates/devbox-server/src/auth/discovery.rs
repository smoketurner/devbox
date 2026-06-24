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
/// All four are required: Vouch publishes them, the server uses `jwks_uri` on
/// every token verification, and the other three drive the dashboard login flow.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcEndpoints {
    /// The issuer identifier; must equal the issuer the document was fetched from.
    pub issuer: String,
    /// OAuth authorization endpoint (dashboard login redirect).
    pub authorization_endpoint: String,
    /// OAuth token endpoint (dashboard authorization-code exchange).
    pub token_endpoint: String,
    /// JWKS URI for the signing keys that verify Vouch tokens.
    pub jwks_uri: String,
    /// RP-Initiated Logout end-session endpoint (dashboard sign-out).
    pub end_session_endpoint: String,
}

/// Fetch and validate the issuer's OIDC discovery document.
///
/// # Errors
///
/// Returns [`AuthError::Invalid`] when the request fails, the response is not
/// 2xx, the body cannot be parsed into [`OidcEndpoints`], or the document's
/// `issuer` does not match `issuer` (an OIDC issuer mix-up guard).
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
            endpoints.authorization_endpoint,
            format!("{base}/oauth/authorize")
        );
        assert_eq!(endpoints.token_endpoint, format!("{base}/oauth/token"));
        assert_eq!(endpoints.jwks_uri, format!("{base}/oauth/jwks"));
        assert_eq!(
            endpoints.end_session_endpoint,
            format!("{base}/oauth/logout")
        );
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
        // A document lacking end_session_endpoint must fail to parse rather than
        // silently producing a half-built config.
        let base = serve_doc(json!({
            "issuer": "PLACEHOLDER",
            "authorization_endpoint": "x/authorize",
            "token_endpoint": "x/token",
            "jwks_uri": "x/jwks",
        }))
        .await;

        let err = discover(&reqwest::Client::new(), &base).await.unwrap_err();
        assert!(
            matches!(&err, AuthError::Invalid(msg) if msg.contains("parse OIDC discovery")),
            "expected parse error, got: {err}"
        );
    }
}
