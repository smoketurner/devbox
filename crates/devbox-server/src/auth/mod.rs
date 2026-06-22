//! Request authentication for the control-plane API and dashboard.
//!
//! Identity sources, in order:
//!
//! 1. `x-amzn-oidc-data` — the signed JWT an ALB injects on OIDC-authenticated
//!    requests, verified against the ALB's regional public key (legacy / when
//!    fronted by an ALB).
//! 2. `Authorization: Bearer <jwt>` — a Vouch-issued token (the CLI / agents),
//!    verified against Vouch's JWKS.
//! 3. A `devbox_session` cookie holding a Vouch OIDC **ID token**, set by the
//!    app-side Authorization Code login ([`Authenticator::authorize_url`],
//!    [`Authenticator::exchange_code`], [`Authenticator::verify_id_token`]) and
//!    used to gate the HTML dashboard when no ALB is in front of it.
//!
//! Either way we extract a single **principal** claim. Claim/release then bind
//! `owner` to that principal, so a caller can only act as the identity they
//! authenticated as — the same Vouch identity namespace the SSH cert uses.

mod jwt;

pub(crate) use jwt::random_token;
pub use jwt::{AuthConfig, AuthError, Authenticator, OidcConfig, Principal};
