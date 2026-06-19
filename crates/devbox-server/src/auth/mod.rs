//! Request authentication for the control-plane API.
//!
//! The control plane sits behind an internal ALB that gates the dashboard with
//! Vouch OIDC. Two identity sources are accepted, in order:
//!
//! 1. `x-amzn-oidc-data` — the signed JWT the ALB injects on OIDC-authenticated
//!    (dashboard) requests, verified against the ALB's regional public key.
//! 2. `Authorization: Bearer <jwt>` — a Vouch-issued token (the CLI / agents),
//!    verified against Vouch's JWKS.
//!
//! Either way we extract a single **principal** claim. Claim/release then bind
//! `owner` to that principal, so a caller can only act as the identity they
//! authenticated as — the same Vouch identity namespace the SSH cert uses.

mod jwt;

pub use jwt::{AuthConfig, AuthError, Authenticator, Principal};
