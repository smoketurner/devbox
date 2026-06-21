//! Deriving the caller's principal (owner) from the session token.
//!
//! Owner is never a CLI flag: for `claim`/`release` we read it from the Vouch
//! JWT (`--token` / `DEVBOX_TOKEN`). The CLI decodes the token's payload
//! **without verifying the signature** — the server performs the real
//! verification and re-binds `owner` to the verified principal; the CLI only
//! needs the value locally for the request body and UX. This mirrors the
//! server's `extract_principal` (`crates/devbox-server/src/auth/jwt.rs`).

use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

/// Decode `token` and return its `sub` claim. The signature is **not** verified.
fn principal_from_token(token: &str) -> Result<String> {
    // Signature/expiry/audience checks are the server's job; here we only want
    // the claim value, so disable every check and decode with a dummy key.
    let mut validation = Validation::new(Algorithm::HS256);
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();

    let key = DecodingKey::from_secret(&[]);
    let data = decode::<serde_json::Value>(token, &key, &validation)
        .context("failed to decode token; is DEVBOX_TOKEN a valid JWT?")?;

    let sub = data
        .claims
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .context("token is missing a 'sub' claim")?;

    if sub.is_empty() {
        anyhow::bail!("token has an empty 'sub' claim");
    }
    Ok(sub.to_string())
}

/// Resolve the owner for `claim`/`release` from the session token.
///
/// A token is required: there is no flag-based override. Without one this errors
/// so a misconfigured environment fails fast rather than claiming as the wrong
/// identity.
pub(crate) fn owner(token: Option<&str>) -> Result<String> {
    let token = token.context("DEVBOX_TOKEN (or --token) is required to claim/release")?;
    principal_from_token(token)
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

    /// Sign a token with an arbitrary secret — the CLI never checks the signature.
    fn sign(claims: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"irrelevant-secret"),
        )
        .unwrap()
    }

    #[test]
    fn reads_sub_ignoring_signature_and_expiry() {
        // Bogus-signed and long-expired: still readable since we don't verify.
        let token = sign(json!({ "sub": "alice", "exp": 1_u64 }));
        assert_eq!(principal_from_token(&token).unwrap(), "alice");
    }

    #[test]
    fn errors_on_missing_sub() {
        let token = sign(json!({ "iss": "vouch" }));
        assert!(principal_from_token(&token).is_err());
    }

    #[test]
    fn errors_on_empty_sub() {
        let token = sign(json!({ "sub": "" }));
        assert!(principal_from_token(&token).is_err());
    }

    #[test]
    fn owner_requires_a_token() {
        assert!(owner(None).is_err());
        let token = sign(json!({ "sub": "bob" }));
        assert_eq!(owner(Some(&token)).unwrap(), "bob");
    }
}
