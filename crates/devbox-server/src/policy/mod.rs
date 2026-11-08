//! Egress policy decision point backed by the Dogwood policy language.
//!
//! This module adopts the published Dogwood crates
//! ([`dogwood_language`] + [`dogwood_local_engine`], both Apache-2.0, from the
//! `dogwood-policy` org) rather than hand-rolling a permit/forbid evaluator. It
//! wraps a Dogwood engine behind a small, devbox-specific API — [`EgressPolicy`]
//! — that FEAT-003 (the CONNECT host gate) and FEAT-004 (the method/path-aware
//! reverse proxy) consume as a stable internal interface.
//!
//! # Model
//!
//! Dogwood is Cedar-derived: a [`SCHEMA`] declares a namespace with `entity`
//! items (a `Box` principal and a `Gateway` resource) and `action` items
//! (`Connect` and `HttpRequest`) each carrying a `context` input type. Policies
//! are `permit`/`forbid` rules; evaluation is default-deny and `forbid`
//! overrides `permit`. A request event yields a decision; a response event is
//! recorded as history that temporal clauses (`when temporal { formerly ... }`)
//! can reason over.
//!
//! # Fail-closed
//!
//! Every engine error, every non-decision outcome, and every policy that fails
//! to install is treated as a **deny**. The embedding proxy enforces the
//! verdict; this decision point never fails open.
//!
//! # Allowlist bridge
//!
//! Dogwood matches context fields by exact string equality, so the dynamic host
//! allowlist (legacy `EGRESS_ALLOWLIST`) is bridged two ways working together:
//! the configured hosts are rendered into generated `permit` rules in the policy
//! source at load time (one rule per host), and the existing egress dot-boundary
//! subdomain semantics are applied by [`normalize_to_allowed_host`] *before* the
//! policy call, so a subdomain of a permitted host is decided against its parent
//! exactly as today's [`crate::egress`] `Allowlist` does.

use std::sync::Mutex;

use dogwood_language::{Event, EventBuilder, Value};
use dogwood_local_engine::{DurableTemporalEngine, Outcome};

/// The egress gateway schema (Cedar-derived Dogwood syntax).
///
/// Field names here are the stable contract FEAT-003/004 build events against:
/// `Connect` carries `context.input.host`; `HttpRequest` carries
/// `context.input.host`, `context.input.method`, and `context.input.path`.
pub const SCHEMA: &str = r#"
namespace Egress {
  type ConnectInput = { host: String };
  type HttpInput = { host: String, method: String, path: String };
  entity Box = { instance_id: String } tags String;
  entity Gateway;
  action "Connect" appliesTo {
    principal: [Box],
    resource: [Gateway],
    context: { input: ConnectInput }
  };
  action "HttpRequest" appliesTo {
    principal: [Box],
    resource: [Gateway],
    context: { input: HttpInput }
  };
}
"#;

/// The action-type path for this schema's actions (the `Action` component
/// included), used with [`Event::builder_for`] so hosts/methods/paths that
/// contain `:` never get mangled by the combined-string parser.
const ACTION_NAMESPACE: &[&str] = &["Egress", "Action"];

/// The `Gateway` resource every egress event acts on.
const GATEWAY_UID: &str = "Egress::Gateway::\"egress\"";

/// Snapshot cadence for the durable engine: snapshot every N appended events.
/// Non-zero so the on-disk log stays bounded on a long-lived gateway.
const SNAPSHOT_INTERVAL: u64 = 1000;

/// The decision returned by the egress policy: an allow/deny plus a human
/// reason suitable for a decision record or a debug log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressDecision {
    /// Whether the request is permitted. `false` is the fail-closed default.
    pub allowed: bool,
    /// Why the decision was reached, for audit/diagnostics.
    pub reason: String,
}

impl EgressDecision {
    /// An allow decision with a reason.
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
        }
    }

    /// A deny decision with a reason. This is the fail-closed outcome.
    fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
        }
    }
}

/// The egress policy decision point: a Dogwood engine plus the normalized host
/// allowlist used to map subdomains onto their permitted parent before the
/// policy call.
///
/// The engine is held behind a [`Mutex`] because Dogwood's `submit` takes
/// `&mut self`. The guard is acquired, the event submitted, the verdict copied
/// out, and the guard dropped synchronously — it is never held across an
/// `.await` (the `await_holding_lock` lint is denied), so the public methods are
/// deliberately synchronous.
pub struct EgressPolicy {
    engine: Mutex<DurableTemporalEngine>,
    /// Normalized allowlist entries, used only to fold a subdomain onto its
    /// permitted parent host before handing the host to the engine.
    allow_hosts: Vec<String>,
}

