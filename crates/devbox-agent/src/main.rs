//! Devbox host agent.
//!
//! A single small binary baked into the golden AMI that owns the host side of
//! SSH access and warm-up:
//!
//! - `principals <login-user>` — sshd `AuthorizedPrincipalsCommand` resolver.
//! - `owner-sync` — provision the claimant's Unix account (restoring a session
//!   when `claim --resume` asked for one), then exit.
//! - `warmup` — warm the host and self-tag `devbox:ready=true` so the reconciler
//!   marks the `DevboxDoc` Ready; boxes that never tag ready are reaped.
//! - `session-watch` — archive the session to S3 when `release --keep` asks.
//! - `doctor` — print a read-only diagnostic of warm-cache delivery.

mod checkout;
mod control_plane;
mod doctor;
mod freshen;
mod git;
mod imds;
mod owner_sync;
mod principals;
mod session;
mod session_restore;
mod session_watch;
mod warmup;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Devbox host agent: SSH principal resolution, account provisioning, warm-up.
#[derive(Parser)]
#[command(name = "devbox-agent", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// sshd AuthorizedPrincipalsCommand resolver: print the authorized principal
    /// for LOGIN_USER (the `devbox:owner` tag), or nothing.
    Principals {
        /// The login account sshd is resolving principals for (`%u`).
        login_user: String,
    },
    /// Provision the claimant's Unix login account from the `devbox:owner` tag,
    /// then exit. Runs as a systemd service (poll while unclaimed).
    OwnerSync,
    /// Warm the host and self-tag the instance `devbox:ready=true` via EC2.
    Warmup,
    /// Clone the given repos onto the workspace, minting a read-only token per repo.
    Checkout {
        /// Target workspace directory.
        #[arg(long, default_value = "/workspace")]
        workspace: PathBuf,
        /// Repo clone URLs (one checkout each under the workspace).
        #[arg(required = true)]
        repos: Vec<String>,
    },
    /// Print a read-only diagnostic of warm-cache delivery (workspace mount,
    /// resolved RUSTUP_HOME/CARGO_HOME, pinned-toolchain and registry presence).
    Doctor,
    /// Watch for a `devbox:archive-session` tag (release --keep); on seeing it,
    /// pack the session, upload it via a presigned URL, report, and exit.
    SessionWatch,
}

// Current-thread runtime: the agent reads IMDS / calls the AWS SDK (both async)
// but has no parallelism to exploit, and `owner-sync` runs until it exits.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Principals { login_user } => {
            // Fail closed and silent: never block a login on resolver errors.
            principals::run(&login_user).await;
            ExitCode::SUCCESS
        }
        Command::OwnerSync => {
            init_tracing();
            owner_sync::run().await;
            ExitCode::SUCCESS
        }
        Command::Warmup => {
            init_tracing();
            match warmup::run().await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "warm-up failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Checkout { workspace, repos } => {
            init_tracing();
            match checkout::run(&workspace, &repos).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "checkout failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Doctor => {
            // Human-facing report on stdout; no tracing setup (keep it clean).
            match doctor::run().await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("doctor failed: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::SessionWatch => {
            init_tracing();
            match session_watch::run().await {
                Ok(()) => ExitCode::SUCCESS,
                // Non-zero exit → systemd Restart=on-failure retries the
                // archive; the request tag stays set until the box terminates.
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "session-watch failed");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Initialize structured logging to stderr (journald captures it under systemd).
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
