//! Server-side GitHub App integration.
//!
//! The control plane owns the GitHub App private key (read from an SSM
//! SecureString via the task role) and mints short-lived, repo-scoped,
//! read-only installation tokens on behalf of devbox hosts. The host never holds
//! the key — it requests a token over the agent API ([`crate::routes`]) and the
//! server mints it here.
//!
//! The same App backs a read-only git reverse proxy ([`git_proxy`]) for the
//! claimant's live fetch traffic, injecting the token upstream so the box never
//! holds it.

pub(crate) mod git_proxy;
mod minter;

pub use git_proxy::GitProxy;
pub use minter::Minter;
