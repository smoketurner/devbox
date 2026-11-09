//! Policy-driven CONNECT forward proxy.
//!
//! A devbox sets `HTTPS_PROXY`/`HTTP_PROXY` at this listener; for each
//! `CONNECT host:port` the proxy asks the dogwood-language-backed [`EgressPolicy`]
//! (default-deny) whether the host is permitted, resolves it, refuses any
//! non-public IP (SSRF / IMDS / RFC1918 protection), then relays the raw bytes. It
//! runs as a second listener in the server binary, independent of the API router.
//!
//! # Allowed ports
//!
//! The proxy relays a CONNECT only to the host on port 80 or 443 — exactly that
//! set (see [`ALLOWED_PORTS`]). The set is intentionally fixed to `{80, 443}` and
//! is not operator-configurable; a CONNECT to any other port is refused with
//! `403` before any policy call or DNS resolution. Port 443 is HTTPS; port 80 is
//! for plain-HTTP package mirrors (`apt`/`yum` and friends) whose repositories are
//! served over HTTP. The SSRF / IMDS / RFC1918 guard
//! ([`crate::net_guard::resolve_public`] + [`crate::net_guard::is_forbidden_ip`],
//! connecting to the exact resolved address so there is no DNS-rebind window) and
//! the default-deny host policy apply identically on both
//! ports — widening the port gate does NOT widen any other guard.
//!
//! The proxy tunnels bytes opaquely — on 443 it never sees inside TLS and on 80 it
//! does not parse the forwarded HTTP, so it cannot inject credentials. That is the
//! reverse proxy's job ([`crate::github::git_proxy`]); this is the policy-driven
//! host gate for everything else.
//!
//! # Subject identity
//!
//! The CONNECT path does NOT authenticate the box per-connection (the tunnel is
//! opaque TLS and there is no per-box credential on this hop today), so every
//! decision is made against the coarse constant subject [`CONNECT_SUBJECT`]
//! (`"pool"`). This is a known limitation: real per-box identity
//! ([`crate::auth::agent_identity`]) is available on the authenticated reverse
//! proxy, not here. The policy is therefore a fleet-wide host gate on this path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use devbox_common::env_non_empty;

use crate::policy::{EgressDecision, EgressPolicy};

const PROXY_ADDR_ENV: &str = "EGRESS_PROXY_ADDR";
const ALLOWLIST_ENV: &str = "EGRESS_ALLOWLIST";
/// Optional path to a Dogwood egress policy source file (`.dw`; see
/// [`crate::policy`]). When set, it replaces the allowlist-derived default
/// policy; `EGRESS_ALLOWLIST` is still read and its hosts are used for subdomain
/// folding before the policy call.
const POLICY_PATH_ENV: &str = "EGRESS_POLICY_PATH";
/// Optional path to append JSONL decision records to, in addition to the default
/// `tracing` sink.
const DECISION_LOG_ENV: &str = "EGRESS_DECISION_LOG";
/// Optional selector for the built-in DEFAULT-ON egress bundle catalog (see
/// [`crate::egress_bundles`]). Unset => all default bundles on; a comma-separated
/// list => only those bundles; the token `none` (or empty-after-trim) => all off.
const DEFAULT_BUNDLES_ENV: &str = "EGRESS_DEFAULT_BUNDLES";

/// The coarse subject used for every CONNECT decision (see the module docs).
const CONNECT_SUBJECT: &str = "pool";

/// The operation recorded in a CONNECT decision record.
const CONNECT_OPERATION: &str = "connect";

/// The ports a CONNECT may target, checked by [`is_allowed_port`]. The set is
/// intentionally fixed and explicit (never "any port"): `443` is HTTPS and `80`
/// is for plain-HTTP package mirrors (`apt`/`yum`). A CONNECT to any other port
/// is refused with `403` before any policy call or DNS resolution. This is NOT
/// operator-configurable by design — widening it would be a security-relevant
/// change, so it lives in source where it is reviewed.
const ALLOWED_PORTS: &[u16] = &[80, 443];

/// Whether a CONNECT to `port` is allowed by the explicit [`ALLOWED_PORTS`] set.
/// Anything outside the set is refused before the policy call and DNS resolution,
/// so the SSRF/IMDS/RFC1918 guard and host policy only ever run for `{80, 443}`.
fn is_allowed_port(port: u16) -> bool {
    ALLOWED_PORTS.contains(&port)
}

