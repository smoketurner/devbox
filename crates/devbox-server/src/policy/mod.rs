//! Egress policy decision point backed by a native, in-crate rule engine.
//!
//! This module implements a small permit/forbid evaluator tailored to the
//! devbox egress gates. It wraps the rule set behind a devbox-specific API —
//! [`EgressPolicy`] — that the CONNECT host gate ([`crate::egress`]) and the
//! method/path-aware reverse proxy ([`crate::egress_proxy`]) consume as a stable
//! internal interface.
//!
//! # Model
//!
//! A policy is an ordered list of [`Rule`]s. Each rule has an [`Effect`]
//! (`Permit`/`Forbid`), a target [`Action`] (`Connect` or `HttpRequest`), an
//! exact-match `host`, optional exact-match `method`/`path` (HTTP only), and an
//! `id` used as the decision reason string. Evaluation is **default-deny** (a
//! request matching no `permit` is denied) and **forbid overrides permit** (a
//! single matching `forbid` denies regardless of any permit).
//!
//! Matching is exact string equality on `host` (Connect) and on
//! `host` + `method` + `path` (HttpRequest). Because matching compares plain
//! Rust strings, a crafted instance id or host cannot "break out" of the matcher
//! the way a string interpolated into a policy DSL could — there is no DSL to
//! inject into.
//!
//! # Fail-closed
//!
//! Every non-matching request, and every request for an action the policy does
//! not cover, is a **deny**. The embedding proxy enforces the verdict; this
//! decision point never fails open.
//!
//! # Allowlist bridge
//!
//! Hosts are matched by exact equality, so the dynamic host allowlist (legacy
//! `EGRESS_ALLOWLIST`) is bridged two ways working together: the configured
//! hosts are rendered into generated `permit`-`Connect` rules (one per host),
//! and the egress dot-boundary subdomain semantics are applied by
//! [`normalize_to_allowed_host`] *before* matching, so a subdomain of a
//! permitted host is decided against its parent exactly as today's
//! [`crate::egress`] `Allowlist` does.
//!
//! # Custom policy source (`EGRESS_POLICY_PATH`)
//!
//! The custom-policy escape hatch loads a **JSON** document (replacing the
//! removed Dogwood/Cedar `.dw` syntax). The document is a JSON array of rule
//! objects:
//!
//! ```json
//! [
//!   { "effect": "permit", "action": "connect", "host": "crates.io", "id": "allow_crates" },
//!   {
//!     "effect": "permit",
//!     "action": "http_request",
//!     "host": "api.anthropic.com",
//!     "method": "POST",
//!     "path": "/v1/messages",
//!     "id": "allow_messages"
//!   },
//!   { "effect": "forbid", "action": "connect", "host": "blocked.example.com" }
//! ]
//! ```
//!
//! Field reference:
//! - `effect`: `"permit"` or `"forbid"` (required).
//! - `action`: `"connect"` or `"http_request"` (required).
//! - `host`: exact host to match (required).
//! - `method`, `path`: optional exact matches; only meaningful for
//!   `http_request` rules. When omitted, any method/path matches.
//! - `id`: optional annotation surfaced in the decision reason. Defaults to the
//!   rule's effect name when omitted.
//!
//! This is a deliberate, documented behaviour change from the previous
//! Dogwood-based `.dw` source. The temporal `formerly within <dur>` clause the
//! old engine supported is **not** reimplemented: it was tested-only and never
//! wired into any production decision path, so it is dropped.

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

/// A rule effect: whether a matching rule permits or forbids the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Effect {
    /// Allow a matching request (unless a `Forbid` also matches).
    Permit,
    /// Deny a matching request; overrides any `Permit`.
    Forbid,
}

/// The egress action a rule targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// A CONNECT tunnel to a host (host-only matching).
    Connect,
    /// A reverse-proxied HTTP request (host + optional method/path matching).
    HttpRequest,
}

