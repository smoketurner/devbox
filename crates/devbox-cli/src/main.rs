//! Devbox CLI client.

mod auth;
mod aws_profile;
mod format;
mod session;
mod ssh;
mod ssm;
mod state;

use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dialoguer::Select;

use devbox_common::{ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, InstanceType};

/// Default server used before the first `devbox login` (and when no
/// `--server`/`$DEVBOX_SERVER` is given and none has been remembered).
const DEFAULT_SERVER: &str = "http://localhost:3000";

/// Devbox CLI - manage remote development environments.
#[derive(Parser)]
#[command(name = "devbox", version, about)]
struct Cli {
    /// Server URL to connect to. Defaults to the server from your last
    /// `devbox login`, then to http://localhost:3000.
    #[arg(long, global = true, env = "DEVBOX_SERVER")]
    server: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// Resolve the server to talk to: an explicit `--server`/`$DEVBOX_SERVER`, else
/// the server remembered from the last `devbox login`, else [`DEFAULT_SERVER`].
fn resolve_server(explicit: Option<String>) -> Result<String> {
    if let Some(server) = explicit {
        return Ok(server);
    }
    if let Some(server) = session::current_server()? {
        return Ok(server);
    }
    Ok(DEFAULT_SERVER.to_string())
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

/// Auto-select the AWS profile for the SSM tunnel by matching the control
/// plane's account, unless the caller already pins credentials via the
/// environment. Returns `None` (use the caller's default credentials) when the
/// environment is already set, the server advertises no account, or it is
/// unreachable — so behaviour is never worse than passing no `--profile`.
async fn resolve_aws_profile(http: &reqwest::Client, server: &str) -> Result<Option<String>> {
    // Respect an explicit AWS environment — never override the caller's creds.
    let env_set = |key: &str| std::env::var_os(key).is_some_and(|v| !v.is_empty());
    if env_set("AWS_PROFILE") || env_set("AWS_ACCESS_KEY_ID") {
        return Ok(None);
    }

    // The account the control plane advertises in its discovery document. A
    // missing field, or an unreachable/out-of-date server, means no auto-select.
    let Some(account_id) = auth::fetch_protected_resource(http, server)
        .await
        .ok()
        .and_then(|prm| prm.aws_account_id)
    else {
        return Ok(None);
    };

    aws_profile::select_profile(&account_id, is_interactive())
}

/// The cached session's bearer token, or an error directing the user to log in.
///
/// Authentication is mandatory: the server binds `owner` to the authenticated
/// principal, so claim/release always need a valid token.
fn require_token(server: &str) -> Result<String> {
    let Some(session) = session::current(server)? else {
        bail!("not logged in to {server}; run `devbox login`")
    };
    Ok(session.token)
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

/// Prune local entries this server no longer reports as `Claimed` **by the
/// current owner**. Best-effort — never fails `list`.
///
/// Filtering by owner matters: a box re-claimed by a different user is still
/// `Claimed`, so an owner-blind reconcile would keep our stale local entry and
/// later drive `ssh <other-owner>@…` into a `Permission denied`. The caller skips
/// reconcile entirely when no owner is available, rather than pruning blind.
fn reconcile_claims(list: &DevboxListResponse, server: &str, owner: &str) {
    let claimed = live_claimed_ids(list, owner);
    if let Err(e) = state::reconcile(server, &claimed) {
        eprintln!("warning: could not reconcile local claim registry: {e:#}");
    }
}

/// The ids the server reports as `Claimed` by `owner` — the set the local
/// registry should be reconciled against.
fn live_claimed_ids(list: &DevboxListResponse, owner: &str) -> BTreeSet<String> {
    list.devboxes
        .iter()
        .filter(|d| d.state == DevboxState::Claimed && d.owner.as_deref() == Some(owner))
        .map(|d| d.id.clone())
        .collect()
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
        /// AWS profile for the SSM tunnel (auto-selected by the control-plane
        /// account when omitted).
        #[arg(long)]
        profile: Option<String>,
        /// Print the ssh command instead of running it.
        #[arg(long)]
        print: bool,
        /// Arguments passed through to ssh (e.g. a remote command after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Internal: native SSM data-channel proxy used as an ssh `ProxyCommand`.
    /// Not meant to be run directly; `devbox ssh` wires it up automatically.
    #[command(hide = true)]
    SsmProxy {
        /// Target EC2 instance id (ssh substitutes `%h`).
        #[arg(long)]
        target: String,
        /// AWS region the instance runs in.
        #[arg(long)]
        region: String,
        /// SSH port on the instance (ssh substitutes `%p`).
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// AWS profile for SSM credentials.
        #[arg(long)]
        profile: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let http = reqwest::Client::new();
    let server = resolve_server(cli.server)?;

    match cli.command {
        Commands::Login => {
            let s = auth::login(&http, &server).await?;
            println!("logged in as {} ({}) on {server}", s.owner, s.email);
        }

        Commands::Logout => {
            session::logout(&server)?;
            println!("logged out of {server}");
        }

        Commands::Claim { instance_type } => {
            let token = require_token(&server)?;
            let url = format!("{server}/api/v1/devboxes/claim");
            let req = ClaimRequest {
                instance_type: instance_type.map(InstanceType),
            };
            let resp = with_auth(http.post(&url).json(&req), Some(&token))
                .send()
                .await
                .context("failed to send claim request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                remember_claim(&devbox, &server);
                println!("{}", format::format_claim_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }

        Commands::Release { id } => {
            let token = require_token(&server)?;
            let id = resolve_id(id, &server)?;
            let url = format!("{server}/api/v1/devboxes/{id}/release");
            let resp = with_auth(http.post(&url), Some(&token))
                .send()
                .await
                .context("failed to send release request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                forget_claim(&id, &server);
                println!("{}", format::format_release_success(&devbox));
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("{} {}", status.as_u16(), body);
                std::process::exit(1);
            }
        }

        Commands::List => {
            // Reads are open; a missing or unreadable session must not block them,
            // so attach a token only if one is cleanly available (best-effort). The
            // same session's owner gates reconcile so we never prune ownership-blind.
            let session = session::current(&server).ok().flatten();
            let token = session.as_ref().map(|s| s.token.clone());
            let url = format!("{server}/api/v1/devboxes");
            let resp = with_auth(http.get(&url), token.as_deref())
                .send()
                .await
                .context("failed to send list request")?;

            if resp.status().is_success() {
                let list: DevboxListResponse =
                    resp.json().await.context("failed to parse response")?;
                if let Some(session) = &session {
                    reconcile_claims(&list, &server, &session.owner);
                }
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
            // Reads are open; a missing or unreadable session must not block them,
            // so attach a token only if one is cleanly available (best-effort).
            let token = session::current(&server).ok().flatten().map(|s| s.token);
            let id = resolve_id(id, &server)?;
            let url = format!("{server}/api/v1/devboxes/{id}");
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
            profile,
            print,
            args,
        } => {
            // Reads are open; a missing or unreadable session must not block them,
            // so attach a token only if one is cleanly available (best-effort).
            let token = session::current(&server).ok().flatten().map(|s| s.token);
            let id = resolve_id(id, &server)?;
            let url = format!("{server}/api/v1/devboxes/{id}");
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
            // With no explicit --profile, auto-select the AWS profile that
            // matches the control plane's account so the SSM tunnel "just works".
            let profile = match profile {
                Some(profile) => Some(profile),
                None => resolve_aws_profile(&http, &server).await?,
            };
            let opts = ssh::SshOptions {
                profile,
                print,
                extra: args,
            };
            ssh::connect(&devbox, &opts)?;
        }

        Commands::SsmProxy {
            target,
            region,
            port,
            profile,
        } => {
            ssm::run_proxy(&target, &region, port, profile.as_deref()).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use devbox_common::AmiId;

    #[test]
    fn test_cli_parses() {
        // Verify the CLI definition is valid (includes new Login/Logout subcommands).
        Cli::command().debug_assert();
    }

    fn devbox(id: &str, state: DevboxState, owner: Option<&str>) -> DevboxResponse {
        DevboxResponse {
            id: id.to_string(),
            instance_id: "i-1234567890abcdef0".to_string(),
            state,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            owner: owner.map(str::to_string),
            region: "us-east-1".to_string(),
            created_at: "2026-06-23T00:00:00Z".to_string(),
            claimed_at: None,
        }
    }

    #[test]
    fn live_claimed_ids_keeps_only_current_owner() {
        let list = DevboxListResponse {
            devboxes: vec![
                devbox("mine", DevboxState::Claimed, Some("jdoe")),
                devbox("theirs", DevboxState::Claimed, Some("asmith")),
                devbox("ready", DevboxState::Ready, None),
            ],
        };
        let ids = live_claimed_ids(&list, "jdoe");
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("mine"));
        // A box re-claimed by another user must not be retained as ours.
        assert!(!ids.contains("theirs"));
    }
}
