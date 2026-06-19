//! Devbox host agent.
//!
//! A single small binary baked into the golden AMI that owns the host side of
//! SSH access and warm-up:
//!
//! - `principals <login-user>` — sshd `AuthorizedPrincipalsCommand` resolver.
//! - `owner-sync` — provision the claimant's Unix account (long-running service).
//! - `warmup` — release the ASG launch lifecycle hook once the host is ready.

mod imds;
mod owner_sync;
mod principals;
mod warmup;

use std::process::ExitCode;

use anyhow::{Context, Result};
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
    /// Provision the claimant's Unix login account from the `devbox:owner` tag.
    /// Runs as a long-lived systemd service.
    OwnerSync,
    /// Warm the host and signal CONTINUE to the ASG launch lifecycle hook.
    Warmup,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Principals { login_user } => {
            // Fail closed and silent: never block a login on resolver errors.
            principals::run(&login_user);
            ExitCode::SUCCESS
        }
        Command::OwnerSync => {
            init_tracing();
            owner_sync::run();
        }
        Command::Warmup => {
            init_tracing();
            match run_warmup() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "warm-up failed");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Run the async warm-up flow on a small current-thread runtime.
fn run_warmup() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(warmup::run())
}

/// Initialize structured logging to stderr (journald captures it under systemd).
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