/// A single egress policy rule.
///
/// `method`/`path` are only consulted for [`Action::HttpRequest`] rules; when
/// `None` they match any method/path. The `id` is surfaced as the decision
/// reason so [`crate::egress::DecisionRecord`] reasons stay meaningful.
#[derive(Clone, Debug, serde::Deserialize)]
struct Rule {
    /// Permit or forbid.
    effect: Effect,
    /// The action this rule applies to.
    action: Action,
    /// The exact host this rule matches.
    host: String,
    /// Optional exact method match (HTTP only).
    #[serde(default)]
    method: Option<String>,
    /// Optional exact path match (HTTP only).
    #[serde(default)]
    path: Option<String>,
    /// Optional annotation surfaced in the decision reason.
    #[serde(default)]
    id: Option<String>,
}

/// A policy rule constructed programmatically by another module (rather than
/// parsed from a JSON source), used to compile per-integration method/path
/// scoping into the shared policy (see [`EgressPolicy::extend_rules`]).
///
/// This is the public, constructable mirror of the private [`Rule`]: callers
/// outside this module cannot name `Rule`/`Effect`/`Action` directly, so they
/// build a `PolicyRule` through [`PolicyRule::permit_http`] and the policy
/// converts it internally.
#[derive(Clone, Debug)]
pub struct PolicyRule {
    rule: Rule,
}

impl PolicyRule {
    /// A `permit` rule for an HTTP request to `host` scoped to `method` and, when
    /// `path` is `Some`, that exact `path` (a `None` path permits any path on the
    /// host for that method). `id` is surfaced in the decision reason.
    pub fn permit_http(
        host: impl Into<String>,
        method: impl Into<String>,
        path: Option<String>,
        id: impl Into<String>,
    ) -> Self {
        Self {
            rule: Rule {
                effect: Effect::Permit,
                action: Action::HttpRequest,
                host: host.into(),
                method: Some(method.into()),
                path,
                id: Some(id.into()),
            },
        }
    }

    /// Consume into the private internal [`Rule`].
    fn into_rule(self) -> Rule {
        self.rule
    }
}

impl Rule {
    /// Whether this rule matches a `Connect` request to `host`.
    fn matches_connect(&self, host: &str) -> bool {
        matches!(self.action, Action::Connect) && self.host == host
    }

    /// Whether this rule matches an `HttpRequest` to `host`/`method`/`path`.
    /// An absent `method`/`path` on the rule is a wildcard for that field.
    fn matches_http(&self, host: &str, method: &str, path: &str) -> bool {
        matches!(self.action, Action::HttpRequest)
            && self.host == host
            && self.method.as_deref().is_none_or(|m| m == method)
            && self.path.as_deref().is_none_or(|p| p == path)
    }

    /// The reason string for a decision attributed to this rule.
    fn reason(&self) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => match self.effect {
                Effect::Permit => "permit".to_string(),
                Effect::Forbid => "forbid".to_string(),
            },
        }
    }
}

/// The egress policy decision point: an ordered rule set plus the normalized
/// host allowlist used to map subdomains onto their permitted parent before
/// matching.
///
/// The decision methods are synchronous and lock-free: evaluation only reads the
/// immutable rule set, so decisions are cheap and never held across an `.await`.
pub struct EgressPolicy {
    rules: Vec<Rule>,
    /// Normalized allowlist entries, used only to fold a subdomain onto its
    /// permitted parent host before matching.
    allow_hosts: Vec<String>,
}

impl EgressPolicy {
    /// Build an egress policy from a legacy `EGRESS_ALLOWLIST` CSV, preserving
    /// backward compatibility: each listed host (and its subdomains, via the
    /// existing dot-boundary semantics) permits `Connect`.
    ///
    /// # Errors
    ///
    /// This constructor is infallible today, but returns `Result` to keep the
    /// call sites stable and allow future validation; it never returns `Err`.
    pub fn from_allowlist_csv(raw: &str) -> anyhow::Result<Self> {
        let hosts = parse_hosts(raw);
        let rules = default_rules_from_hosts(&hosts);
        Ok(Self {
            rules,
            allow_hosts: hosts,
        })
    }

