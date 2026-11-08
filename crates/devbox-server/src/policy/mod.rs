//! Egress policy decision point backed by the Dogwood policy language.
//!
//! This module adopts the published [`dogwood_language`] crate (Apache-2.0,
//! from the `dogwood-policy` org) as the egress policy language and evaluation
//! backend, rather than hand-rolling a permit/forbid evaluator. It wraps
//! Dogwood behind a small, devbox-specific API — [`EgressPolicy`] — that the
//! CONNECT host gate ([`crate::egress`]) and the method/path-aware reverse
//! proxy ([`crate::egress_proxy`]) consume as a stable internal interface.
//!
//! # No local engine
//!
//! `dogwood-language` 1.1.0 ships its own built-in, engine-free
//! [`Authorizer`](dogwood_language::Authorizer) assembled from two default
//! backends: a local `CedarPolicyEngine` that makes the decision and an
//! in-memory `InMemoryTemporalEngine`. devbox therefore depends only on
//! `dogwood-language` — there is no `dogwood-local-engine`, no durable/redb
//! event store, and no persisted temporal history. The previous durable
//! `DurableTemporalEngine` and the `formerly within <dur>` temporal history it
//! fed were tested-only and never wired into a production decision path, so
//! they are dropped.
//!
//! # Model
//!
//! Dogwood is Cedar-derived: a [`SCHEMA`] declares a namespace with `entity`
//! items (a `Box` principal and a `Gateway` resource) and `action` items
//! (`Connect` and `HttpRequest`) each carrying a `context` input type. Policies
//! are `permit`/`forbid` rules; evaluation is default-deny and `forbid`
//! overrides `permit`. Each decision builds a request [`Event`] and asks a
//! fresh [`Authorizer`](dogwood_language::Authorizer) for a verdict.
//!
//! A policy here is assembled as Dogwood `.dw` source: generated
//! `@id`-annotated `permit`-`Connect` rules (one per allowlist host), an
//! optional caller-supplied custom `.dw` source, and generated
//! `@id`-annotated `permit`-`HttpRequest` rules compiled from the inline
//! `EGRESS_UPSTREAMS` integration scoping ([`PolicyRule`]). The source and a
//! side list of rule ids (in the SAME order the rules appear in the source)
//! are retained; each decision re-lowers the source and builds a fresh
//! `Authorizer`.
//!
//! # Fresh authorizer per decision
//!
//! [`Authorizer::is_authorized`](dogwood_language::Authorizer::is_authorized)
//! takes `&mut self` and is stateful (it folds each event into accumulated
//! temporal history and assumes strictly-increasing timestamps). Since devbox
//! keeps no temporal history, each `decide_*` call builds a FRESH `Authorizer`
//! from the retained lowered policy set, feeds it exactly one `request` event
//! at timestamp `0`, and drops it. This removes all cross-request temporal
//! bleed and any monotonic-timestamp bookkeeping, and keeps the decision
//! methods synchronous and lock-free (nothing is ever held across an `.await`).
//!
//! # Fail-closed
//!
//! Every lowering/validation error, every evaluation error, and every
//! non-decision outcome is treated as a **deny**. The embedding proxy enforces
//! the verdict; this decision point never fails open.
//!
//! # Host normalization and the allowlist bridge
//!
//! Every host — whether it comes from `EGRESS_ALLOWLIST`, a request, or a
//! compiled integration rule — is normalized the **same** way before it is
//! rendered into a rule or matched ([`normalize_host`]: trim, strip dots,
//! lowercase). Both sides of every comparison flow through that one function,
//! so a host containing uppercase (e.g. `API.Example.com`) can never silently
//! fail to match its own rule.
//!
//! Dogwood matches `context.input.host` by exact string equality, so the
//! dynamic host allowlist (legacy `EGRESS_ALLOWLIST`) is bridged two ways
//! working together: the configured hosts are rendered into generated
//! `permit`-`Connect` rules (one per host), and the egress dot-boundary
//! subdomain semantics are applied by [`normalize_to_allowed_host`] *before*
//! the policy call, so a subdomain of a permitted host is decided against its
//! parent exactly as today's [`crate::egress`] `Allowlist` does.
//!
//! ## Allowlist folding never rewrites a host that has its own rule
//!
//! Subdomain folding exists only to let a subdomain of an `EGRESS_ALLOWLIST`
//! entry be decided against that parent. It must **not** rewrite a host that is
//! itself the subject of an explicit rule — otherwise a configured integration
//! whose host happens to be a subdomain of an allowlisted entry (allowlist
//! `example.com`, integration host `api.example.com`) would be folded onto
//! `example.com` before matching and would never match its own
//! `permit`-`HttpRequest` rule. So folding is applied only when the normalized
//! request host does not already equal some rule's host: an integration's
//! configured host is always matched as-is, and only genuinely uncovered
//! subdomains are folded onto an allowlist parent.

