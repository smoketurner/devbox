//! Devbox CLI client.

mod format;
mod ssh;
mod state;
mod token;

use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dialoguer::Select;

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, InstanceType, ReleaseRequest,
};

/// Devbox CLI - manage remote development environments.
#[derive(Parser)]
#[command(name = "devbox", version, about)]
struct Cli {
    /// Server URL to connect to.
    #[arg(long, default_value = "http://localhost:3000", global = true)]
    server_url: String,

    /// Bearer token (Vouch OIDC) for authenticated API calls.
    #[arg(long, env = "DEVBOX_TOKEN", global = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// Attach a Bearer token to a request when one is configured.
fn with_auth(builder: reqwest::RequestBuilder, token: Option<&String>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => builder.bearer_auth(token),
        None => builder,
    }
}

/// Resolve the target devbox id for `ssh`/`status`/`release`.
///
/// An explicit `--id` always wins. Otherwise we consult the local registry of
/// active claims for this server: zero is an error, one is used directly, and
/// several open an interactive picker (or, on a non-TTY, an error listing them).
fn resolve_id(explicit: Option<String>, server_url: &str) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(id);
    }

    let claims = state::active_claims(server_url)?;
    match claims.len() {
        0 => anyhow::bail!(
            "no active devbox for {server_url}; run `devbox claim` first or pass --id"
        ),
        1 => {
            let claim = claims
                .into_iter()
                .next()
                .context("active claim disappeared while resolving id")?;
            Ok(claim.id)
        }
        _ if std::io::stdin().is_terminal() => {
            let labels: Vec<String> = claims
                .iter()
                .map(|c| match &c.claimed_at {
                    Some(at) => format!("{}  (claimed {at})", c.id),
                    None => c.id.clone(),
                })
                .collect();
            let choice = Select::new()
                .with_prompt("Select a devbox")
                .items(&labels)
                .default(0)
                .interact()
                .context("devbox selection cancelled")?;
            claims
                .get(choice)
                .map(|c| c.id.clone())
                .context("invalid devbox selection")
        }
        _ => {
            let ids: Vec<&str> = claims.iter().map(|c| c.id.as_str()).collect();
            anyhow::bail!(
                "multiple active devboxes ({}); pass --id to choose",
                ids.join(", ")
            )
        }
    }
}

/// Record a freshly claimed devbox in the local registry. Best-effort: the box is
/// already claimed, so a write failure only warns.
fn remember_claim(devbox: &DevboxResponse, server_url: &str) {
    let claim = state::Claim {
        id: devbox.id.clone(),
        server_url: server_url.to_string(),
        claimed_at: devbox.claimed_at.clone(),
    };
    if let Err(e) = state::add(claim) {
        eprintln!("warning: could not record claim locally: {e:#}");
    }
}

/// Drop a released devbox from the local registry. Best-effort.
fn forget_claim(id: &str, server_url: &str) {
    if let Err(e) = state::remove(id, server_url) {
        eprintln!("warning: could not update local claim registry: {e:#}");
    }
}

/// Prune local entries this server no longer reports as `Claimed`. Best-effort —
/// never fails `list`.
fn reconcile_claims(list: &DevboxListResponse, server_url: &str) {
    let claimed: BTreeSet<String> = list
        .devboxes
        .iter()
        .filter(|d| d.state == DevboxState::Claimed)
        .map(|d| d.id.clone())
        .collect();
    if let Err(e) = state::reconcile(server_url, &claimed) {
        eprintln!("warning: could not reconcile local claim registry: {e:#}");
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Claim an available devbox.
    Claim {
        /// Preferred instance type.
        #[arg(long)]
        instance_type: Option<String>,
    },
    /// Release a claimed devbox.
    Release {
        /// Devbox ID to release (defaults to your active claim).
        #[arg(long)]
        id: Option<String>,
    },
    /// List all devboxes.
    List,
    /// Get status of a specific devbox.
    Status {
        /// Devbox ID to check (defaults to your active claim).
        #[arg(long)]
        id: Option<String>,
    },
    /// SSH into a claimed devbox over an SSM tunnel.
    Ssh {
        /// Devbox ID to connect to (defaults to your active claim).
        #[arg(long)]
        id: Option<String>,
        /// Login user (defaults to the devbox owner / certificate principal).
        #[arg(long)]
        user: Option<String>,
        /// AWS region for the SSM tunnel (defaults to your aws CLI config).
        #[arg(long)]
        region: Option<String>,
        /// AWS profile for the SSM tunnel.
        #[arg(long)]
        profile: Option<String>,
        /// Print the ssh command instead of running it.
        #[arg(long)]
        print: bool,
        /// Arguments passed through to ssh (e.g. a remote command after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Commands::Claim { instance_type } => {
            let owner = token::owner(cli.token.as_deref())?;
            let url = format!("{}/api/v1/devboxes/claim", cli.server_url);
            let req = ClaimRequest {
                owner,
                instance_type: instance_type.map(InstanceType),
            };
            let resp = with_auth(client.post(&url).json(&req), cli.token.as_ref())
                .send()
                .await
                .context("failed to send claim request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                remember_claim(&devbox, &cli.server_url);
                println!("{}", format::format_claim_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }
        Commands::Release { id } => {
            let owner = token::owner(cli.token.as_deref())?;
            let id = resolve_id(id, &cli.server_url)?;
            let url = format!("{}/api/v1/devboxes/{}/release", cli.server_url, id);
            let req = ReleaseRequest { owner };
            let resp = with_auth(client.post(&url).json(&req), cli.token.as_ref())
                .send()
                .await
                .context("failed to send release request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                forget_claim(&id, &cli.server_url);
                println!("{}", format::format_release_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }
        Commands::List => {
            let url = format!("{}/api/v1/devboxes", cli.server_url);
            let resp = with_auth(client.get(&url), cli.token.as_ref())
                .send()
                .await
                .context("failed to send list request")?;

            if resp.status().is_success() {
                let list: DevboxListResponse =
                    resp.json().await.context("failed to parse response")?;
                reconcile_claims(&list, &cli.server_url);
                if list.devboxes.is_empty() {
                    println!("No devboxes found.");
                } else {
                    println!("{}", format::format_list_table(&list));
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }
        Commands::Status { id } => {
            let id = resolve_id(id, &cli.server_url)?;
            let url = format!("{}/api/v1/devboxes/{}", cli.server_url, id);
            let resp = with_auth(client.get(&url), cli.token.as_ref())
                .send()
                .await
                .context("failed to send status request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                println!("{}", format::format_status(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }
        Commands::Ssh {
            id,
            user,
            region,
            profile,
            print,
            args,
        } => {
            let id = resolve_id(id, &cli.server_url)?;
            let url = format!("{}/api/v1/devboxes/{}", cli.server_url, id);
            let resp = with_auth(client.get(&url), cli.token.as_ref())
                .send()
                .await
                .context("failed to look up devbox")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }

            let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
            let opts = ssh::SshOptions {
                user,
                region,
                profile,
                print,
                extra: args,
            };
            ssh::connect(&devbox, &opts)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses() {
        // Verify the CLI definition is valid
        Cli::command().debug_assert();
    }
}