    /// Build an egress policy from an explicit native policy source (JSON; see
    /// the module docs), for the richer custom-configuration path. The caller
    /// supplies the full policy document; `allow_hosts` are the normalized hosts
    /// used for subdomain folding before matching (pass an empty vec to disable
    /// folding).
    ///
    /// # Errors
    ///
    /// Returns an error when `policy_source` is not valid JSON or does not match
    /// the documented rule-array schema.
    pub fn from_policy_source(
        policy_source: &str,
        allow_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        let rules: Vec<Rule> = serde_json::from_str(policy_source)
            .map_err(|e| anyhow::anyhow!("parse egress policy source: {e}"))?;
        Ok(Self { rules, allow_hosts })
    }

    /// Append pre-parsed [`PolicyRule`]s to this policy's rule set.
    ///
    /// Used to **compile per-integration method/path scoping** (defined inline in
    /// `EGRESS_UPSTREAMS`, see [`crate::egress_proxy`]) into the single shared
    /// policy at startup, so the reverse proxy's `decide_http` authorization flows
    /// through the same rule set as the CONNECT gate — one authorization path, no
    /// parallel ad-hoc check. Rules are appended, so a pre-existing `forbid` still
    /// overrides a later integration `permit` for the same target (forbid-override
    /// is order-independent).
    pub fn extend_rules(&mut self, rules: impl IntoIterator<Item = PolicyRule>) {
        self.rules
            .extend(rules.into_iter().map(PolicyRule::into_rule));
    }

    /// Decide whether `subject` (a box instance id) may `Connect` to `host`.
    ///
    /// Fails closed: a subdomain is first folded onto its permitted parent, then
    /// the rule set is evaluated (forbid overrides permit, default-deny).
    ///
    /// `subject` does not participate in matching today (the CONNECT hop uses a
    /// coarse constant subject); it is accepted for API stability and future
    /// per-subject rules.
    pub fn decide_connect(&self, _subject: &str, host: &str) -> EgressDecision {
        let host = normalize_to_allowed_host(host, &self.allow_hosts);
        self.evaluate(|rule| rule.matches_connect(&host))
    }

    /// Decide whether `subject` may issue an `HttpRequest` with `method` and
    /// `path` to `host`.
    ///
    /// Fails closed, with the same subdomain folding and forbid-override /
    /// default-deny semantics as [`Self::decide_connect`].
    pub fn decide_http(
        &self,
        _subject: &str,
        host: &str,
        method: &str,
        path: &str,
    ) -> EgressDecision {
        let host = normalize_to_allowed_host(host, &self.allow_hosts);
        self.evaluate(|rule| rule.matches_http(&host, method, path))
    }

    /// Record a response event.
    ///
    /// Retained as a no-op for API stability. The previous Dogwood engine kept a
    /// temporal event history that `formerly within <dur>` clauses read; that
    /// feature was tested-only and never wired into a production decision path,
    /// so it is dropped. Callers may still invoke this without effect.
    pub fn record_response(&self, _subject: &str, _action: &str, _fields: &[(&str, &str)]) {}