/// Cap on the CONNECT request head read before giving up.
const MAX_HEAD: usize = 8 * 1024;

/// Read chunk for the CONNECT head.
const READ_CHUNK: usize = 1024;

/// Bound on reading the CONNECT head, so a slow client can't hold a connection open.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the upstream TCP connect, so an unreachable/black-holed host fails fast
/// instead of hanging the client indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A structured egress authorization decision record, emitted for every allow and
/// every deny so a fleet's egress is auditable (mirrors Box's `records.jsonl`).
///
/// Serialized as one JSON object per line (JSONL). The exact shape is:
///
/// ```json
/// {"subject":"pool","operation":"connect","host":"crates.io","decision":"allow","reason":"allow_connect_0","timestamp":"2026-11-08T03:42:57.123456789Z"}
/// ```
///
/// Fields:
/// - `subject`: the coarse box subject (see [`CONNECT_SUBJECT`]).
/// - `operation`: always `"connect"` on this path.
/// - `host`: the requested CONNECT host (as sent by the client).
/// - `decision`: `"allow"` or `"deny"`.
/// - `reason`: the policy attribution / reason string from the engine.
/// - `timestamp`: RFC3339 (UTC) wall-clock time the decision was made, via jiff.
#[derive(Debug, serde::Serialize)]
pub struct DecisionRecord {
    /// The subject the decision was made for.
    pub subject: String,
    /// The egress operation (`"connect"` on the CONNECT path).
    pub operation: String,
    /// The requested host.
    pub host: String,
    /// `"allow"` or `"deny"`.
    pub decision: String,
    /// The policy attribution / reason for the decision.
    pub reason: String,
    /// RFC3339 timestamp the decision was made.
    pub timestamp: String,
}

impl DecisionRecord {
    /// Build a CONNECT decision record from a policy decision for `host`.
    fn connect(subject: &str, host: &str, decision: &EgressDecision) -> Self {
        Self {
            subject: subject.to_string(),
            operation: CONNECT_OPERATION.to_string(),
            host: host.to_string(),
            decision: if decision.allowed { "allow" } else { "deny" }.to_string(),
            reason: decision.reason.clone(),
            timestamp: jiff::Timestamp::now().to_string(),
        }
    }

    /// Build an HTTP reverse-proxy decision record from a policy decision for a
    /// `method`/`path` request to `host`. Shared with [`crate::egress_proxy`] so
    /// the CONNECT gate and the credential-injecting reverse proxy emit the same
    /// JSONL record shape; the `operation` is `"http_request"` and the `host` is
    /// recorded with the request method and path appended as `METHOD path` so an
    /// auditor can distinguish HTTP decisions from opaque CONNECT ones.
    pub(crate) fn http(
        subject: &str,
        host: &str,
        method: &str,
        path: &str,
        decision: &EgressDecision,
    ) -> Self {
        Self {
            subject: subject.to_string(),
            operation: "http_request".to_string(),
            host: format!("{host} {method} {path}"),
            decision: if decision.allowed { "allow" } else { "deny" }.to_string(),
            reason: decision.reason.clone(),
            timestamp: jiff::Timestamp::now().to_string(),
        }
    }
}

/// Where decision records are emitted. The default `tracing` event is always
/// produced; an optional append-only file sink mirrors the records to disk.
///
/// Both sinks are best-effort and non-blocking on the proxy hot path: a file
/// write error is logged and swallowed, never propagated to the tunnel.
#[derive(Clone, Default)]
pub struct DecisionSink {
    /// An optional append file; shared across tasks behind a mutex (held only for
    /// the synchronous write, never across an `.await`).
    file: Option<Arc<Mutex<std::fs::File>>>,
}