use dogwood_language::{
    Authorizer, Decision, Event, LoweredPolicySet, PolicySchema, ServiceSchema, Value,
};

/// The egress gateway schema (Cedar `.cedarschema` fed to
/// [`PolicySchema::from_cedarschema_str`]).
///
/// Field names here are the stable contract the decision builders build events
/// against: `Connect` carries `context.input.host`; `HttpRequest` carries
/// `context.input.host`, `context.input.method`, and `context.input.path`.
pub const SCHEMA: &str = r#"
namespace Egress {
  entity Box = { instance_id: String };
  entity Gateway;
  action "Connect" appliesTo {
    principal: [Box],
    resource: [Gateway],
    context: { input: { host: String } }
  };
  action "HttpRequest" appliesTo {
    principal: [Box],
    resource: [Gateway],
    context: { input: { host: String, method: String, path: String } }
  };
}
"#;

/// The action-type path for this schema's actions (the `Action` component
/// included), used with [`Event::builder_for`] so hosts/methods/paths that
/// contain `:` never get mangled by the combined-string parser.
const ACTION_NAMESPACE: &[&str] = &["Egress", "Action"];

/// The `Gateway` resource every egress event acts on.
const GATEWAY_UID: &str = "Egress::Gateway::\"egress\"";

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

/// A policy rule constructed programmatically by another module (rather than
/// parsed from a Dogwood source), used to compile per-integration method/path
/// scoping into the shared policy (see [`EgressPolicy::extend_rules`]).
///
/// It is compiled into a generated `@id`-annotated `permit`-`HttpRequest`
/// Dogwood rule appended to the policy source. The `host` is normalized through
/// [`normalize_host`] — the **same** normalization applied to request hosts
/// before matching — so a configured integration host containing uppercase
/// still matches the lowercased request host.
#[derive(Clone, Debug)]
pub struct PolicyRule {
    /// The exact (normalized) host this rule permits.
    host: String,
    /// The exact HTTP method this rule permits.
    method: String,
    /// Optional exact path; `None` permits any path on the host for the method.
    path: Option<String>,
    /// The annotation surfaced in the decision reason.
    id: String,
}

impl PolicyRule {
    /// A `permit` rule for an HTTP request to `host` scoped to `method` and,
    /// when `path` is `Some`, that exact `path` (a `None` path permits any path
    /// on the host for that method). `id` is surfaced in the decision reason.
    pub fn permit_http(
        host: impl Into<String>,
        method: impl Into<String>,
        path: Option<String>,
        id: impl Into<String>,
    ) -> Self {
        Self {
            host: normalize_host(&host.into()),
            method: method.into(),
            path,
            id: id.into(),
        }
    }

    /// Render this rule as an `@id`-annotated `permit`-`HttpRequest` Dogwood
    /// rule. Host/method/path are interpolated as escaped Cedar string literals.
    fn to_dw(&self) -> String {
        let mut conditions = format!(
            "context.input.host == \"{}\" &&\n  context.input.method == \"{}\"",
            escape_cedar_str(&self.host),
            escape_cedar_str(&self.method),
        );
        if let Some(path) = &self.path {
            conditions.push_str(&format!(
                " &&\n  context.input.path == \"{}\"",
                escape_cedar_str(path),
            ));
        }
        format!(
            "@id(\"{}\")\n\
             permit (principal, action == Egress::Action::\"HttpRequest\", resource)\n\
             when {{\n  {conditions}\n}};\n\n",
            escape_cedar_str(&self.id),
        )
    }
}