    /// Evaluate the rule set against a matcher: a matching `forbid` denies, else
    /// a matching `permit` allows, else default-deny.
    fn evaluate(&self, matches: impl Fn(&Rule) -> bool) -> EgressDecision {
        let mut permit: Option<&Rule> = None;
        for rule in &self.rules {
            if !matches(rule) {
                continue;
            }
            match rule.effect {
                // A forbid short-circuits: it overrides any permit.
                Effect::Forbid => return EgressDecision::deny(rule.reason()),
                // Remember the first matching permit, but keep scanning for a
                // later forbid that would override it.
                Effect::Permit => {
                    if permit.is_none() {
                        permit = Some(rule);
                    }
                }
            }
        }
        match permit {
            Some(rule) => EgressDecision::allow(rule.reason()),
            None => EgressDecision::deny("default-deny"),
        }
    }
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

/// Render a default-deny rule set that permits `Connect` to each configured
/// host.
///
/// Default-deny is implicit (a request with no matching `permit` is denied);
/// this emits one annotated `permit`-`Connect` rule per host. Hosts are matched
/// by exact equality — subdomains are folded onto their parent by
/// [`normalize_to_allowed_host`] before matching.
fn default_rules_from_hosts(hosts: &[String]) -> Vec<Rule> {
    hosts
        .iter()
        .enumerate()
        .map(|(i, host)| Rule {
            effect: Effect::Permit,
            action: Action::Connect,
            host: host.clone(),
            method: None,
            path: None,
            id: Some(format!("allow_connect_{i}")),
        })
        .collect()
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
        // Representative host set decided through the native policy must match
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
            [
              {
                "effect": "permit",
                "action": "http_request",
                "host": "api.example.com",
                "method": "GET",
                "path": "/v1/data",
                "id": "allow_get_data"
              }
            ]
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
            [
              { "effect": "permit", "action": "connect", "host": "ok.example.com", "id": "allow_ok" },
              { "effect": "permit", "action": "connect", "host": "blocked.example.com", "id": "allow_blocked" },
              { "effect": "forbid", "action": "connect", "host": "blocked.example.com", "id": "forbid_quarantined" }
            ]
        "#;
        let engine = EgressPolicy::from_policy_source(policy, Vec::new()).unwrap();
        // A host the permit would allow is still denied by the forbid.
        let deny = engine.decide_connect("i-1", "blocked.example.com");
        assert!(!deny.allowed);
        assert_eq!(deny.reason, "forbid_quarantined");
        // Another host is allowed by its permit.
        assert!(engine.decide_connect("i-1", "ok.example.com").allowed);
    }

    #[test]
    fn forbid_overrides_permit_regardless_of_order() {
        // A permit appearing AFTER a forbid for the same host must still be
        // overridden by the forbid — forbid-override is order-independent.
        let policy = r#"
            [
              { "effect": "forbid", "action": "connect", "host": "blocked.example.com", "id": "forbid_first" },
              { "effect": "permit", "action": "connect", "host": "blocked.example.com", "id": "allow_second" }
            ]
        "#;
        let engine = EgressPolicy::from_policy_source(policy, Vec::new()).unwrap();
        assert!(!engine.decide_connect("i-1", "blocked.example.com").allowed);
    }

    #[test]
    fn record_response_is_a_noop() {
        // Temporal history was dropped; record_response is retained as a no-op
        // for API stability and must not change any later decision.
        let policy = r#"
            [
              {
                "effect": "permit",
                "action": "http_request",
                "host": "api.example.com",
                "method": "GET",
                "path": "/",
                "id": "allow_root"
              }
            ]
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
    fn unknown_action_fails_closed() {
        // A policy source with an unrecognized action value fails to parse,
        // which the caller must treat as a hard error (fail closed) rather than
        // silently loading an empty/permissive policy.
        let bad = r#"[{ "effect": "permit", "action": "telepathy", "host": "x" }]"#;
        let err = EgressPolicy::from_policy_source(bad, Vec::new());
        assert!(err.is_err(), "unknown action must not parse");
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
        // The old Dogwood adapter had to escape quotes/backslashes when
        // interpolating the instance id into a Cedar entity-id literal. The
        // native matcher compares plain Rust strings, so there is no DSL to
        // inject into: a host or subject full of quotes/backslashes/policy
        // syntax simply does not equal any configured host and is denied.
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
    fn allowlist_is_infallible() {
        // from_allowlist_csv never errors, even on odd input.
        assert!(EgressPolicy::from_allowlist_csv(", ,, . .").is_ok());
    }
}
