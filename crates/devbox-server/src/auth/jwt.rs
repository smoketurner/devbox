//! JWT-based authentication: Vouch bearer tokens and ALB OIDC data.

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

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
    /// Claim that carries the principal — MUST match the Vouch SSH cert principal
    /// (a Unix-safe username), since `owner` drives both the box tag and login.
    pub principal_claim: String,
    /// Region whose ALB public keys verify `x-amzn-oidc-data` (e.g. `us-east-1`).
    pub alb_region: Option<String>,
}

/// An authenticated principal (the `owner` a caller may act as).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(pub String);

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

        let mut validation = Validation::new(Algorithm::RS256);
        validation.algorithms = vec![Algorithm::RS256, Algorithm::ES256];
        validation.set_issuer(&[self.config.issuer.as_str()]);
        match self.config.audience.as_deref() {
            Some(aud) => validation.set_audience(&[aud]),
            None => validation.validate_aud = false,
        }

        extract_principal(token, &key, &validation, &self.config.principal_claim)
    }

    /// Verify an ALB `x-amzn-oidc-data` JWT against the ALB's regional key.
    async fn verify_alb(&self, token: &str) -> Result<Principal, AuthError> {
        let kid = key_id(token)?;
        let key = self.alb_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_aud = false;

        extract_principal(token, &key, &validation, &self.config.principal_claim)
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

/// Verify `token` with `key`/`validation` and pull the principal claim.
fn extract_principal(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
    claim: &str,
) -> Result<Principal, AuthError> {
    let data = decode::<serde_json::Value>(token, key, validation)
        .map_err(|e| AuthError::Invalid(format!("token validation failed: {e}")))?;

    let principal = data
        .claims
        .get(claim)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AuthError::Invalid(format!("token missing '{claim}' claim")))?;

    if principal.is_empty() {
        return Err(AuthError::Invalid(format!("empty '{claim}' claim")));
    }
    Ok(Principal(principal.to_string()))
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
    fn valid_token_yields_principal() {
        let token = sign(json!({ "sub": "jplock", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        let principal = extract_principal(&token, &key, &validation(), "sub").unwrap();
        assert_eq!(principal, Principal("jplock".to_string()));
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(json!({ "sub": "jplock", "iss": ISSUER, "exp": 1_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(extract_principal(&token, &key, &validation(), "sub").is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let token = sign(
            json!({ "sub": "jplock", "iss": "https://evil.example", "exp": 9_999_999_999_u64 }),
        );
        let key = DecodingKey::from_secret(SECRET);
        assert!(extract_principal(&token, &key, &validation(), "sub").is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let token = sign(json!({ "sub": "jplock", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(b"a-different-secret");
        assert!(extract_principal(&token, &key, &validation(), "sub").is_err());
    }

    #[test]
    fn missing_claim_rejected() {
        let token = sign(json!({ "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(extract_principal(&token, &key, &validation(), "sub").is_err());
    }

    #[test]
    fn empty_principal_rejected() {
        let token = sign(json!({ "sub": "", "iss": ISSUER, "exp": 9_999_999_999_u64 }));
        let key = DecodingKey::from_secret(SECRET);
        assert!(extract_principal(&token, &key, &validation(), "sub").is_err());
    }

    #[test]
    fn configurable_claim() {
        let token = sign(
            json!({ "preferred_username": "agent-42", "iss": ISSUER, "exp": 9_999_999_999_u64 }),
        );
        let key = DecodingKey::from_secret(SECRET);
        let principal =
            extract_principal(&token, &key, &validation(), "preferred_username").unwrap();
        assert_eq!(principal, Principal("agent-42".to_string()));
    }
}