impl EgressPolicy {
    /// Build an egress policy from a legacy `EGRESS_ALLOWLIST` CSV, preserving
    /// backward compatibility: each listed host (and its subdomains, via the
    /// existing dot-boundary semantics) permits `Connect`.
    ///
    /// The engine is opened at `store_path`; that redb-backed store holds the
    /// temporal EVENT HISTORY the engine recovers across restarts (the prior
    /// Connect/HttpRequest events temporal clauses read). It does not hold the
    /// policy itself; the policy is the Dogwood source installed here.
    ///
    /// # Errors
    ///
    /// Returns an error when the temporal event-history store cannot be opened,
    /// or when the generated default policy or schema fails to install.
    pub fn from_allowlist_csv(raw: &str, store_path: &std::path::Path) -> anyhow::Result<Self> {
        let hosts = parse_hosts(raw);
        let policy = default_policy_from_hosts(&hosts);
        Self::install(&policy, store_path, hosts)
    }

    /// Build an egress policy from an explicit Dogwood policy source, for the
    /// richer future configuration path. The caller supplies the full policy
    /// text; `allow_hosts` are the normalized hosts used for subdomain folding
    /// before the policy call (pass an empty slice to disable folding).
    ///
    /// # Errors
    ///
    /// Returns an error when the temporal event-history store cannot be opened,
    /// or when the supplied policy or schema fails to install.
    pub fn from_policy_source(
        policy_source: &str,
        store_path: &std::path::Path,
        allow_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        Self::install(policy_source, store_path, allow_hosts)
    }

    /// Open the durable temporal event-history engine and install `policy_source`
    /// against [`SCHEMA`].
    fn install(
        policy_source: &str,
        store_path: &std::path::Path,
        allow_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        let mut engine = DurableTemporalEngine::open(store_path, SNAPSHOT_INTERVAL)
            .map_err(|e| anyhow::anyhow!("open dogwood store: {e}"))?;
        engine
            .install(policy_source, SCHEMA, None, None)
            .map_err(|e| anyhow::anyhow!("install egress policy: {e}"))?;
        Ok(Self {
            engine: Mutex::new(engine),
            allow_hosts,
        })
    }

    /// Decide whether `subject` (a box instance id) may `Connect` to `host`.
    ///
    /// Fails closed: a subdomain is first folded onto its permitted parent, then
    /// the Dogwood engine decides. Any engine error or non-decision outcome
    /// yields a deny.
    pub fn decide_connect(&self, subject: &str, host: &str) -> EgressDecision {
        let host = normalize_to_allowed_host(host, &self.allow_hosts);
        let event = Event::builder_for(ACTION_NAMESPACE, "Connect", "request")
            .principal(&box_uid(subject))
            .resource(GATEWAY_UID)
            .field("input", "host", Value::String(host.clone()))
            .request_context("input", "host", Value::String(host));
        self.decide(event, "Connect")
    }

    /// Decide whether `subject` may issue an `HttpRequest` with `method` and
    /// `path` to `host`.
    ///
    /// Fails closed, with the same subdomain folding and error handling as
    /// [`Self::decide_connect`].
    pub fn decide_http(
        &self,
        subject: &str,
        host: &str,
        method: &str,
        path: &str,
    ) -> EgressDecision {
        let host = normalize_to_allowed_host(host, &self.allow_hosts);
        let event = Event::builder_for(ACTION_NAMESPACE, "HttpRequest", "request")
            .principal(&box_uid(subject))
            .resource(GATEWAY_UID)
            .field("input", "host", Value::String(host.clone()))
            .field("input", "method", Value::String(method.to_string()))
            .field("input", "path", Value::String(path.to_string()))
            .request_context("input", "host", Value::String(host))
            .request_context("input", "method", Value::String(method.to_string()))
            .request_context("input", "path", Value::String(path.to_string()));
        self.decide(event, "HttpRequest")
    }

    /// Record a response event in the engine's history so temporal clauses can
    /// reason over it. Response events get no verdict; a failure is logged and
    /// swallowed (it only means a later temporal permit will not fire, which is
    /// the fail-closed direction).
    ///
    /// `fields` are added to the response event's `input` group.
    pub fn record_response(&self, subject: &str, action: &str, fields: &[(&str, &str)]) {
        let mut event = Event::builder_for(ACTION_NAMESPACE, action, "response")
            .principal(&box_uid(subject))
            .resource(GATEWAY_UID);
        for (name, value) in fields {
            event = event.field("input", name, Value::String((*value).to_string()));
        }
        let Ok(mut guard) = self.engine.lock() else {
            tracing::error!(
                action,
                "egress policy engine mutex poisoned; response dropped"
            );
            return;
        };
        if let Err(e) = guard.submit(event) {
            tracing::warn!(action, error = %e, "egress policy response record failed");
        }
    }