/// The egress policy decision point: a lowered Dogwood policy set plus the
/// normalized host allowlist used to map subdomains onto their permitted parent
/// before the policy call.
///
/// A fresh [`Authorizer`](dogwood_language::Authorizer) is built from the
/// retained `(policy_src, rule_ids)` for each decision, so the methods are
/// synchronous and lock-free and there is no temporal state to leak between
/// unrelated decisions.
pub struct EgressPolicy {
    /// The full Dogwood `.dw` policy source (generated + custom rules).
    policy_src: String,
    /// Rule ids in the SAME order the rules appear in `policy_src`, keyed by a
    /// [`DogwoodRuleRef`](dogwood_language::DogwoodRuleRef)'s `rule_index`, used
    /// to recover the `@id` reason (the rule ref carries no annotation).
    rule_ids: Vec<String>,
    /// Normalized allowlist entries, used only to fold a subdomain onto its
    /// permitted parent host before the policy call.
    allow_hosts: Vec<String>,
    /// Hosts that are the subject of an explicit rule, used to ensure allowlist
    /// folding never rewrites a host out from under its own rule.
    rule_hosts: Vec<String>,
}

impl EgressPolicy {
    /// Build an egress policy from a legacy `EGRESS_ALLOWLIST` CSV, preserving
    /// backward compatibility: each listed host (and its subdomains, via the
    /// existing dot-boundary semantics) permits `Connect`.
    ///
    /// # Errors
    ///
    /// Returns an error when the generated policy source fails to lower against
    /// [`SCHEMA`]. Infallible in practice for allowlist input.
    pub fn from_allowlist_csv(raw: &str) -> anyhow::Result<Self> {
        let hosts = parse_hosts(raw);
        let (policy_src, rule_ids) = default_policy_from_hosts(&hosts);
        let rule_hosts = hosts.clone();
        Self::build(policy_src, rule_ids, hosts, rule_hosts)
    }

    /// Build an egress policy from an explicit Dogwood policy source (`.dw`
    /// text), for the richer custom-configuration path. The caller supplies the
    /// full policy document; `allow_hosts` are the normalized hosts used for
    /// subdomain folding before the policy call (pass an empty vec to disable
    /// folding).
    ///
    /// # Errors
    ///
    /// Returns an error when `policy_source` is not valid Dogwood `.dw` source
    /// or does not lower against [`SCHEMA`].
    pub fn from_policy_source(
        policy_source: &str,
        allow_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        let rule_ids = scan_rule_ids(policy_source);
        // Populate the folding guard from the custom source's own rule hosts
        // (mirroring how `extend_rules` records each integration rule's host),
        // so a custom `.dw` rule whose host is a subdomain of an
        // `EGRESS_ALLOWLIST` entry is matched against its own rule rather than
        // folded onto the allowlist parent and silently default-denied.
        let rule_hosts = scan_rule_hosts(policy_source);
        Self::build(policy_source.to_string(), rule_ids, allow_hosts, rule_hosts)
    }

    /// Lower `policy_src` against [`SCHEMA`] once to validate it, then retain
    /// the source + id list for per-decision re-lowering.
    fn build(
        policy_src: String,
        rule_ids: Vec<String>,
        allow_hosts: Vec<String>,
        rule_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        // Lower once up front so a malformed source is rejected at construction
        // (fail closed at load), not silently at first decision.
        lower(&policy_src).map_err(|e| anyhow::anyhow!("lower egress policy: {e}"))?;
        Ok(Self {
            policy_src,
            rule_ids,
            allow_hosts,
            rule_hosts,
        })
    }

    /// Append pre-built [`PolicyRule`]s to this policy's rule set.
    ///
    /// Used to **compile per-integration method/path scoping** (defined inline
    /// in `EGRESS_UPSTREAMS`, see [`crate::egress_proxy`]) into the single
    /// shared policy at startup, so the reverse proxy's `decide_http`
    /// authorization flows through the same rule set as the CONNECT gate — one
    /// authorization path, no parallel ad-hoc check. Rules are appended, so a
    /// pre-existing `forbid` still overrides a later integration `permit` for
    /// the same target (forbid-override is order-independent).
    pub fn extend_rules(&mut self, rules: impl IntoIterator<Item = PolicyRule>) {
        for rule in rules {
            self.policy_src.push_str(&rule.to_dw());
            self.rule_ids.push(rule.id.clone());
            self.rule_hosts.push(rule.host);
        }
    }

