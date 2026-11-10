//! Devbox host agent.
//!
//! A single small binary baked into the golden AMI that owns the host side of
//! SSH access and warm-up:
//!
//! - `principals <login-user>` — sshd `AuthorizedPrincipalsCommand` resolver.
//! - `owner-sync` — provision the claimant's Unix account, then exit.
//! - `warmup` — warm the host and self-tag `devbox:ready=true` so the reconciler
//!   marks the `DevboxDoc` Ready; boxes that never tag ready are reaped.
//! - `doctor` — print a read-only diagnostic of warm-cache delivery.
//! - `sandbox` — run sub-agents in Nitro Sandboxes (hardware-isolated child VMs)
//!   on a claimed box, each on its own clone, with work returned as a git branch.
//! - `sandboxd` — the root daemon (systemd) that jails and runs sandbox VMMs.

mod checkout;
mod control_plane;
mod doctor;
mod freshen;
mod git;
mod git_credential;
mod imds;
mod owner_sync;
mod principals;
mod sandbox;
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
    /// Git credential helper (configured by owner-sync): supply the box's
    /// web-identity token as the password for reverse-proxy git traffic, so the
    /// GitHub credential is minted server-side and never held by the box.
    GitCredential {
        /// The git credential operation (`get`, `store`, or `erase`).
        operation: String,
    },
    /// Run sub-agents in Nitro Sandboxes: each gets its own clone of the repo on
    /// branch `agent/<name>`, and its work comes back as that branch.
    Sandbox {
        #[command(subcommand)]
        command: sandbox::SandboxCommand,
    },
    /// Run the sandbox daemon (root, under systemd): jails and runs sandbox
    /// VMMs and their vsock relays for `sandbox` commands.
    Sandboxd,
    /// The init inside a sandbox guest (run by the guest image's
    /// `devbox-guest.service`, not on the host). Linux only, like vsock.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    Guest {
        #[command(subcommand)]
        command: Option<GuestCommand>,
    },
}

/// `devbox-agent guest` subcommands (none: run the init).
#[cfg(target_os = "linux")]
#[derive(Subcommand)]
enum GuestCommand {
    /// Connect stdin/stdout to host vsock port PORT (git's `ext::` transport).
    VsockPipe {
        /// Host vsock port.
        port: u32,
    },
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
        Command::GitCredential { operation } => {
            // git parses stdout: emit only the credential protocol, no tracing.
            git_credential::run(&operation).await;
            ExitCode::SUCCESS
        }
        Command::Sandboxd => {
            init_tracing();
            let config = sandbox::daemon::Config::from_env().await;
            match sandbox::daemon::serve(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "sandbox daemon failed");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(target_os = "linux")]
        Command::Guest {
            command: Some(GuestCommand::VsockPipe { port }),
        } => {
            // git speaks its protocol on stdout: errors only, on stderr.
            match sandbox::guest::vsock_pipe(port).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vsock-pipe: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(target_os = "linux")]
        Command::Guest { command: None } => {
            init_tracing();
            match sandbox::guest::run(sandbox::guest::Config::guest()).await {
                Ok(ending) => {
                    tracing::info!(?ending, "sandbox guest finished");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "sandbox guest failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Sandbox { command } => {
            // Human-facing output on stdout; warnings (and the relay's logs) on
            // stderr via tracing; the final error on stderr, like `doctor`.
            init_tracing();
            match sandbox::run(command).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
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
