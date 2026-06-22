//! JWT-based authentication: Vouch bearer tokens and ALB OIDC data.

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use secrecy::{ExposeSecret, SecretString};

use devbox_common::is_valid_unix_username;

/// Header the ALB injects with a signed JWT for OIDC-authenticated requests.
const ALB_OIDC_DATA_HEADER: &str = "x-amzn-oidc-data";

/// Configuration for token verification.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Expected token issuer (e.g. `https://us.vouch.sh`).
    pub issuer: String,
    /// JWKS URI for bearer-token signing keys (e.g. `https://us.vouch.sh/oauth/jwks`).
    pub jwks_uri: String,
    /// Expected audience (the OIDC client id). `None` skips audience validation.
    pub audience: Option<String>,
    /// Region whose ALB public keys verify `x-amzn-oidc-data` (e.g. `us-east-1`).
    pub alb_region: Option<String>,
    /// OIDC Authorization Code settings for the browser dashboard login. `None`
    /// leaves the dashboard ungated (the API bearer path is unaffected).
    pub oidc: Option<OidcConfig>,
}

/// OIDC Authorization Code parameters for the dashboard login flow.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    /// Confidential client id of the Vouch dashboard app.
    pub client_id: String,
    /// Client secret for the dashboard app (used only in the token exchange).
    /// `SecretString` redacts it in `Debug` output and zeroizes it on drop.
    pub client_secret: SecretString,
    /// Authorization endpoint (e.g. `https://us.vouch.sh/oauth/authorize`).
    pub authorize_endpoint: String,
    /// Token endpoint (e.g. `https://us.vouch.sh/oauth/token`).
    pub token_endpoint: String,
    /// Redirect URI registered with the IdP (e.g. `https://<host>/oauth2/idpresponse`).
    pub redirect_uri: String,
    /// Scopes to request (e.g. `openid email`).
    pub scope: String,
}

/// The subset of an OAuth token response we use (the OIDC ID token).
#[derive(serde::Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// An authenticated principal (the `owner` a caller may act as).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(pub String);

/// A signed-in dashboard user: the `principal` (the Unix-safe `owner`/login,
/// derived from the email local part) plus the full `email` shown in the UI.
#[derive(Debug, Clone)]
pub struct SessionUser {
    /// The owner/login used for claim/release — the email local part.
    pub principal: String,
    /// The full email address, shown in the dashboard header.
    pub display: String,
}

/// Authentication failure.
#[derive(Debug)]
pub enum AuthError {
    /// No credential was presented.
    Missing,
    /// A credential was presented but is invalid.
    Invalid(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("no authentication credential presented"),
            Self::Invalid(msg) => write!(f, "invalid credential: {msg}"),
        }
    }
}

/// Verifies request credentials and resolves the caller's principal.
pub struct Authenticator {
    config: AuthConfig,
    http: reqwest::Client,
    /// Cached bearer (Vouch) signing keys, keyed by `kid`.
    jwks: RwLock<HashMap<String, DecodingKey>>,
    /// Cached ALB signing keys, keyed by `kid`.
    alb_keys: RwLock<HashMap<String, DecodingKey>>,
}

impl Authenticator {
    /// Build an authenticator for the given configuration.
    #[must_use]
    pub fn new(config: AuthConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            jwks: RwLock::new(HashMap::new()),
            alb_keys: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve the caller's principal from request headers.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Missing`] when no credential is present, or
    /// [`AuthError::Invalid`] when one is present but fails verification.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        if let Some(value) = headers.get(ALB_OIDC_DATA_HEADER) {
            let token = value
                .to_str()
                .map_err(|_| AuthError::Invalid("non-ASCII x-amzn-oidc-data".to_string()))?;
            return self.verify_alb(token).await;
        }

        if let Some(value) = headers.get(AUTHORIZATION) {
            let header = value
                .to_str()
                .map_err(|_| AuthError::Invalid("non-ASCII authorization header".to_string()))?;
            let token = header
                .strip_prefix("Bearer ")
                .or_else(|| header.strip_prefix("bearer "))
                .ok_or_else(|| AuthError::Invalid("expected a Bearer token".to_string()))?;
            return self.verify_bearer(token.trim()).await;
        }