    /// Decide whether `subject` (a box instance id) may `Connect` to `host`.
    ///
    /// Fails closed: a subdomain is first folded onto its permitted parent, then
    /// a fresh Dogwood [`Authorizer`](dogwood_language::Authorizer) decides. Any
    /// lowering/evaluation error or non-decision outcome yields a deny.
    ///
    /// `subject` does not participate in matching today (the CONNECT hop uses a
    /// coarse constant subject); it is accepted for API stability and future
    /// per-subject rules.
    pub fn decide_connect(&self, subject: &str, host: &str) -> EgressDecision {
        let host = self.match_host(host);
        let event = Event::builder_for(ACTION_NAMESPACE, "Connect", "request")
            .principal(&box_uid(subject))
            .resource(GATEWAY_UID)
            .field("input", "host", Value::String(host.clone()))
            .request_context("input", "host", Value::String(host))
            .build();
        self.decide(&event, "Connect")
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
        let host = self.match_host(host);
        let event = Event::builder_for(ACTION_NAMESPACE, "HttpRequest", "request")
            .principal(&box_uid(subject))
            .resource(GATEWAY_UID)
            .field("input", "host", Value::String(host.clone()))
            .field("input", "method", Value::String(method.to_string()))
            .field("input", "path", Value::String(path.to_string()))
            .request_context("input", "host", Value::String(host))
            .request_context("input", "method", Value::String(method.to_string()))
            .request_context("input", "path", Value::String(path.to_string()))
            .build();
        self.decide(&event, "HttpRequest")
    }

    /// Record a response event.
    ///
    /// Retained as a no-op for API stability. The previous Dogwood durable
    /// engine kept a temporal event history that `formerly within <dur>` clauses
    /// read; that feature was tested-only and never wired into a production
    /// decision path, so it is dropped. Callers may still invoke this without
    /// effect.
    pub fn record_response(&self, _subject: &str, _action: &str, _fields: &[(&str, &str)]) {}

    /// Build a fresh [`Authorizer`](dogwood_language::Authorizer) from the
    /// retained source and decide `event`, failing closed on any lowering /
    /// evaluation error or non-decision outcome.
    fn decide(&self, event: &Event, action: &str) -> EgressDecision {
        let policies = match lower(&self.policy_src) {
            Ok(policies) => policies,
            Err(e) => {
                tracing::warn!(action, error = %e, "egress policy lowering failed; deny");
                return EgressDecision::deny(format!("lowering error: {e}"));
            }
        };
        // A fresh authorizer, fed exactly one request event at ts 0: no temporal
        // bleed, no timestamp bookkeeping, nothing held across an await.
        let mut authorizer = Authorizer::new(policies);
        match authorizer.is_authorized(event) {
            Some(response) => {
                if response.decision() == Decision::Allow {
                    EgressDecision::allow(self.decision_reason(&response, "permit"))
                } else {
                    EgressDecision::deny(self.decision_reason(&response, "default-deny"))
                }
            }
            // A `request`-kind event is always a decision kind; `None` means the
            // event did not decide, which must fail closed.
            None => {
                tracing::error!(action, "egress request did not produce a decision; deny");
                EgressDecision::deny("request event was not decided")
            }
        }
    }

    /// Recover the `@id` reason of the first determining Dogwood rule by mapping
    /// its `rule_index` back to [`Self::rule_ids`]; `fallback` when no rule
    /// determined the decision (an implicit deny).
    fn decision_reason(&self, response: &dogwood_language::Response, fallback: &str) -> String {
        match response.diagnostics().reason().next() {
            Some(rule_ref) => self
                .rule_ids
                .get(rule_ref.rule_index)
                .cloned()
                .unwrap_or_else(|| fallback.to_string()),
            None => fallback.to_string(),
        }
    }

    /// Normalize a request host for matching, folding it onto an allowlist
    /// parent **only when it is not already the subject of an explicit rule**.
    ///
    /// The host is first normalized ([`normalize_host`]: trim, strip dots,
    /// lowercase) exactly as every rule host is. If that normalized host equals
    /// some rule's host (e.g. a configured integration host), it is returned
    /// as-is so it matches its own rule — allowlist folding never rewrites a
    /// host out from under its own rule. Otherwise the dot-boundary subdomain
    /// folding of [`normalize_to_allowed_host`] maps a subdomain of an
    /// allowlisted entry onto that parent.
    fn match_host(&self, host: &str) -> String {
        let host = normalize_host(host);
        if self.rule_hosts.contains(&host) {
            return host;
        }
        normalize_to_allowed_host(&host, &self.allow_hosts)
    }
}