    /// Submit a request `event` and translate the outcome into an
    /// [`EgressDecision`], failing closed on any error or non-decision outcome.
    fn decide(&self, event: EventBuilder, action: &str) -> EgressDecision {
        let outcome = {
            let Ok(mut guard) = self.engine.lock() else {
                return EgressDecision::deny("egress policy engine mutex poisoned");
            };
            // Submit under the lock, copy the verdict out, drop the guard before
            // returning — never held across an await.
            match guard.submit(event) {
                Ok(submitted) => submitted.outcome,
                Err(e) => {
                    tracing::warn!(action, error = %e, "egress policy submit failed; deny");
                    return EgressDecision::deny(format!("engine error: {e}"));
                }
            }
        };
        match outcome {
            Outcome::Decision(response) => {
                if response.allowed() {
                    EgressDecision::allow(decision_reason(&response, "permit"))
                } else {
                    EgressDecision::deny(decision_reason(&response, "default-deny"))
                }
            }
            Outcome::Recorded => {
                tracing::error!(action, "egress request unexpectedly recorded; deny");
                EgressDecision::deny("request event was recorded, not decided")
            }
        }
    }
}

/// Summarize the determining policies of a decision for a reason string.
fn decision_reason(response: &dogwood_local_engine::DecisionResponse, fallback: &str) -> String {
    let ids: Vec<&str> = response
        .diagnostics()
        .reason()
        .filter_map(|p| p.annotation_id())
        .collect();
    if ids.is_empty() {
        fallback.to_string()
    } else {
        ids.join(",")
    }
}

/// The `Box` principal entity UID for a box instance id.
fn box_uid(instance_id: &str) -> String {
    // Escape embedded quotes/backslashes so a crafted instance id cannot break
    // out of the entity-id string literal.
    let escaped = instance_id.replace('\\', "\\\\").replace('"', "\\\"");
    format!("Egress::Box::\"{escaped}\"")
}

/// Parse a comma-separated allowlist into normalized host entries, matching the
/// existing [`crate::egress`] `Allowlist` parsing (lowercase, trim, strip dots,
/// drop empties).
fn parse_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|e| e.trim().trim_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Fold `host` onto a permitted parent when it is a subdomain of an allowlist
/// entry, reproducing the existing egress dot-boundary semantics. A host that is
/// exactly an entry, or not covered at all, is returned normalized but unchanged
/// (an uncovered host stays itself and is denied by default).
fn normalize_to_allowed_host(host: &str, allow_hosts: &[String]) -> String {
    let host = host.trim_matches('.').to_ascii_lowercase();
    for entry in allow_hosts {
        if host == *entry || host.ends_with(&format!(".{entry}")) {
            return entry.clone();
        }
    }
    host
}

