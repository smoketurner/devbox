//! Server-side GitHub App integration.
//!
//! The control plane owns the GitHub App private key (read from an SSM
//! SecureString via the task role) and mints short-lived, repo-scoped
//! installation tokens on behalf of devbox hosts. The host never holds the
//! key — it requests a token over the agent API ([`crate::routes`]) and the
//! server mints it here.
//!
//! The same App backs the git reverse proxy ([`git_proxy`]) for the claimant's
//! live traffic — fetch for any devbox host, push gated on a claimed box —
//! injecting the token upstream so the box never holds it.

mod app;
pub(crate) mod git_proxy;

pub use app::GitHubApp;
pub use git_proxy::GitProxy;
