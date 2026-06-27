//! Server-side GitHub App integration.
//!
//! The control plane owns the GitHub App private key (read from an SSM
//! SecureString via the task role) and mints short-lived, repo-scoped,
//! read-only installation tokens on behalf of devbox hosts. The host never holds
//! the key — it requests a token over the agent API ([`crate::routes`]) and the
//! server mints it here.

mod minter;

pub use minter::Minter;