impl DecisionSink {
    /// Build a sink from the environment: a `tracing`-only sink by default, plus
    /// an append file when `EGRESS_DECISION_LOG` is set.
    ///
    /// # Errors
    ///
    /// Returns an error when `EGRESS_DECISION_LOG` is set but the file cannot be
    /// opened for appending — a misconfigured audit sink is a hard startup error
    /// rather than silently losing records.
    pub(crate) fn from_env() -> Result<Self> {
        let Some(path) = env_non_empty(DECISION_LOG_ENV) else {
            return Ok(Self::default());
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {DECISION_LOG_ENV} at {path}"))?;
        Ok(Self {
            file: Some(Arc::new(Mutex::new(file))),
        })
    }

    /// Emit a decision record: always a structured `tracing` event, plus a JSONL
    /// line to the file sink when configured. Best-effort: serialization or write
    /// failures are logged and swallowed so the hot path is never blocked.
    pub(crate) fn emit(&self, record: &DecisionRecord) {
        tracing::info!(
            subject = %record.subject,
            operation = %record.operation,
            host = %record.host,
            decision = %record.decision,
            reason = %record.reason,
            timestamp = %record.timestamp,
            "egress decision"
        );
        let Some(file) = &self.file else {
            return;
        };
        let mut line = match serde_json::to_string(record) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "egress decision record serialize failed");
                return;
            }
        };
        line.push('\n');
        // Hold the mutex only for the synchronous write; no await inside.
        let Ok(mut guard) = file.lock() else {
            tracing::error!("egress decision log mutex poisoned; record dropped");
            return;
        };
        if let Err(e) = std::io::Write::write_all(&mut *guard, line.as_bytes()) {
            tracing::warn!(error = %e, "egress decision record write failed");
        }
    }
}

/// The shared dogwood-language-backed egress policy plus decision sink.
///
/// This is intentionally independent of the CONNECT listener address: the same
/// `EgressPolicy` (one rule set) and the same
/// [`DecisionSink`] back BOTH the CONNECT proxy ([`serve`]) and the
/// credential-injecting reverse proxy ([`crate::egress_proxy`] via
/// [`crate::policy::EgressPolicy::decide_http`]). Building it here — rather than
/// only on the CONNECT path — means the reverse proxy always has a usable policy
/// whenever egress is configured at all, so a deployment that sets
/// `EGRESS_UPSTREAMS` without `EGRESS_PROXY_ADDR` is not silently deny-all.
#[derive(Clone)]
pub struct EgressShared {
    pub policy: Arc<EgressPolicy>,
    pub sink: DecisionSink,
}