/// Lower a Dogwood `.dw` policy source against [`SCHEMA`] into a
/// [`LoweredPolicySet`](dogwood_language::LoweredPolicySet).
fn lower(policy_src: &str) -> Result<LoweredPolicySet, dogwood_language::Error> {
    let service = ServiceSchema::defaults();
    let policy_schema = PolicySchema::from_cedarschema_str(SCHEMA)?;
    LoweredPolicySet::from_str(policy_src, &service, &policy_schema)
}

/// The `Box` principal entity UID for a box instance id.
fn box_uid(instance_id: &str) -> String {
    // Escape embedded quotes/backslashes so a crafted instance id cannot break
    // out of the entity-id string literal.
    format!("Egress::Box::\"{}\"", escape_cedar_str(instance_id))
}

/// Escape a string for interpolation into a Cedar/Dogwood string literal:
/// backslash first, then double-quote, so a crafted host/id/subject cannot
/// break out of the literal.
fn escape_cedar_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Normalize a single host to its canonical matching form: trimmed of
/// surrounding whitespace, stripped of leading/trailing dots, and lowercased.
///
/// This is the **one** normalization every host flows through — allowlist
/// entries, request hosts, and compiled integration rule hosts
/// ([`PolicyRule::permit_http`]) — so the two sides of every comparison are
/// canonicalized identically and a host's case can never make it fail to match
/// its own rule.
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_matches('.').to_ascii_lowercase()
}

/// Parse a comma-separated allowlist into normalized host entries, matching the
/// existing [`crate::egress`] `Allowlist` parsing (lowercase, trim, strip dots,
/// drop empties).
fn parse_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(normalize_host)
        .filter(|e| !e.is_empty())
        .collect()
}

/// Fold `host` onto a permitted parent when it is a subdomain of an allowlist
/// entry, reproducing the existing egress dot-boundary semantics. A host that
/// is exactly an entry, or not covered at all, is returned normalized but
/// unchanged (an uncovered host stays itself and is denied by default).
///
/// `host` is expected to already be normalized ([`normalize_host`]); callers in
/// this module normalize before calling.
fn normalize_to_allowed_host(host: &str, allow_hosts: &[String]) -> String {
    let host = normalize_host(host);
    for entry in allow_hosts {
        if host == *entry || host.ends_with(&format!(".{entry}")) {
            return entry.clone();
        }
    }
    host
}

/// Render a default-deny policy that permits `Connect` to each configured host,
/// returning the `.dw` source plus the rule ids in source order.
///
/// Default-deny is implicit in Dogwood (a request with no matching `permit` is
/// denied); this emits one `@id`-annotated `permit` per host. Hosts are matched
/// by exact equality on `context.input.host` — subdomains are folded onto their
/// parent by [`normalize_to_allowed_host`] before the policy call.
fn default_policy_from_hosts(hosts: &[String]) -> (String, Vec<String>) {
    let mut policy = String::new();
    let mut ids = Vec::with_capacity(hosts.len());
    for (i, host) in hosts.iter().enumerate() {
        let id = format!("allow_connect_{i}");
        policy.push_str(&format!(
            "@id(\"{}\")\n\
             permit (principal, action == Egress::Action::\"Connect\", resource)\n\
             when {{ context.input.host == \"{}\" }};\n\n",
            escape_cedar_str(&id),
            escape_cedar_str(host),
        ));
        ids.push(id);
    }
    (policy, ids)
}

/// Scan a Dogwood `.dw` source for rule ids in rule order, so a
/// [`DogwoodRuleRef`](dogwood_language::DogwoodRuleRef)'s `rule_index` can be
/// mapped back to the originating `@id`.
///
/// Rules are `permit`/`forbid` statements; each is preceded by at most one
/// `@id("...")` annotation. We walk the source line-wise: an `@id("...")` line
/// records the pending id, and the next `permit`/`forbid` statement consumes it
/// (or an empty string when the rule carried no `@id`). This yields one id per
/// rule in the same order the lowering assigns `rule_index`.
fn scan_rule_ids(source: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut pending: Option<String> = None;
    for line in source.lines() {
        let line = line.trim();
        if let Some(id) = parse_id_annotation(line) {
            pending = Some(id);
        } else if line.starts_with("permit") || line.starts_with("forbid") {
            ids.push(pending.take().unwrap_or_default());
        }
    }
    ids
}