        Err(AuthError::Missing)
    }

    /// Verify a Vouch bearer token against the configured JWKS.
    async fn verify_bearer(&self, token: &str) -> Result<Principal, AuthError> {
        let kid = key_id(token)?;
        let key = self.bearer_key(&kid).await?;

        let mut validation = Validation::new(token_algorithm(token)?);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        match self.config.audience.as_deref() {
            Some(aud) => validation.set_audience(&[aud]),
            None => validation.validate_aud = false,
        }

        let (owner, _email) = decode_owner(token, &key, &validation)?;
        Ok(Principal(owner))
    }

    /// Verify an ALB `x-amzn-oidc-data` JWT against the ALB's regional key.
    async fn verify_alb(&self, token: &str) -> Result<Principal, AuthError> {
        let kid = key_id(token)?;
        let key = self.alb_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_aud = false;

        let (owner, _email) = decode_owner(token, &key, &validation)?;
        Ok(Principal(owner))
    }

    /// Look up a bearer signing key by `kid`, refreshing the JWKS on a miss.
    async fn bearer_key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = read_key(&self.jwks, kid) {
            return Ok(key);
        }
        self.refresh_jwks().await?;
        read_key(&self.jwks, kid)
            .ok_or_else(|| AuthError::Invalid(format!("unknown signing key id {kid}")))
    }

    /// Fetch the JWKS and replace the cache (handles key rotation).
    async fn refresh_jwks(&self) -> Result<(), AuthError> {
        let set: JwkSet = self
            .http
            .get(&self.config.jwks_uri)
            .send()
            .await
            .map_err(|e| AuthError::Invalid(format!("fetch JWKS: {e}")))?
            .json()
            .await
            .map_err(|e| AuthError::Invalid(format!("parse JWKS: {e}")))?;

        let mut keys = HashMap::new();
        for jwk in &set.keys {
            if let Some(kid) = jwk.common.key_id.clone()
                && let Ok(key) = DecodingKey::from_jwk(jwk)
            {
                keys.insert(kid, key);
            }
        }

        if let Ok(mut guard) = self.jwks.write() {
            *guard = keys;
        }
        Ok(())
    }

    /// Look up an ALB signing key by `kid`, fetching its PEM on a miss.
    async fn alb_key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = read_key(&self.alb_keys, kid) {
            return Ok(key);
        }

        let region = self
            .config
            .alb_region
            .as_deref()
            .ok_or_else(|| AuthError::Invalid("ALB region not configured".to_string()))?;
        let url = format!("https://public-keys.auth.elb.{region}.amazonaws.com/{kid}");
        let pem = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AuthError::Invalid(format!("fetch ALB key: {e}")))?
            .text()
            .await
            .map_err(|e| AuthError::Invalid(format!("read ALB key: {e}")))?;

        let key = DecodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|e| AuthError::Invalid(format!("parse ALB key: {e}")))?;

        if let Ok(mut guard) = self.alb_keys.write() {
            guard.insert(kid.to_string(), key.clone());
        }
        Ok(key)
    }

    /// OIDC dashboard-login settings, when configured.
    #[must_use]
    pub fn oidc(&self) -> Option<&OidcConfig> {
        self.config.oidc.as_ref()
    }

    /// Build the IdP authorization URL for the dashboard login redirect.
    ///
    /// `state` is the opaque CSRF token echoed back to the callback. Returns
    /// `None` when OIDC login is not configured or the endpoint is unparseable.
    #[must_use]
    pub fn authorize_url(&self, state: &str) -> Option<String> {
        let oidc = self.config.oidc.as_ref()?;
        let mut url = url::Url::parse(&oidc.authorize_endpoint).ok()?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &oidc.client_id)
            .append_pair("redirect_uri", &oidc.redirect_uri)
            .append_pair("scope", &oidc.scope)
            .append_pair("state", state);
        Some(url.to_string())
    }

    /// Exchange an authorization `code` for the IdP's OIDC ID token.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Invalid`] when OIDC is unconfigured, the token
    /// request fails, or the response lacks an `id_token`.
    pub async fn exchange_code(&self, code: &str) -> Result<String, AuthError> {
        let oidc = self
            .config
            .oidc
            .as_ref()
            .ok_or_else(|| AuthError::Invalid("OIDC login not configured".to_string()))?;

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &oidc.redirect_uri)
            .append_pair("client_id", &oidc.client_id)
            .append_pair("client_secret", oidc.client_secret.expose_secret())
            .finish();

        let resp = self
            .http
            .post(&oidc.token_endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|e| AuthError::Invalid(format!("token exchange request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(AuthError::Invalid(format!(
                "token endpoint returned {}",
                resp.status()
            )));
        }

        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| AuthError::Invalid(format!("parse token response: {e}")))?;
        Ok(token.id_token)
    }

    /// Verify an OIDC ID token (the dashboard session cookie) against Vouch's
    /// JWKS, the configured issuer, and the dashboard client id as audience.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Invalid`] when OIDC is unconfigured or the token
    /// fails verification.
    pub async fn verify_id_token(&self, token: &str) -> Result<SessionUser, AuthError> {
        let oidc = self
            .config
            .oidc
            .as_ref()
            .ok_or_else(|| AuthError::Invalid("OIDC login not configured".to_string()))?;

        let kid = key_id(token)?;
        let key = self.bearer_key(&kid).await?;

        let mut validation = Validation::new(token_algorithm(token)?);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[oidc.client_id.as_str()]);

        let (owner, email) = decode_owner(token, &key, &validation)?;
        Ok(SessionUser {
            principal: owner,
            display: email,
        })
    }
}