impl EgressShared {
    /// Build the shared policy + sink from the environment.
    ///
    /// `EGRESS_ALLOWLIST` drives the default-deny policy (each listed host, and
    /// its subdomains, permits `Connect`). A richer policy can be supplied
    /// via `EGRESS_POLICY_PATH` (a Dogwood `.dw` source; see [`crate::policy`]).
    /// Records are emitted via `tracing` and, when `EGRESS_DECISION_LOG` is set,
    /// appended to that file.
    ///
    /// `integration_rules` are the per-integration method/path `permit` rules
    /// compiled from `EGRESS_UPSTREAMS` (see [`crate::egress_proxy`]); they are
    /// appended to the policy so the reverse proxy's `decide_http` and the CONNECT
    /// gate share one rule set (a single authorization path). Pass an empty vec
    /// when the integrations layer is not configured.
    ///
    /// # Errors
    ///
    /// Returns an error when `EGRESS_POLICY_PATH` is set but unreadable, when the
    /// policy source fails to parse (egress must not boot fail-open), or when
    /// `EGRESS_DECISION_LOG` cannot be opened.
    pub fn from_env(integration_rules: Vec<crate::policy::PolicyRule>) -> Result<Self> {
        let allowlist_raw = env_non_empty(ALLOWLIST_ENV).unwrap_or_default();

        // Resolve the built-in DEFAULT-ON bundle catalog selection from
        // EGRESS_DEFAULT_BUNDLES (a pure parse of the raw value), log any unknown
        // names, and compute the package-repo bundle Connect hosts to merge into
        // the SAME shared policy alongside the operator allowlist.
        let (selection, unknown) = crate::egress_bundles::BundleSelection::parse(
            env_non_empty(DEFAULT_BUNDLES_ENV).as_deref(),
        );
        for name in &unknown {
            tracing::warn!(
                bundle = %name,
                "unknown egress bundle in EGRESS_DEFAULT_BUNDLES; ignored"
            );
        }
        let bundle_hosts = crate::egress_bundles::connect_hosts(&selection);
        log_active_bundles(&selection, &bundle_hosts);

        let mut policy = if let Some(policy_path) = env_non_empty(POLICY_PATH_ENV) {
            let source = std::fs::read_to_string(&policy_path)
                .with_context(|| format!("read {POLICY_PATH_ENV} at {policy_path}"))?;
            // Build the custom-policy-path policy with the active bundle hosts
            // merged in. Factored into a pure helper (no process env) so the
            // bundle merge on this branch is covered deterministically by a unit
            // test that drives the real merge code.
            build_custom_policy(&source, &allowlist_raw, &bundle_hosts)
                .context("build egress policy from EGRESS_POLICY_PATH")?
        } else {
            if allowlist_raw.is_empty() && integration_rules.is_empty() && bundle_hosts.is_empty() {
                // Only deny-all now: bundles OFF (=none or all-unknown) AND no
                // operator config of any kind.
                tracing::warn!(
                    "egress enabled with no bundles, an empty allowlist, and no integrations; \
                     all egress is denied (set EGRESS_DEFAULT_BUNDLES, EGRESS_ALLOWLIST, \
                     EGRESS_POLICY_PATH, or EGRESS_UPSTREAMS)"
                );
            }
            // Seed the shared policy with the operator allowlist AND the active
            // bundle Connect hosts together, so both get permit-Connect rules and
            // identical dot-boundary subdomain folding via the one allowlist path.
            let mut hosts = parse_allow_hosts(&allowlist_raw);
            for host in &bundle_hosts {
                if !hosts.contains(host) {
                    hosts.push(host.clone());
                }
            }
            EgressPolicy::from_allowlist_csv(&hosts.join(","))
                .context("build egress policy from EGRESS_ALLOWLIST + default bundles")?
        };

        // Compile the inline EGRESS_UPSTREAMS method/path scoping into the SAME
        // policy, so there is exactly one authorization path for both gates. A
        // default-on API template becomes live ONLY here, when the operator names
        // it in EGRESS_UPSTREAMS with a credential (see crate::egress_bundles);
        // an unconfigured template contributes no rule and stays fail-closed.
        policy.extend_rules(integration_rules);

        Ok(Self {
            policy: Arc::new(policy),
            sink: DecisionSink::from_env()?,
        })
    }
}

/// Configuration for the CONNECT egress proxy listener, or `None` when
/// `EGRESS_PROXY_ADDR` is unset (the CONNECT listener is disabled). The policy and
/// sink are the shared ones from [`EgressShared`], so the CONNECT listener and the
/// reverse proxy share one policy and one audit sink.
pub struct EgressConfig {
    pub addr: SocketAddr,
    pub policy: Arc<EgressPolicy>,
    pub sink: DecisionSink,
}

impl EgressConfig {
    /// Read the CONNECT listener config from the environment, reusing the already
    /// built shared policy + sink. `Ok(None)` disables the CONNECT listener (but
    /// the shared policy may still back the reverse proxy).
    ///
    /// Backward compatible: `EGRESS_PROXY_ADDR` enables the listener; the policy
    /// semantics (including `EGRESS_ALLOWLIST`) live in [`EgressShared::from_env`].
    ///
    /// # Errors
    ///
    /// Returns an error when `EGRESS_PROXY_ADDR` is set but is not a valid socket
    /// address.
    pub fn from_env(shared: &EgressShared) -> Result<Option<Self>> {
        let Some(addr_raw) = env_non_empty(PROXY_ADDR_ENV) else {
            return Ok(None);
        };
        let addr: SocketAddr = addr_raw
            .parse()
            .with_context(|| format!("invalid {PROXY_ADDR_ENV}: {addr_raw}"))?;

        Ok(Some(Self {
            addr,
            policy: Arc::clone(&shared.policy),
            sink: shared.sink.clone(),
        }))
    }
}