/// Scan a Dogwood `.dw` source for every `context.input.host == "..."` literal,
/// in source order, returning the normalized hosts that are the subject of a
/// rule.
///
/// This mirrors [`scan_rule_ids`]: it walks the source line-wise and recovers
/// each host literal written against `context.input.host`, unescaping it (the
/// inverse of the escaped Cedar literals this module emits) and normalizing it
/// through [`normalize_host`] so it is canonicalized identically to a request
/// host. The result feeds the allowlist-folding guard
/// ([`EgressPolicy::match_host`]) so folding never rewrites a host out from
/// under its own rule on the custom `EGRESS_POLICY_PATH` path, exactly as
/// [`EgressPolicy::extend_rules`] does for compiled integration rules.
fn scan_rule_hosts(source: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in source.lines() {
        if let Some(host) = parse_host_condition(line.trim()) {
            hosts.push(normalize_host(&host));
        }
    }
    hosts
}

/// Parse the host literal from a `context.input.host == "..."` condition line,
/// returning the (unescaped) host. Returns `None` for any line that is not such
/// a condition. Only the leading `context.input.host == "<literal>"` fragment is
/// read, so a compound `when { ... && ... }` line still yields its host.
fn parse_host_condition(line: &str) -> Option<String> {
    let idx = line.find("context.input.host")?;
    let rest = line
        .get(idx..)?
        .strip_prefix("context.input.host")?
        .trim_start();
    let rest = rest.strip_prefix("==")?.trim_start();
    let inner = rest.strip_prefix('"')?;
    // The closing quote is the first unescaped `"`; our emitter only escapes
    // `\"` and `\\`, so track escaping to find it.
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(unescape_cedar_str(inner.get(..i)?));
        }
    }
    None
}

/// Parse an `@id("...")` annotation line, returning the (unescaped) id. Returns
/// `None` for any line that is not an `@id` annotation.
fn parse_id_annotation(line: &str) -> Option<String> {
    let rest = line.strip_prefix("@id")?.trim_start();
    let inner = rest.strip_prefix('(')?.trim_start();
    let inner = inner.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(unescape_cedar_str(inner.get(..end)?))
}

