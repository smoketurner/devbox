//! Command handler functions.
//!
//! Each public `cmd_*` function implements one CLI subcommand. `main.rs` holds
//! the Clap definitions and a thin dispatch; all business logic lives here.
//! The dispatch/resolve helpers are private to this module.

use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use dialoguer::Select;

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, RenameRequest,
    is_valid_devbox_name,
};

use crate::format;
use crate::session;
use crate::state;

// ============================================================================
// Constants
// ============================================================================

/// Default server used before the first `devbox login` (and when no
/// `--server`/`$DEVBOX_SERVER` is given and none has been remembered).
pub(crate) const DEFAULT_SERVER: &str = "http://localhost:3000";

// ============================================================================
// Dispatch helpers
// ============================================================================

/// Resolve the server to talk to: an explicit `--server`/`$DEVBOX_SERVER`, else
/// the server remembered from the last `devbox login`, else [`DEFAULT_SERVER`].
pub(crate) fn resolve_server(explicit: Option<String>) -> Result<String> {
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
pub(crate) fn with_auth(builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    builder.bearer_auth(token)
}

/// Whether we can safely open an interactive prompt. `dialoguer` renders to and
/// reads keys from the terminal, so every standard stream must be a TTY —
/// otherwise (piped/redirected stdout, scripts, CI) the picker would render
/// nowhere or fail mid-read. In that case we fall back to a listing error.
pub(crate) fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

/// Resolve the target devbox id for `ssh`/`status`/`release`.
///
/// An explicit `target` (a name or an id) is resolved against the server: a
/// known local claim id short-circuits without a round-trip, otherwise the
/// server listing is matched on `name == target || id == target`. With no
/// `target` we consult the local registry of active claims for this server;
/// when it is empty — e.g. the box was claimed from another machine or directly
/// via the API — we fall back to the server, scoped to the authenticated owner,
/// and remember the box we resolve to so the next call is a local read again.
pub(crate) async fn resolve_target(
    target: Option<String>,
    server: &str,
    http: &reqwest::Client,
    session: &session::Session,
) -> Result<String> {
    if let Some(target) = target {
        // Fast path: a known local claim id needs no network round-trip.
        if state::active_claims(server)?.iter().any(|c| c.id == target) {
            return Ok(target);
        }
        return resolve_by_name_or_id(http, server, session, &target).await;
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

/// Resolve `target` to a devbox id by matching it against the server listing on
/// either the friendly name or the id. Names are globally unique among
/// non-terminated boxes, so at most one box matches.
async fn resolve_by_name_or_id(
    http: &reqwest::Client,
    server: &str,
    session: &session::Session,
    target: &str,
) -> Result<String> {
    let url = format!("{server}/api/v1/devboxes");
    let resp = with_auth(http.get(&url), &session.token)
        .send()
        .await
        .context("failed to query devboxes while resolving the target")?;
    if !resp.status().is_success() {
        bail!(
            "could not look up devbox '{target}' on {server} (HTTP {})",
            resp.status()
        );
    }
    let list: DevboxListResponse = resp
        .json()
        .await
        .context("failed to parse devbox list while resolving the target")?;

    let found = list
        .devboxes
        .into_iter()
        .find(|d| d.name == target || d.id == target)
        .with_context(|| format!("no devbox named '{target}' on {server}"))?;

    // Cache it locally if it is the owner's claim, so later calls are local reads.
    if claimed_by(&found, &session.owner) {
        remember_claim(&found, server);
    }
    Ok(found.id)
}

/// Choose one claim: zero is an error, one is used directly, and several open an
/// interactive picker (or, on a non-TTY, an error listing the candidate ids).
pub(crate) fn select_claim(claims: Vec<state::Claim>, server: &str) -> Result<state::Claim> {
    match claims.len() {
        0 => {
            bail!("no active devbox for {server}; run `devbox claim` first or pass a name or id")
        }
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
                "multiple active devboxes ({}); pass a name or id to choose",
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
    let Some(account_id) = crate::auth::fetch_protected_resource(http, server)
        .await
        .ok()
        .and_then(|prm| prm.aws_account_id)
    else {
        return Ok(None);
    };

    crate::aws_profile::select_profile(&account_id, is_interactive())
}

/// The cached session for `server`, or an error directing the user to log in.
///
/// Authentication is mandatory for mutating calls: the server binds `owner` to
/// the authenticated principal, so claim/release always need a valid session.
pub(crate) fn require_session(server: &str) -> Result<session::Session> {
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
pub(crate) fn remember_claim(devbox: &DevboxResponse, server: &str) {
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
pub(crate) fn claimed_by(d: &DevboxResponse, owner: &str) -> bool {
    d.state == DevboxState::Claimed && d.owner.as_deref() == Some(owner)
}

/// The ids the server reports as `Claimed` by `owner` — the set the local
/// registry should be reconciled against.
pub(crate) fn live_claimed_ids(list: &DevboxListResponse, owner: &str) -> BTreeSet<String> {
    list.devboxes
        .iter()
        .filter(|d| claimed_by(d, owner))
        .map(|d| d.id.clone())
        .collect()
}

/// The authenticated owner's active claims, derived from a server listing.
pub(crate) fn claims_from_list(
    list: DevboxListResponse,
    server: &str,
    owner: &str,
) -> Vec<state::Claim> {
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

// ============================================================================
// Command handlers
// ============================================================================

/// Authenticate to the devbox server via device-code OAuth.
pub(crate) async fn cmd_login(http: &reqwest::Client, server: &str) -> Result<()> {
    let s = crate::auth::login(http, server).await?;
    println!("logged in as {} ({}) on {server}", s.owner, s.email);
    Ok(())
}

/// Forget the cached session (keeps the registered OAuth client).
pub(crate) fn cmd_logout(server: &str) -> Result<()> {
    session::logout(server)?;
    println!("logged out of {server}");
    Ok(())
}

/// Claim an available devbox, optionally setting its name.
pub(crate) async fn cmd_claim(
    http: &reqwest::Client,
    server: &str,
    name: Option<String>,
) -> Result<()> {
    let session = require_session(server)?;
    // Normalize blank → None and reject an obviously invalid name before
    // the round-trip; the server validates authoritatively too.
    let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(ref n) = name
        && !is_valid_devbox_name(n)
    {
        bail!(
            "invalid name '{n}': use 1-32 lowercase letters, digits, \
             '_' or '-', not starting with '-'"
        );
    }
    let url = format!("{server}/api/v1/devboxes/claim");
    let req = ClaimRequest { name };
    let resp = with_auth(http.post(&url).json(&req), &session.token)
        .send()
        .await
        .context("failed to send claim request")?;

    if resp.status().is_success() {
        let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
        remember_claim(&devbox, server);
        println!("{}", format::format_claim_success(&devbox));
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }
}

/// Release a claimed devbox.
pub(crate) async fn cmd_release(
    http: &reqwest::Client,
    server: &str,
    target: Option<String>,
) -> Result<()> {
    let session = require_session(server)?;
    let id = resolve_target(target, server, http, &session).await?;
    let url = format!("{server}/api/v1/devboxes/{id}/release");
    let resp = with_auth(http.post(&url), &session.token)
        .send()
        .await
        .context("failed to send release request")?;

    if resp.status().is_success() {
        let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
        forget_claim(&id, server);
        println!("{}", format::format_release_success(&devbox));
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }
}

/// Rename the devbox identified by `target` (name or id) to `new_name`.
///
/// Validates `new_name` locally before the round-trip; on success prints the
/// renamed box.
pub(crate) async fn cmd_rename(
    http: &reqwest::Client,
    server: &str,
    target: String,
    new_name: String,
) -> Result<()> {
    let session = require_session(server)?;
    // Normalize and validate locally; server validates authoritatively too.
    let new_name = new_name.trim().to_string();
    if !is_valid_devbox_name(&new_name) {
        bail!(
            "invalid name '{new_name}': use 1-32 lowercase letters, digits, \
             '_' or '-', not starting with '-'"
        );
    }
    let id = resolve_target(Some(target), server, http, &session).await?;
    let url = format!("{server}/api/v1/devboxes/{id}/rename");
    let req = RenameRequest { name: new_name };
    let resp = with_auth(http.post(&url).json(&req), &session.token)
        .send()
        .await
        .context("failed to send rename request")?;

    if resp.status().is_success() {
        let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
        println!("{}", format::format_rename_success(&devbox));
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }
}

/// List all devboxes.
pub(crate) async fn cmd_list(http: &reqwest::Client, server: &str) -> Result<()> {
    let session = require_session(server)?;
    let url = format!("{server}/api/v1/devboxes");
    let resp = with_auth(http.get(&url), &session.token)
        .send()
        .await
        .context("failed to send list request")?;

    if resp.status().is_success() {
        let list: DevboxListResponse = resp.json().await.context("failed to parse response")?;
        reconcile_claims(&list, server, &session.owner);
        if list.devboxes.is_empty() {
            println!("No devboxes found.");
        } else {
            println!("{}", format::format_list_table(&list));
        }
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }
}

/// Get status of a specific devbox.
pub(crate) async fn cmd_status(
    http: &reqwest::Client,
    server: &str,
    target: Option<String>,
) -> Result<()> {
    let session = require_session(server)?;
    let id = resolve_target(target, server, http, &session).await?;
    let url = format!("{server}/api/v1/devboxes/{id}");
    let resp = with_auth(http.get(&url), &session.token)
        .send()
        .await
        .context("failed to send status request")?;

    if resp.status().is_success() {
        let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
        println!("{}", format::format_status(&devbox));
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }
}

/// SSH into a claimed devbox over an SSM tunnel.
pub(crate) async fn cmd_ssh(
    http: &reqwest::Client,
    server: &str,
    target: Option<String>,
    profile: Option<String>,
    print: bool,
    args: Vec<String>,
) -> Result<()> {
    let session = require_session(server)?;
    let id = resolve_target(target, server, http, &session).await?;
    let url = format!("{server}/api/v1/devboxes/{id}");
    let resp = with_auth(http.get(&url), &session.token)
        .send()
        .await
        .context("failed to look up devbox")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} {}", status.as_u16(), body)
    }

    let devbox: DevboxResponse = resp.json().await.context("failed to parse response")?;
    // With no explicit --profile, auto-select the AWS profile that
    // matches the control plane's account so the SSM tunnel "just works".
    let profile = match profile {
        Some(profile) => Some(profile),
        None => resolve_aws_profile(http, server).await?,
    };
    let opts = crate::ssh::SshOptions {
        profile,
        print,
        extra: args,
    };
    crate::ssh::connect(&devbox, &opts)
}

/// Native SSM data-channel proxy used as an ssh `ProxyCommand`.
pub(crate) async fn cmd_ssm_proxy(
    target: &str,
    region: &str,
    port: u16,
    profile: Option<&str>,
) -> Result<()> {
    crate::ssm::run_proxy(target, region, port, profile).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use devbox_common::{AmiId, InstanceType};

    use super::*;

    fn devbox(id: &str, state: DevboxState, owner: Option<&str>) -> DevboxResponse {
        DevboxResponse {
            id: id.to_string(),
            instance_id: "i-1234567890abcdef0".to_string(),
            name: id.to_string(),
            state,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            owner: owner.map(str::to_string),
            region: "us-east-1".to_string(),
            created_at: "2026-06-23T00:00:00Z".to_string(),
            claimed_at: None,
        }
    }

    fn claim(id: &str) -> state::Claim {
        state::Claim {
            id: id.to_string(),
            server_url: "http://s".to_string(),
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