/// Render a default-deny policy that permits `Connect` to each configured host.
///
/// Default-deny is implicit in Dogwood (a request with no matching `permit` is
/// denied); this emits one annotated `permit` per host. Hosts are matched by
/// exact equality on `context.input.host` — subdomains are folded onto their
/// parent by [`normalize_to_allowed_host`] before the policy call.
fn default_policy_from_hosts(hosts: &[String]) -> String {
    let mut policy = String::new();
    for (i, host) in hosts.iter().enumerate() {
        let escaped = host.replace('\\', "\\\\").replace('"', "\\\"");
        policy.push_str(&format!(
            "@id(\"allow_connect_{i}\")\n\
             permit (principal, action == Egress::Action::\"Connect\", resource)\n\
             when {{ context.input.host == \"{escaped}\" }};\n\n"
        ));
    }
    policy
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    /// A fresh engine store under a unique temp directory per test.
    fn store_path(name: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("devbox-egress-policy-{name}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.redb")
    }

    fn policy_from_csv(name: &str, csv: &str) -> EgressPolicy {
        EgressPolicy::from_allowlist_csv(csv, &store_path(name)).unwrap()
    }

    #[test]
    fn default_deny_denies_unconfigured_host() {
        let policy = policy_from_csv("deny", "crates.io");
        let d = policy.decide_connect("i-123", "evil.com");
        assert!(!d.allowed, "unconfigured host must be denied: {d:?}");
    }

    #[test]
    fn permitted_host_allows_connect() {
        let policy = policy_from_csv("allow", "crates.io, github.com");
        assert!(policy.decide_connect("i-123", "crates.io").allowed);
        assert!(policy.decide_connect("i-123", "github.com").allowed);
    }

    #[test]
    fn subdomain_matches_like_egress_allowlist() {
        let policy = policy_from_csv("subdomain", "crates.io, github.com");
        // Subdomains of a permitted host are allowed (static.crates.io when
        // crates.io is), exactly like egress::Allowlist.
        assert!(policy.decide_connect("i-1", "static.crates.io").allowed);
        assert!(policy.decide_connect("i-1", "index.crates.io").allowed);
        assert!(policy.decide_connect("i-1", "codeload.github.com").allowed);
        // A suffix match must be on a dot boundary, not a substring.
        assert!(!policy.decide_connect("i-1", "notcrates.io").allowed);
        assert!(!policy.decide_connect("i-1", "crates.io.evil.com").allowed);
    }

    #[test]
    fn allowlist_outcome_parity_with_egress_allowlist() {
        // Representative host set decided through the Dogwood policy must match
        // the legacy egress Allowlist's allow/deny outcome for each host.
        let csv = "crates.io, static.crates.io, github.com";
        let policy = policy_from_csv("parity", csv);
        let cases = [
            ("crates.io", true),
            ("static.crates.io", true),
            ("index.crates.io", true),
            ("github.com", true),
            ("codeload.github.com", true),
            ("evil.com", false),
            ("notcrates.io", false),
            ("crates.io.evil.com", false),
        ];
        for (host, expected) in cases {
            let got = policy.decide_connect("i-1", host).allowed;
            assert_eq!(got, expected, "host {host} parity mismatch");
        }
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let policy = policy_from_csv("empty", "");
        assert!(!policy.decide_connect("i-1", "crates.io").allowed);
    }

    #[test]
    fn http_request_discriminates_method_and_path() {
        // Permit only GET /v1/data to api.example.com; everything else denied.
        let policy = r#"
@id("allow_get_data")
permit (principal, action == Egress::Action::"HttpRequest", resource)
when {
  context.input.host == "api.example.com" &&
  context.input.method == "GET" &&
  context.input.path == "/v1/data"
};
"#;
        let engine = EgressPolicy::from_policy_source(
            policy,
            &store_path("http"),
            vec!["api.example.com".to_string()],
        )
        .unwrap();
        assert!(
            engine
                .decide_http("i-1", "api.example.com", "GET", "/v1/data")
                .allowed
        );
        // Wrong method.
        assert!(
            !engine
                .decide_http("i-1", "api.example.com", "POST", "/v1/data")
                .allowed
        );
        // Wrong path.
        assert!(
            !engine
                .decide_http("i-1", "api.example.com", "GET", "/v1/other")
                .allowed
        );
    }

    #[test]
    fn forbid_overrides_permit() {
        let policy = r#"
@id("allow_all_connect")
permit (principal, action == Egress::Action::"Connect", resource);

@id("forbid_quarantined")
forbid (principal, action == Egress::Action::"Connect", resource)
when { context.input.host == "blocked.example.com" };
"#;
        let engine =
            EgressPolicy::from_policy_source(policy, &store_path("forbid"), Vec::new()).unwrap();
        // A host the permit would allow is still denied by the forbid.
        assert!(!engine.decide_connect("i-1", "blocked.example.com").allowed);
        // Another host is allowed by the blanket permit.
        assert!(engine.decide_connect("i-1", "ok.example.com").allowed);
    }

    #[test]
    fn temporal_clause_gates_on_prior_event() {
        // HttpRequest is permitted only if the same box recorded a Connect
        // response within the last hour. This exercises the temporal engine's
        // event history: a request is denied based on earlier recorded events.
        let policy = r#"
@id("allow_connect")
permit (principal, action == Egress::Action::"Connect", resource);

@id("http_after_connect")
permit (principal, action == Egress::Action::"HttpRequest", resource)
when temporal {
  formerly within 1h Egress::Action::"Connect"::response{ input.host: context.input.host }
};
"#;
        let engine = EgressPolicy::from_policy_source(
            policy,
            &store_path("temporal"),
            vec!["api.example.com".to_string()],
        )
        .unwrap();
        // Before any connect, the HttpRequest has no prior event and is denied.
        assert!(
            !engine
                .decide_http("i-1", "api.example.com", "GET", "/")
                .allowed,
            "http without a prior connect must be denied by the temporal clause"
        );
        // Record a Connect response for the same host...
        engine.record_response("i-1", "Connect", &[("host", "api.example.com")]);
        // ...now the HttpRequest is permitted because a Connect response exists
        // within the window.
        assert!(
            engine
                .decide_http("i-1", "api.example.com", "GET", "/")
                .allowed,
            "http after a recorded connect within the window must be allowed"
        );
    }

    #[test]
    fn fail_closed_when_action_not_in_schema() {
        // An event whose action is not in the installed schema is unservable;
        // the engine errors and the decision must fail closed (deny).
        let policy = policy_from_csv("failclosed", "crates.io");
        let event = Event::builder_for(ACTION_NAMESPACE, "Nonexistent", "request")
            .principal(&box_uid("i-1"))
            .resource(GATEWAY_UID)
            .field("input", "host", Value::String("crates.io".to_string()))
            .request_context("input", "host", Value::String("crates.io".to_string()));
        let decision = policy.decide(event, "Nonexistent");
        assert!(
            !decision.allowed,
            "unservable event must fail closed: {decision:?}"
        );
    }
}