/// Inverse of [`escape_cedar_str`] for the `@id` interior (unescape `\"` and
/// `\\`). Only the two escapes this module emits are handled.
fn unescape_cedar_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
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
    fn default_deny_denies_unconfigured_host() {
        let policy = policy_from_csv("crates.io");
        let d = policy.decide_connect("i-123", "evil.com");
        assert!(!d.allowed, "unconfigured host must be denied: {d:?}");
        assert_eq!(d.reason, "default-deny");
    }

    #[test]
    fn permitted_host_allows_connect() {
        let policy = policy_from_csv("crates.io, github.com");
        assert!(policy.decide_connect("i-123", "crates.io").allowed);
        assert!(policy.decide_connect("i-123", "github.com").allowed);
    }

    #[test]
    fn subdomain_matches_like_egress_allowlist() {
        let policy = policy_from_csv("crates.io, github.com");
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
        let policy = policy_from_csv(csv);
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
        let policy = policy_from_csv("");
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
        let engine =
            EgressPolicy::from_policy_source(policy, vec!["api.example.com".to_string()]).unwrap();
        let allow = engine.decide_http("i-1", "api.example.com", "GET", "/v1/data");
        assert!(allow.allowed);
        assert_eq!(allow.reason, "allow_get_data");
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
        let engine = EgressPolicy::from_policy_source(policy, Vec::new()).unwrap();
        // A host the permit would allow is still denied by the forbid.
        let deny = engine.decide_connect("i-1", "blocked.example.com");
        assert!(!deny.allowed);
        assert_eq!(deny.reason, "forbid_quarantined");
        // Another host is allowed by the blanket permit.
        assert!(engine.decide_connect("i-1", "ok.example.com").allowed);
    }

    #[test]
    fn forbid_overrides_permit_regardless_of_order() {
        // A permit appearing BEFORE a forbid for the same host must still be
        // overridden by the forbid — forbid-override is order-independent.
        let policy = r#"
@id("forbid_first")
forbid (principal, action == Egress::Action::"Connect", resource)
when { context.input.host == "blocked.example.com" };

@id("allow_second")
permit (principal, action == Egress::Action::"Connect", resource)
when { context.input.host == "blocked.example.com" };
"#;
        let engine = EgressPolicy::from_policy_source(policy, Vec::new()).unwrap();
        assert!(!engine.decide_connect("i-1", "blocked.example.com").allowed);
    }

    #[test]
    fn record_response_is_a_noop() {
        // Temporal history was dropped; record_response is retained as a no-op
        // for API stability and must not change any later decision.
        let policy = r#"
@id("allow_root")
permit (principal, action == Egress::Action::"HttpRequest", resource)
when {
  context.input.host == "api.example.com" &&
  context.input.method == "GET" &&
  context.input.path == "/"
};
"#;
        let engine =
            EgressPolicy::from_policy_source(policy, vec!["api.example.com".to_string()]).unwrap();
        // Before any recorded response.
        let before = engine
            .decide_http("i-1", "api.example.com", "GET", "/")
            .allowed;
        engine.record_response("i-1", "Connect", &[("host", "api.example.com")]);
        // After — identical, because record_response does nothing.
        let after = engine
            .decide_http("i-1", "api.example.com", "GET", "/")
            .allowed;
        assert!(before);
        assert_eq!(before, after);
    }

    #[test]
    fn invalid_policy_source_fails_closed() {
        // A policy source that is not valid Dogwood `.dw` must be rejected at
        // construction (fail closed at load) rather than silently loading an
        // empty/permissive policy.
        let bad = "this is not a valid dogwood policy @@@";
        let err = EgressPolicy::from_policy_source(bad, Vec::new());
        assert!(err.is_err(), "invalid policy source must not lower");
    }

    #[test]
    fn allowlist_policy_default_denies_http_requests() {
        // The allowlist-derived policy permits Connect only; an HttpRequest to
        // the same host is default-denied (fail-closed for the uncovered
        // action).
        let policy = policy_from_csv("api.example.com");
        let d = policy.decide_http("i-1", "api.example.com", "POST", "/v1/data");
        assert!(!d.allowed, "allowlist policy must not permit http: {d:?}");
        assert_eq!(d.reason, "default-deny");
    }

    #[test]
    fn crafted_host_or_subject_cannot_break_matching() {
        // The Dogwood adapter escapes quotes/backslashes when interpolating the
        // instance id and hosts into Cedar string/entity-id literals, so a host
        // or subject full of quotes/backslashes/policy syntax cannot break out
        // of the literal: it simply does not equal any configured host and is
        // denied.
        let policy = policy_from_csv("crates.io");
        let nasty_subject = "i-\"); permit(principal, action, resource); //";
        let nasty_host = "crates.io\" || true || \"";
        assert!(
            !policy.decide_connect(nasty_subject, nasty_host).allowed,
            "injection-style host must not match"
        );
        // A legitimate host still works even with a nasty subject.
        assert!(
            policy.decide_connect(nasty_subject, "crates.io").allowed,
            "subject does not affect host matching"
        );
    }

    #[test]
    fn integration_host_with_uppercase_still_matches() {
        // Regression: a compiled integration rule whose host contains uppercase
        // must still match the lowercased request host. Both the rule host (via
        // PolicyRule::permit_http) and the request host (via decide_http) flow
        // through normalize_host, so the case cannot cause a silent default-deny.
        let mut policy = policy_from_csv("");
        policy.extend_rules([PolicyRule::permit_http(
            "API.Example.com",
            "POST",
            Some("/v1/messages".to_string()),
            "egress_up_0",
        )]);
        // Request host arrives lowercased by the HTTP stack; the uppercase rule
        // host still matches it.
        let allow = policy.decide_http("i-1", "api.example.com", "POST", "/v1/messages");
        assert!(
            allow.allowed,
            "uppercase-host integration must match: {allow:?}"
        );
        assert_eq!(allow.reason, "egress_up_0");
        // A mixed-case request host also matches (normalized on both sides).
        assert!(
            policy
                .decide_http("i-1", "Api.Example.Com", "POST", "/v1/messages")
                .allowed
        );
        // The declared method/path scoping still holds (default-deny otherwise).
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "GET", "/v1/messages")
                .allowed
        );
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "POST", "/v1/other")
                .allowed
        );
    }

    #[test]
    fn integration_host_that_is_subdomain_of_allowlist_matches_its_own_rule() {
        // Regression: allowlist "example.com" would fold a request to
        // "api.example.com" onto "example.com" before matching, so an integration
        // whose host is "api.example.com" would never match its own rule and
        // every request would 403. Folding must NOT rewrite a host that is itself
        // the subject of a rule, so the integration host matches as-is.
        let mut policy = policy_from_csv("example.com");
        policy.extend_rules([PolicyRule::permit_http(
            "api.example.com",
            "POST",
            Some("/v1/messages".to_string()),
            "egress_sub_0",
        )]);
        let allow = policy.decide_http("i-1", "api.example.com", "POST", "/v1/messages");
        assert!(
            allow.allowed,
            "subdomain-of-allowlist integration host must match its own rule: {allow:?}"
        );
        assert_eq!(allow.reason, "egress_sub_0");
        // The scoping still default-denies a non-declared method/path on that host.
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "DELETE", "/v1/messages")
                .allowed
        );
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "POST", "/v1/other")
                .allowed
        );
        // A different subdomain of the allowlisted parent (with no integration
        // rule) still has no HttpRequest permit, so it default-denies for HTTP
        // (folding onto example.com only yields a Connect permit, not HTTP).
        assert!(
            !policy
                .decide_http("i-1", "other.example.com", "POST", "/v1/messages")
                .allowed
        );
        // And the allowlisted parent still permits Connect (folding unaffected
        // for hosts without their own rule).
        assert!(policy.decide_connect("i-1", "static.example.com").allowed);
    }

    #[test]
    fn custom_source_host_that_is_subdomain_of_allowlist_matches_its_own_rule() {
        // Regression for the custom `EGRESS_POLICY_PATH` path: a custom `.dw`
        // HttpRequest permit whose host ("api.example.com") is a subdomain of an
        // `EGRESS_ALLOWLIST` entry ("example.com") must match its own rule, not
        // be folded onto the allowlist parent and silently default-denied.
        // `from_policy_source` derives `rule_hosts` from the source's own host
        // literals (via scan_rule_hosts), so folding never rewrites a host out
        // from under its own rule.
        let policy = r#"
@id("allow_api_post")
permit (principal, action == Egress::Action::"HttpRequest", resource)
when {
  context.input.host == "api.example.com" &&
  context.input.method == "POST" &&
  context.input.path == "/v1/data"
};
"#;
        let engine =
            EgressPolicy::from_policy_source(policy, vec!["example.com".to_string()]).unwrap();
        let allow = engine.decide_http("i-1", "api.example.com", "POST", "/v1/data");
        assert!(
            allow.allowed,
            "custom-source subdomain host must match its own rule, not fold: {allow:?}"
        );
        assert_eq!(allow.reason, "allow_api_post");
        // The scoping still default-denies a non-declared method/path on that host.
        assert!(
            !engine
                .decide_http("i-1", "api.example.com", "GET", "/v1/data")
                .allowed
        );
        // A different, uncovered subdomain still default-denies for HTTP (it
        // folds onto example.com, which carries no HttpRequest permit).
        assert!(
            !engine
                .decide_http("i-1", "other.example.com", "POST", "/v1/data")
                .allowed
        );
    }

    #[test]
    fn scan_rule_hosts_recovers_normalized_hosts_in_order() {
        // The scanner mirrors scan_rule_ids: one normalized host per
        // `context.input.host == "..."` condition, unescaping the Cedar literal
        // and lowercasing/trimming exactly as request hosts are.
        let source = r#"
@id("a")
permit (principal, action == Egress::Action::"Connect", resource)
when { context.input.host == "API.Example.com" };

@id("b")
permit (principal, action == Egress::Action::"HttpRequest", resource)
when {
  context.input.host == "two.example.com" &&
  context.input.method == "GET"
};
"#;
        assert_eq!(
            scan_rule_hosts(source),
            vec!["api.example.com".to_string(), "two.example.com".to_string()]
        );
    }

    #[test]
    fn allowlist_is_infallible() {
        // from_allowlist_csv never errors, even on odd input.
        assert!(EgressPolicy::from_allowlist_csv(", ,, . .").is_ok());
    }
}