/// Emit startup logging describing the active built-in egress bundle catalog:
/// the catalog version, the active package-repo bundles and their per-bundle
/// host counts, the total Connect host count contributed, and the API templates
/// the selection makes eligible (still inert until the operator supplies a
/// credential via `EGRESS_UPSTREAMS`). A clear line is logged when bundles are
/// disabled (`EGRESS_DEFAULT_BUNDLES=none`).
fn log_active_bundles(selection: &crate::egress_bundles::BundleSelection, bundle_hosts: &[String]) {
    use crate::egress_bundles::{
        BundleSelection, CATALOG_VERSION, active_api_templates, active_package_bundles,
    };
    if matches!(selection, BundleSelection::None) {
        tracing::info!(
            catalog_version = CATALOG_VERSION,
            "egress default bundles disabled (EGRESS_DEFAULT_BUNDLES=none); no built-in hosts \
             permitted"
        );
        return;
    }
    let packages = active_package_bundles(selection);
    for (name, host_count) in &packages {
        tracing::info!(
            catalog_version = CATALOG_VERSION,
            bundle = %name,
            hosts = host_count,
            "egress default bundle active"
        );
    }
    let templates = active_api_templates(selection);
    tracing::info!(
        catalog_version = CATALOG_VERSION,
        package_bundles = packages.len(),
        connect_hosts = bundle_hosts.len(),
        api_templates = ?templates,
        "egress default bundle catalog loaded (API templates inert until a credential is \
         supplied via EGRESS_UPSTREAMS)"
    );
}

/// Parse a comma-separated allowlist into normalized host entries (lowercase,
/// trimmed, dots stripped, empties dropped). Used only for subdomain folding when
/// a custom `EGRESS_POLICY_PATH` is supplied.
fn parse_allow_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|e| e.trim().trim_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Build the shared [`EgressPolicy`] for the custom `EGRESS_POLICY_PATH` branch,
/// merging the active default-bundle Connect hosts into the operator-supplied
/// Dogwood source.
///
/// This is the pure core of the `EGRESS_POLICY_PATH` arm of
/// [`EgressShared::from_env`], split out so the bundle merge on this branch is
/// exercised deterministically by a unit test without mutating process env (the
/// caller resolves `source`, `allowlist_raw`, and `bundle_hosts` from the
/// environment and passes them in). The behavior is identical to the inlined
/// version it replaced:
///
/// 1. Hosts from `EGRESS_ALLOWLIST` (`allowlist_raw`) and the active bundles seed
///    subdomain folding, so a custom policy keeps dot-boundary semantics for
///    those hosts.
/// 2. A custom policy bypasses the allowlist `permit`-`Connect` seed, so each
///    bundle host is merged as a generated `permit`-`Connect` rule
///    (`bundle_connect_<i>`) via [`EgressPolicy::extend_rules`], keeping one
///    authorization path.
/// 3. A custom `forbid` in the source still overrides these bundle permits
///    (forbid-overrides-permit holds, order-independently).
///
/// # Errors
///
/// Returns an error when the Dogwood source fails to parse (egress must not boot
/// fail-open).
fn build_custom_policy(
    source: &str,
    allowlist_raw: &str,
    bundle_hosts: &[String],
) -> Result<EgressPolicy> {
    let mut allow_hosts = parse_allow_hosts(allowlist_raw);
    for host in bundle_hosts {
        if !allow_hosts.contains(host) {
            allow_hosts.push(host.clone());
        }
    }
    let mut policy = EgressPolicy::from_policy_source(source, allow_hosts)?;
    policy.extend_rules(bundle_hosts.iter().enumerate().map(|(i, host)| {
        crate::policy::PolicyRule::permit_connect(host, format!("bundle_connect_{i}"))
    }));
    Ok(policy)
}

/// Accept CONNECT tunnels until `cancel` fires. Each connection is handled on its
/// own task; a failed connection is logged and dropped.
pub async fn serve(
    listener: TcpListener,
    policy: Arc<EgressPolicy>,
    sink: DecisionSink,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("egress proxy shutting down");
                return;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let policy = Arc::clone(&policy);
                    let sink = sink.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, &policy, &sink).await {
                            tracing::debug!(%peer, error = %format!("{e:#}"), "egress connection failed");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "egress accept failed"),
            },
        }
    }
}

