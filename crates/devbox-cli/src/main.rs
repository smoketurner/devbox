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

/// Attach the caller's bearer token to a request. Every API endpoint requires
/// authentication (only `/health` and the discovery document are open), so the
/// token is always present.
fn with_auth(builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    builder.bearer_auth(token)
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
/// active claims for this server. When it is empty — e.g. the box was claimed
/// from another machine or directly via the API — we fall back to the server,
/// scoped to the authenticated owner, and remember the box we resolve to so the
/// next call is a local read again.
async fn resolve_id(
    explicit: Option<String>,
    server: &str,
    http: &reqwest::Client,
    session: &session::Session,
) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(id);
    }

    let local = state::active_claims(server)?;
    if !local.is_empty() {
        return Ok(select_claim(local, server)?.id);
    }

    // Empty registry: discover the owner's claims from the server and adopt only
    // the one we resolve to (a narrow read-through cache; `list` stays prune-only).
    let discovered = discover_claims(http, server, session).await?;
    let chosen = select_claim(discovered, server)?;
    remember(chosen.clone());
    Ok(chosen.id)
}

/// Choose one claim: zero is an error, one is used directly, and several open an
/// interactive picker (or, on a non-TTY, an error listing the candidate ids).
fn select_claim(claims: Vec<state::Claim>, server: &str) -> Result<state::Claim> {
    match claims.len() {
        0 => bail!("no active devbox for {server}; run `devbox claim` first or pass --id"),
        1 => claims
            .into_iter()
            .next()
            .context("active claim disappeared while resolving id"),
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
                .into_iter()
                .nth(choice)
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

/// The cached session for `server`, or an error directing the user to log in.
///
/// Authentication is mandatory for mutating calls: the server binds `owner` to
/// the authenticated principal, so claim/release always need a valid session.
fn require_session(server: &str) -> Result<session::Session> {
    session::current(server)?.with_context(|| {
        format!("not logged in to {server} (or your session expired); run `devbox login`")
    })
}

/// Persist `claim` in the local registry, warning (not failing) on error — the
/// box is already claimed, so a local write failure is non-fatal.
fn remember(claim: state::Claim) {
    if let Err(e) = state::add(claim) {
        eprintln!("warning: could not record claim locally: {e:#}");
    }
}

/// Record a freshly claimed devbox in the local registry.
fn remember_claim(devbox: &DevboxResponse, server: &str) {
    remember(state::Claim {
        id: devbox.id.clone(),
        server_url: server.to_string(),
        claimed_at: devbox.claimed_at.clone(),
    });
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

/// Whether `d` is currently `Claimed` by `owner`.
fn claimed_by(d: &DevboxResponse, owner: &str) -> bool {
    d.state == DevboxState::Claimed && d.owner.as_deref() == Some(owner)
}

/// The ids the server reports as `Claimed` by `owner` — the set the local
/// registry should be reconciled against.
fn live_claimed_ids(list: &DevboxListResponse, owner: &str) -> BTreeSet<String> {
    list.devboxes
        .iter()
        .filter(|d| claimed_by(d, owner))
        .map(|d| d.id.clone())
        .collect()
}

/// The authenticated owner's active claims, derived from a server listing.
fn claims_from_list(list: DevboxListResponse, server: &str, owner: &str) -> Vec<state::Claim> {
    list.devboxes
        .into_iter()
        .filter(|d| claimed_by(d, owner))
        .map(|d| state::Claim {
            id: d.id,
            server_url: server.to_string(),
            claimed_at: d.claimed_at,
        })
        .collect()
}

/// Discover the authenticated owner's active claims from the server, used when
/// the local registry is empty. Returns an empty list (rather than erroring) when
/// the read fails, so the caller surfaces the normal "no active devbox" message.
async fn discover_claims(
    http: &reqwest::Client,
    server: &str,
    session: &session::Session,
) -> Result<Vec<state::Claim>> {
    let url = format!("{server}/api/v1/devboxes");
    let resp = with_auth(http.get(&url), &session.token)
        .send()
        .await
        .context("failed to query devboxes while resolving the active claim")?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let list: DevboxListResponse = resp
        .json()
        .await
        .context("failed to parse devbox list while resolving the active claim")?;
    Ok(claims_from_list(list, server, &session.owner))
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
            let session = require_session(&server)?;
            let url = format!("{server}/api/v1/devboxes/claim");
            let req = ClaimRequest {
                instance_type: instance_type.map(InstanceType),
            };
            let resp = with_auth(http.post(&url).json(&req), &session.token)
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
            let session = require_session(&server)?;
            let id = resolve_id(id, &server, &http, &session).await?;
            let url = format!("{server}/api/v1/devboxes/{id}/release");
            let resp = with_auth(http.post(&url), &session.token)
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
            let session = require_session(&server)?;
            let url = format!("{server}/api/v1/devboxes");
            let resp = with_auth(http.get(&url), &session.token)
                .send()
                .await
                .context("failed to send list request")?;

            if resp.status().is_success() {
                let list: DevboxListResponse =
                    resp.json().await.context("failed to parse response")?;
                reconcile_claims(&list, &server, &session.owner);
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
            let session = require_session(&server)?;
            let id = resolve_id(id, &server, &http, &session).await?;
            let url = format!("{server}/api/v1/devboxes/{id}");
            let resp = with_auth(http.get(&url), &session.token)
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
            let session = require_session(&server)?;
            let id = resolve_id(id, &server, &http, &session).await?;
            let url = format!("{server}/api/v1/devboxes/{id}");
            let resp = with_auth(http.get(&url), &session.token)
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
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
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

    fn claim(id: &str) -> state::Claim {
        state::Claim {
            id: id.to_string(),
            server_url: "http://s".to_string(),
            claimed_at: None,
        }
    }

    #[test]
    fn claims_from_list_keeps_only_owner_claimed_and_carries_fields() {
        let mut mine = devbox("mine", DevboxState::Claimed, Some("jdoe"));
        mine.claimed_at = Some("2026-06-23T01:00:00Z".to_string());
        let list = DevboxListResponse {
            devboxes: vec![
                mine,
                devbox("theirs", DevboxState::Claimed, Some("asmith")),
                devbox("ready", DevboxState::Ready, None),
            ],
        };
        let claims = claims_from_list(list, "http://s1", "jdoe");
        assert_eq!(claims.len(), 1);
        let c = claims.first().unwrap();
        assert_eq!(c.id, "mine");
        assert_eq!(
            c.server_url, "http://s1",
            "server is stamped onto the claim"
        );
        assert_eq!(
            c.claimed_at.as_deref(),
            Some("2026-06-23T01:00:00Z"),
            "claimed_at is carried through for the picker label"
        );
    }

    #[test]
    fn select_claim_empty_errors_with_claim_hint() {
        let err = select_claim(Vec::new(), "http://s").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no active devbox"), "got: {msg}");
        assert!(msg.contains("devbox claim"), "got: {msg}");
    }

    #[test]
    fn select_claim_single_returns_it() {
        let chosen = select_claim(vec![claim("box-a")], "http://s").unwrap();
        assert_eq!(chosen.id, "box-a");
    }

    #[test]
    fn select_claim_multiple_without_tty_lists_ids() {
        // The interactive picker needs a human; only the non-TTY branch (which
        // lists the candidate ids) is deterministic under `cargo test`.
        if is_interactive() {
            return;
        }
        let err = select_claim(vec![claim("box-a"), claim("box-b")], "http://s").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("multiple active devboxes"), "got: {msg}");
        assert!(msg.contains("box-a") && msg.contains("box-b"), "got: {msg}");
    }
}