/// Generate an unguessable token (256 bits, hex-encoded) for the OIDC `state`
/// CSRF parameter.
///
/// # Errors
///
/// Returns [`AuthError::Invalid`] if the system RNG fails.
pub(crate) fn random_token() -> Result<String, AuthError> {
    let mut buf = [0u8; 32];
    aws_lc_rs::rand::fill(&mut buf).map_err(|_| AuthError::Invalid("RNG failure".to_string()))?;
    let mut out = String::with_capacity(64);
    for byte in buf {
        if let (Some(hi), Some(lo)) = (
            char::from_digit(u32::from(byte >> 4), 16),
            char::from_digit(u32::from(byte & 0x0f), 16),
        ) {
            out.push(hi);
            out.push(lo);
        }
    }
    Ok(out)
}

/// Clone a cached key out of `cache` without holding the lock across `.await`.
fn read_key(cache: &RwLock<HashMap<String, DecodingKey>>, kid: &str) -> Option<DecodingKey> {
    cache.read().ok().and_then(|map| map.get(kid).cloned())
}

/// Read the `kid` from a token's header.
fn key_id(token: &str) -> Result<String, AuthError> {
    decode_header(token)
        .map_err(|e| AuthError::Invalid(format!("bad token header: {e}")))?
        .kid
        .ok_or_else(|| AuthError::Invalid("token header missing kid".to_string()))
}

/// Read a token's signing algorithm, restricted to the asymmetric algorithms
/// Vouch issues (`RS256`, `ES256`).
///
/// Validation must use the token's own algorithm rather than a fixed list:
/// `jsonwebtoken` rejects a decoding key whose family differs from *any*
/// algorithm in `Validation::algorithms`, so a mixed `[RS256, ES256]` list fails
/// an `ES256` (EC-key) token with `InvalidAlgorithm`. Restricting to the
/// allow-listed header algorithm — with the key looked up by `kid` from the
/// trusted JWKS — keeps a single family and blocks algorithm-confusion.
fn token_algorithm(token: &str) -> Result<Algorithm, AuthError> {
    let alg = decode_header(token)
        .map_err(|e| AuthError::Invalid(format!("bad token header: {e}")))?
        .alg;
    match alg {
        Algorithm::RS256 | Algorithm::ES256 => Ok(alg),
        unsupported => Err(AuthError::Invalid(format!(
            "unsupported token algorithm {unsupported:?}"
        ))),
    }
}

/// Verify `token` with `key`/`validation`, then return `(owner, email)`: the
/// `email` claim and the Unix-safe owner derived from its local part.
///
/// Vouch has no username concept — its `sub` is an opaque UUID — so the `owner`
/// (Unix login / claim identity / SSH cert principal) is derived from the email.
fn decode_owner(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
) -> Result<(String, String), AuthError> {
    let data = decode::<serde_json::Value>(token, key, validation)
        .map_err(|e| AuthError::Invalid(format!("token validation failed: {e}")))?;

    let email = data
        .claims
        .get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::Invalid("token missing 'email' claim".to_string()))?;

    let owner = username_from_email(email).ok_or_else(|| {
        AuthError::Invalid(format!("cannot derive a Unix login from email '{email}'"))
    })?;

    Ok((owner, email.to_string()))
}