/// Handle one client connection: parse CONNECT, authorize via policy, then tunnel.
async fn handle_connection(
    mut client: TcpStream,
    policy: &EgressPolicy,
    sink: &DecisionSink,
) -> Result<()> {
    let (host, port, leftover) =
        match tokio::time::timeout(HEAD_TIMEOUT, read_connect_head(&mut client)).await {
            Ok(Ok(Some(head))) => head,
            Ok(Ok(None)) => return refuse(&mut client, "400 Bad Request").await,
            Ok(Err(e)) => return Err(e),
            Err(_) => return refuse(&mut client, "408 Request Timeout").await,
        };

    // Port refusal happens before any policy call or DNS resolution (preserved
    // exactly): only the explicit {80, 443} set is admitted; anything else is
    // refused here so the SSRF guard and host policy run only for allowed ports.
    if !is_allowed_port(port) {
        tracing::info!(host, port, "egress denied (port not allowed)");
        return refuse(&mut client, "403 Forbidden").await;
    }

    // decide_connect is synchronous and lock-free, so nothing is held across the
    // connect/tunnel awaits below.
    let decision = policy.decide_connect(CONNECT_SUBJECT, &host);
    sink.emit(&DecisionRecord::connect(CONNECT_SUBJECT, &host, &decision));
    if !decision.allowed {
        tracing::info!(host, reason = %decision.reason, "egress denied (policy)");
        return refuse(&mut client, "403 Forbidden").await;
    }

    let Some(target) = crate::net_guard::resolve_public(&host, port).await? else {
        tracing::warn!(
            host,
            "egress denied (resolves only to non-public addresses)"
        );
        return refuse(&mut client, "403 Forbidden").await;
    };

    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            tracing::debug!(host, %target, error = %e, "egress upstream connect failed");
            return refuse(&mut client, "502 Bad Gateway").await;
        }
        Err(_) => {
            tracing::warn!(host, %target, "egress upstream connect timed out");
            return refuse(&mut client, "504 Gateway Timeout").await;
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if !leftover.is_empty() {
        upstream.write_all(&leftover).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Write a minimal HTTP error response and end the connection. A write failure
/// here just means the client already went away, surfaced to the caller's debug log.
async fn refuse(client: &mut TcpStream, status: &str) -> Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
    client.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Read the CONNECT request head, returning `(host, port, leftover)` where
/// `leftover` is any bytes the client pipelined after the head (forwarded upstream).
/// `Ok(None)` for a malformed or oversized head, or EOF before it completes.
async fn read_connect_head(client: &mut TcpStream) -> Result<Option<(String, u16, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        match chunk.get(..n) {
            Some(read) => buf.extend_from_slice(read),
            None => return Ok(None),
        }
        if let Some(end) = find_head_end(&buf) {
            let head = buf.get(..end).unwrap_or_default();
            let Some((host, port)) = parse_connect_target(head) else {
                return Ok(None);
            };
            let leftover = buf
                .get(end.saturating_add(4)..)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            return Ok(Some((host, port, leftover)));
        }
    }
}

/// The index where the `\r\n\r\n` head terminator begins, if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the `host` and `port` from a `CONNECT host:port HTTP/1.1` request head.
/// `None` for a non-CONNECT method or a malformed target.
fn parse_connect_target(head: &[u8]) -> Option<(String, u16)> {
    let line = std::str::from_utf8(head).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let (host, port) = parts.next()?.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn policy_from_csv(csv: &str) -> EgressPolicy {
        EgressPolicy::from_allowlist_csv(csv).unwrap()
    }

    #[test]
    fn policy_gate_allows_permitted_host() {
        // A permitted host (and its subdomains, dot-boundary) passes the gate,
        // reproducing the old Allowlist semantics now that the policy is the
        // authorization path on the CONNECT hop.
        let policy = policy_from_csv("crates.io, github.com");
        assert!(policy.decide_connect(CONNECT_SUBJECT, "crates.io").allowed);
        assert!(
            policy
                .decide_connect(CONNECT_SUBJECT, "static.crates.io")
                .allowed
        );
        assert!(
            policy
                .decide_connect(CONNECT_SUBJECT, "codeload.github.com")
                .allowed
        );
    }

    #[test]
    fn policy_gate_denies_non_members_and_lookalikes() {
        let policy = policy_from_csv("crates.io");
        assert!(!policy.decide_connect(CONNECT_SUBJECT, "evil.com").allowed);
        // A suffix match must be on a dot boundary, not a substring.
        assert!(
            !policy
                .decide_connect(CONNECT_SUBJECT, "notcrates.io")
                .allowed
        );
        assert!(
            !policy
                .decide_connect(CONNECT_SUBJECT, "crates.io.evil.com")
                .allowed
        );
        // An empty allowlist denies all (backward compatible with the warning).
        let empty = policy_from_csv("");
        assert!(!empty.decide_connect(CONNECT_SUBJECT, "crates.io").allowed);
    }

    #[test]
    fn decision_record_captures_documented_fields() {
        let policy = policy_from_csv("crates.io");
        // Allow record.
        let allow = policy.decide_connect(CONNECT_SUBJECT, "crates.io");
        let allow_rec = DecisionRecord::connect(CONNECT_SUBJECT, "crates.io", &allow);
        assert_eq!(allow_rec.subject, CONNECT_SUBJECT);
        assert_eq!(allow_rec.operation, "connect");
        assert_eq!(allow_rec.host, "crates.io");
        assert_eq!(allow_rec.decision, "allow");
        assert!(!allow_rec.reason.is_empty());
        assert!(!allow_rec.timestamp.is_empty());
        // The record serializes to a single JSON line with every field.
        let json: serde_json::Value = serde_json::to_value(&allow_rec).unwrap();
        for key in [
            "subject",
            "operation",
            "host",
            "decision",
            "reason",
            "timestamp",
        ] {
            assert!(json.get(key).is_some(), "record missing {key}");
        }
        // Deny record.
        let deny = policy.decide_connect(CONNECT_SUBJECT, "evil.com");
        let deny_rec = DecisionRecord::connect(CONNECT_SUBJECT, "evil.com", &deny);
        assert_eq!(deny_rec.decision, "deny");
    }

    #[test]
    fn http_decision_record_captures_method_and_path_for_allow_and_deny() {
        // The shared HTTP record (used by the reverse proxy in egress_proxy)
        // carries operation "http_request" and records the method/path alongside
        // the host, for both an allow and a deny decision.
        let policy = policy_from_csv("api.example.com");
        // The default allowlist policy permits Connect only, so an HttpRequest is
        // default-denied here — exercise the deny shape.
        let deny = policy.decide_http("i-abc", "api.example.com", "POST", "/v1/data");
        let deny_rec = DecisionRecord::http("i-abc", "api.example.com", "POST", "/v1/data", &deny);
        assert_eq!(deny_rec.subject, "i-abc");
        assert_eq!(deny_rec.operation, "http_request");
        assert_eq!(deny_rec.host, "api.example.com POST /v1/data");
        assert_eq!(deny_rec.decision, "deny");
        assert!(!deny_rec.reason.is_empty());

        // An allow shape, synthesized from an allow decision, serializes with all
        // six documented keys.
        let allow = EgressDecision {
            allowed: true,
            reason: "permit_http".to_string(),
        };
        let allow_rec = DecisionRecord::http("i-abc", "api.example.com", "GET", "/v1/ping", &allow);
        assert_eq!(allow_rec.decision, "allow");
        let json: serde_json::Value = serde_json::to_value(&allow_rec).unwrap();
        for key in [
            "subject",
            "operation",
            "host",
            "decision",
            "reason",
            "timestamp",
        ] {
            assert!(json.get(key).is_some(), "http record missing {key}");
        }
    }

    #[test]
    fn parses_connect_target() {
        assert_eq!(
            parse_connect_target(b"CONNECT github.com:443 HTTP/1.1"),
            Some(("github.com".to_string(), 443))
        );
        // Method is case-insensitive; IPv6 authority brackets are stripped.
        assert_eq!(
            parse_connect_target(b"connect [2606:4700::1111]:443 HTTP/1.1"),
            Some(("2606:4700::1111".to_string(), 443))
        );
    }

    #[test]
    fn rejects_non_connect_and_malformed_targets() {
        assert_eq!(parse_connect_target(b"GET / HTTP/1.1"), None);
        assert_eq!(parse_connect_target(b"CONNECT github.com HTTP/1.1"), None);
        assert_eq!(
            parse_connect_target(b"CONNECT github.com:https HTTP/1.1"),
            None
        );
        assert_eq!(parse_connect_target(b""), None);
    }

    #[test]
    fn finds_head_terminator_and_ignores_partial() {
        assert_eq!(
            find_head_end(b"CONNECT x:443 HTTP/1.1\r\n\r\nDATA"),
            Some(22)
        );
        assert_eq!(find_head_end(b"CONNECT x:443 HTTP/1.1\r\n"), None);
    }

    #[test]
    fn allowed_ports_are_exactly_80_and_443() {
        // The explicit port gate admits HTTPS (443) and plain-HTTP package
        // mirrors (80), and nothing else. This runs before any policy call or DNS
        // resolution in handle_connection.
        assert!(is_allowed_port(80));
        assert!(is_allowed_port(443));
        // Representative disallowed ports: SSH, a common alt-HTTP port, SMTP.
        assert!(!is_allowed_port(22));
        assert!(!is_allowed_port(8080));
        assert!(!is_allowed_port(25));
        assert!(!is_allowed_port(0));
        // The set is exactly {80, 443}.
        assert_eq!(ALLOWED_PORTS, &[80, 443]);
    }

    #[test]
    fn policy_gate_is_host_only_on_the_connect_path() {
        // decide_connect is host-only (port-independent), so an unallowlisted
        // host is denied on the CONNECT path regardless of which allowed port
        // (80 or 443) the client targeted — the default-deny host policy gates
        // port-80 CONNECTs identically to 443.
        let policy = policy_from_csv("crates.io");
        assert!(policy.decide_connect(CONNECT_SUBJECT, "crates.io").allowed);
        assert!(
            !policy
                .decide_connect(CONNECT_SUBJECT, "deb.debian.org")
                .allowed
        );
        assert!(!policy.decide_connect(CONNECT_SUBJECT, "evil.com").allowed);
    }

    #[test]
    fn custom_policy_path_merges_bundles_and_forbid_overrides_permit() {
        // Drive the REAL custom-EGRESS_POLICY_PATH merge path
        // (`build_custom_policy`, the pure core of `EgressShared::from_env`'s
        // `EGRESS_POLICY_PATH` arm) with default bundles ON, proving the
        // `permit_connect` + `extend_rules` wiring end-to-end without mutating
        // process env. Previously this branch was only covered by a hand-built
        // Dogwood source in `egress_bundles::tests::forbid_overrides_a_bundled_host`.
        use crate::egress_bundles::{BundleSelection, connect_hosts};

        // A minimal operator custom source that only forbids a bundled host
        // (crates.io). It intentionally permits nothing itself, so any host that
        // is reachable comes from the merged bundle `permit`-`Connect` rules.
        let source = "@id(\"forbid_crates\")\n\
             forbid (principal, action == Egress::Action::\"Connect\", resource)\n\
             when { context.input.host == \"crates.io\" };\n";

        // Default bundles on; no operator EGRESS_ALLOWLIST hosts.
        let bundle_hosts = connect_hosts(&BundleSelection::All);
        // Sanity: the bundled hosts we assert on are actually in the selection.
        assert!(bundle_hosts.iter().any(|h| h == "pypi.org"));
        assert!(bundle_hosts.iter().any(|h| h == "crates.io"));

        let policy = build_custom_policy(source, "", &bundle_hosts).unwrap();

        // (a) A bundled Connect host with no operator forbid is permitted through
        // the merged policy (the generated bundle `permit`-`Connect` rule).
        assert!(
            policy.decide_connect(CONNECT_SUBJECT, "pypi.org").allowed,
            "bundled host must be permitted via the merged custom policy"
        );
        // Dot-boundary folding still applies to a bundled host on this path.
        assert!(
            policy
                .decide_connect(CONNECT_SUBJECT, "files.pythonhosted.org")
                .allowed
        );

        // (b) The operator `forbid` for a bundled host beats the bundle permit
        // (forbid-overrides-permit), even though the bundle would otherwise
        // permit crates.io.
        assert!(
            !policy.decide_connect(CONNECT_SUBJECT, "crates.io").allowed,
            "operator forbid must override the bundle permit on the custom path"
        );

        // A host outside the bundle set and the custom source is still denied.
        assert!(!policy.decide_connect(CONNECT_SUBJECT, "evil.com").allowed);
    }
}
