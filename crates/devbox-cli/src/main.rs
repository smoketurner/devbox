//! Devbox CLI client.

mod auth;
mod format;
mod session;
mod ssh;
mod state;

use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dialoguer::Select;

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, InstanceType, ReleaseRequest,
    is_valid_unix_username,
};

/// Devbox CLI - manage remote development environments.
#[derive(Parser)]
#[command(name = "devbox", version, about)]
struct Cli {
    /// Server URL to connect to.
    #[arg(
        long,
        default_value = "http://localhost:3000",
        global = true,
        env = "DEVBOX_SERVER"
    )]
    server: String,

    #[command(subcommand)]
    command: Commands,
}

/// Attach a Bearer token to a request when one is present.
fn with_auth(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => builder.bearer_auth(t),
        None => builder,
    }
}

/// Whether we can safely open an interactive prompt. `dialoguer` renders to and
/// reads keys from the terminal, so every standard stream must be a TTY —
/// otherwise (piped/redirected stdout, scripts, CI) the picker would render
/// nowhere or fail mid-read. In that case we fall back to a listing error.
fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

/// Resolve the target devbox id for `ssh`/`status`/`release`.
///
/// An explicit `--id` always wins. Otherwise we consult the local registry of
/// active claims for this server: zero is an error, one is used directly, and
/// several open an interactive picker (or, on a non-TTY, an error listing them).
fn resolve_id(explicit: Option<String>, server: &str) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(id);
    }

    let claims = state::active_claims(server)?;
    match claims.len() {
        0 => bail!("no active devbox for {server}; run `devbox claim` first or pass --id"),
        1 => {
            let claim = claims
                .into_iter()
                .next()
                .context("active claim disappeared while resolving id")?;
            Ok(claim.id)
        }
        _ if is_interactive() => {
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
            bail!(
                "multiple active devboxes ({}); pass --id to choose",
                ids.join(", ")
            )
        }
    }
}

/// Resolve the owner for `claim`/`release`.
///
/// Precedence: cached session (email-derived login) → `$USER` fallback
/// (gated by [`is_valid_unix_username`]). The `$USER` path serves the local-dev
/// case where auth is disabled on the server.
fn resolve_owner() -> Result<String> {
    if let Some(s) = session::current()? {
        return Ok(s.owner);
    }
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty());
    let Some(user) = user else {
        bail!("not logged in; run `devbox login`")
    };
    if !is_valid_unix_username(&user) {
        bail!("$USER '{user}' is not a valid devbox owner; run `devbox login`");
    }
    Ok(user)
}

/// Record a freshly claimed devbox in the local registry. Best-effort: the box is
/// already claimed, so a write failure only warns.
fn remember_claim(devbox: &DevboxResponse, server: &str) {
    let claim = state::Claim {
        id: devbox.id.clone(),
        server_url: server.to_string(),
        claimed_at: devbox.claimed_at.clone(),
    };
    if let Err(e) = state::add(claim) {
        eprintln!("warning: could not record claim locally: {e:#}");
    }
}

/// Drop a released devbox from the local registry. Best-effort.
fn forget_claim(id: &str, server: &str) {
    if let Err(e) = state::remove(id, server) {
        eprintln!("warning: could not update local claim registry: {e:#}");
    }
}

/// Prune local entries this server no longer reports as `Claimed`. Best-effort —
/// never fails `list`.
fn reconcile_claims(list: &DevboxListResponse, server: &str) {
    let claimed: BTreeSet<String> = list
        .devboxes
        .iter()
        .filter(|d| d.state == DevboxState::Claimed)
        .map(|d| d.id.clone())
        .collect();
    if let Err(e) = state::reconcile(server, &claimed) {
        eprintln!("warning: could not reconcile local claim registry: {e:#}");
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate to the devbox server via device-code OAuth.
    Login,
    /// Forget the cached session (keeps the registered OAuth client).
    Logout,
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
    let http = reqwest::Client::new();

    match cli.command {
        Commands::Login => match auth::login(&http, &cli.server).await? {
            auth::LoginOutcome::LoggedIn(s) => {
                println!("logged in as {} ({})", s.owner, s.email);
            }
            auth::LoginOutcome::AuthDisabled => {
                println!("auth not enabled on this server");
            }
        },

        Commands::Logout => {
            session::logout()?;
            println!("logged out");
        }

        Commands::Claim { instance_type } => {
            let owner = resolve_owner()?;
            let token = session::current()?.map(|s| s.id_token);
            let url = format!("{}/api/v1/devboxes/claim", cli.server);
            let req = ClaimRequest {
                owner,
                instance_type: instance_type.map(InstanceType),
            };
            let resp = with_auth(http.post(&url).json(&req), token.as_deref())
                .send()
                .await
                .context("failed to send claim request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                remember_claim(&devbox, &cli.server);
                println!("{}", format::format_claim_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }

        Commands::Release { id } => {
            let owner = resolve_owner()?;
            let token = session::current()?.map(|s| s.id_token);
            let id = resolve_id(id, &cli.server)?;
            let url = format!("{}/api/v1/devboxes/{}/release", cli.server, id);
            let req = ReleaseRequest { owner };
            let resp = with_auth(http.post(&url).json(&req), token.as_deref())
                .send()
                .await
                .context("failed to send release request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                forget_claim(&id, &cli.server);
                println!("{}", format::format_release_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }

        Commands::List => {
            let token = session::current()?.map(|s| s.id_token);
            let url = format!("{}/api/v1/devboxes", cli.server);
            let resp = with_auth(http.get(&url), token.as_deref())
                .send()
                .await
                .context("failed to send list request")?;

            if resp.status().is_success() {
                let list: DevboxListResponse =
                    resp.json().await.context("failed to parse response")?;
                reconcile_claims(&list, &cli.server);
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
            let token = session::current()?.map(|s| s.id_token);
            let id = resolve_id(id, &cli.server)?;
            let url = format!("{}/api/v1/devboxes/{}", cli.server, id);
            let resp = with_auth(http.get(&url), token.as_deref())
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
            let token = session::current()?.map(|s| s.id_token);
            let id = resolve_id(id, &cli.server)?;
            let url = format!("{}/api/v1/devboxes/{}", cli.server, id);
            let resp = with_auth(http.get(&url), token.as_deref())
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
        // Verify the CLI definition is valid (includes new Login/Logout subcommands).
        Cli::command().debug_assert();
    }
}