/// Derive a Unix login from an email: the local part (before `@`), surrounding
/// whitespace trimmed and lowercased. Returns `None` unless that is already a
/// valid Unix username.
///
/// Only surrounding whitespace is trimmed; internal characters are never
/// stripped and the result is never truncated — a non-conforming local part is
/// rejected, not mangled — so distinct local parts can never collide on the same
/// `owner` (which would let one user act on another's devboxes).
fn username_from_email(email: &str) -> Option<String> {
    let local = email.trim().split('@').next()?.trim().to_ascii_lowercase();
    is_valid_unix_username(&local).then_some(local)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    const SECRET: &[u8] = b"test-signing-secret";
    const ISSUER: &str = "https://us.vouch.sh";

    fn sign(claims: serde_json::Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap()
    }

    fn validation() -> Validation {
        let mut v = Validation::new(Algorithm::HS256);
        v.set_issuer(&[ISSUER]);
        v.validate_aud = false;
        v
    }

    #[test]
    fn decode_owner_derives_login_from_email() {
        let token =
            sign(json!({ "email": "jane@example.com", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        let (owner, email) = decode_owner(&token, &key, &validation()).unwrap();
        assert_eq!(owner, "jane");
        assert_eq!(email, "jane@example.com");
    }

    #[test]
    fn decode_owner_rejects_missing_email() {
        let token = sign(json!({ "sub": "uuid-only", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(decode_owner(&token, &key, &validation()).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(json!({ "email": "jane@example.com", "iss": ISSUER, "exp": 1_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(decode_owner(&token, &key, &validation()).is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let token = sign(json!({
            "email": "jane@example.com", "iss": "https://evil.example", "exp": 9_999_999_999_u64
        }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(decode_owner(&token, &key, &validation()).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let token =
            sign(json!({ "email": "jane@example.com", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(b"a-different-secret");
        assert!(decode_owner(&token, &key, &validation()).is_err());
    }

    #[test]
    fn username_from_email_takes_local_part() {
        assert_eq!(
            username_from_email("justin@plock.net").as_deref(),
            Some("justin")
        );
    }

    #[test]
    fn username_from_email_lowercases_and_trims() {
        assert_eq!(
            username_from_email("  Justin@example.com  ").as_deref(),
            Some("justin")
        );
    }

    #[test]
    fn username_from_email_rejects_non_conforming_local_part_without_collision() {
        // Punctuation is never stripped, so `a.b` is rejected rather than mangled
        // into `ab` (which would collide with a distinct `ab@` address).
        assert_eq!(username_from_email("a.b@corp.com"), None);
        assert_eq!(username_from_email("ab@corp.com").as_deref(), Some("ab"));
    }

    #[test]
    fn username_from_email_rejects_underiverable() {
        assert!(username_from_email("123@example.com").is_none()); // leading digit
        assert!(username_from_email("@example.com").is_none()); // empty local part
        let long = format!("{}@example.com", "a".repeat(33));
        assert!(username_from_email(&long).is_none()); // >32 chars, never truncated
    }

    fn base_config(oidc: Option<OidcConfig>) -> AuthConfig {
        AuthConfig {
            issuer: ISSUER.to_string(),
            jwks_uri: "https://us.vouch.sh/oauth/jwks".to_string(),
            audience: None,
            alb_region: None,
            oidc,
        }
    }

    fn test_oidc() -> OidcConfig {
        OidcConfig {
            client_id: "client-123".to_string(),
            client_secret: SecretString::from("s3cr3t-do-not-leak".to_string()),
            authorize_endpoint: "https://us.vouch.sh/oauth/authorize".to_string(),
            token_endpoint: "https://us.vouch.sh/oauth/token".to_string(),
            redirect_uri: "https://cp.example/oauth2/idpresponse".to_string(),
            scope: "openid email".to_string(),
        }
    }

    #[test]
    fn authorize_url_includes_required_params() {
        let auth = Authenticator::new(base_config(Some(test_oidc())));
        let url = auth.authorize_url("abc123").unwrap();
        assert!(url.starts_with("https://us.vouch.sh/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("state=abc123"));
        assert!(url.contains("scope=openid+email"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fcp.example%2Foauth2%2Fidpresponse"));
    }

    #[test]
    fn authorize_url_none_without_oidc() {
        let auth = Authenticator::new(base_config(None));
        assert!(auth.authorize_url("abc123").is_none());
        assert!(auth.oidc().is_none());
    }

    #[test]
    fn oidc_debug_redacts_secret() {
        let rendered = format!("{:?}", test_oidc());
        assert!(
            rendered.contains("REDACTED"),
            "expected a redaction marker: {rendered}"
        );
        assert!(
            !rendered.contains("s3cr3t-do-not-leak"),
            "client_secret must not leak: {rendered}"
        );
    }

    #[test]
    fn token_algorithm_rejects_non_allowlisted() {
        // The test helper signs with HS256, which is not an accepted asymmetric
        // algorithm — token_algorithm must reject it.
        let token = sign(json!({ "sub": "jplock", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        assert!(token_algorithm(&token).is_err());
    }

    #[test]
    fn random_token_is_64_unique_hex_chars() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_token().unwrap(), token);
    }
}
