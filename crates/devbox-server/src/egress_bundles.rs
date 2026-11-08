//! Built-in, versioned, DEFAULT-ON curated egress bundle catalog.
//!
//! This module ships a small, curated set of outbound egress "bundles" compiled
//! directly into the `devbox-server` binary, so a freshly deployed devbox can
//! reach the package registries every common toolchain needs (cargo, pip, npm,
//! go, container registries, GitHub, apt/yum mirrors) without the operator
//! hand-writing an `EGRESS_ALLOWLIST`, while still honoring every existing egress
//! invariant (default-deny, forbid-overrides-permit, fail-closed, single
//! [`crate::policy::normalize_host`], one authorization path).
//!
//! # Why a static Rust catalog (not embedded TOML/JSON)
//!
//! The catalog is expressed as `const`/`&'static` Rust data ([`PackageBundle`],
//! [`ApiTemplate`]) rather than an embedded TOML/JSON document that is parsed at
//! startup. This choice is deliberate:
//!
//! - **Zero new dependency.** Embedding TOML/JSON would pull in (or lean on) a
//!   deserializer and a parse-at-boot step. Static Rust data needs nothing
//!   beyond what the crate already has, keeping the dependency surface (and the
//!   `cargo deny` license/advisory review) unchanged.
//! - **Compile-time validation.** The type system validates the catalog shape at
//!   compile time: a malformed bundle simply does not compile, so there is no
//!   class of "catalog parse failed at boot" runtime error and no fail-open
//!   window.
//! - **Versioned alongside the code.** The catalog lives in the binary and is
//!   versioned with it; [`CATALOG_VERSION`] is surfaced in startup logging so an
//!   operator can tell exactly which catalog a running server shipped.
//! - **No heap at rest.** The bundles are `&'static` slices of `&'static str`,
//!   so the catalog costs nothing until it is turned into policy rules at boot.
//!
//! # Two bundle kinds
//!
//! 1. **Package-repository bundles** ([`PackageBundle`]): a name plus a list of
//!    hosts reachable over HTTPS. Each host becomes a `permit`-`Connect`
//!    allowlist entry in the shared [`crate::policy::EgressPolicy`], exactly like
//!    an `EGRESS_ALLOWLIST` entry (so dot-boundary subdomain folding still
//!    applies and there is one authorization path). These are what let
//!    cargo/pip/npm/go/containers/github work out of the box.
//! 2. **API integration templates** ([`ApiTemplate`]): a name, a single upstream
//!    host (or host *pattern* for region-parameterized services), and the
//!    method/path scaffolding an integration needs. A template carries **no**
//!    credential, region, or SSM parameter name — those are deployment-specific
//!    and must never be hardcoded.
//!
//! # API templates are fail-closed until an operator supplies a credential
//!
//! A default-on API template contributes host + method/path *scaffolding* to the
//! catalog **by name only**. It contributes **no** `permit`-`HttpRequest` rule
//! and **no** live reverse-proxy upstream until the operator supplies a
//! credential for it. The credential mechanism reuses the existing
//! `EGRESS_UPSTREAMS` seam: when the operator names a template (e.g. `anthropic`)
//! in `EGRESS_UPSTREAMS` and supplies the deployment credential, the existing
//! [`crate::egress_proxy`] path compiles that integration's host/method/path into
//! the shared policy. A template that is NOT named in `EGRESS_UPSTREAMS` is inert:
//! [`template_connect_hosts`] does not emit a Connect host for it and no
//! `permit`-`HttpRequest` rule exists, so `decide_http` default-denies it. This
//! keeps the credential on the control plane (it never reaches the box) and keeps
//! an unconfigured template fully denied (no silent credential-less or wide-open
//! upstream).
//!
//! The templates exist in the catalog so the catalog can *document and name* the
//! supported integrations (and, for AWS Bedrock, declare the host *pattern* and
//! the SigV4 method/path scaffold), not to grant any access on their own.
//!
//! # Selection knob: `EGRESS_DEFAULT_BUNDLES`
//!
//! See [`BundleSelection::parse`] for the exact, documented semantics. In short:
//! unset => all default bundles on; a comma-separated list => only those; the
//! token `none` (or empty-after-trim) => all off (the opt-out switch); unknown
//! names are logged and ignored (fail-safe, not fail-open). Parsing is a pure
//! function of the raw string so tests exercise it without mutating process env.

use crate::policy::{PolicyRule, normalize_host};

/// The catalog version, surfaced in startup logging so an operator can tell
/// which built-in bundle set a running server shipped. Bump on any catalog
/// change (host added/removed, template changed).
pub const CATALOG_VERSION: &str = "1";

/// A package-repository bundle: a name and the HTTPS hosts it permits to
/// `Connect`. Each host becomes a `permit`-`Connect` allowlist entry in the
/// shared policy (with dot-boundary subdomain folding), so the common toolchains
/// work out of the box.
#[derive(Clone, Copy, Debug)]
pub struct PackageBundle {
    /// The individually addressable bundle name (e.g. `"rust"`).
    pub name: &'static str,
    /// The HTTPS hosts this bundle permits.
    pub hosts: &'static [&'static str],
}

/// One method/path scaffold entry for an [`ApiTemplate`]: the HTTP method and an
/// optional exact path (`None` permits any path for that method). This mirrors
/// the `allow` entries of `EGRESS_UPSTREAMS`.
#[derive(Clone, Copy, Debug)]
pub struct ApiAllow {
    /// The HTTP method this entry scopes (e.g. `"POST"`).
    pub method: &'static str,
    /// The exact path (e.g. `"/v1/messages"`), or `None` for any path.
    pub path: Option<&'static str>,
}

/// An API integration template: host (or host *pattern*) plus method/path
/// scaffolding, and **no** credential. A template is inert until the operator
/// supplies a credential by naming it in `EGRESS_UPSTREAMS` (see the module
/// docs); it exists in the catalog to name/document the supported integration
/// and, for region-parameterized services, to declare the host pattern and
/// SigV4 method/path scaffold.
#[derive(Clone, Copy, Debug)]
pub struct ApiTemplate {
    /// The template name (also the `EGRESS_UPSTREAMS` key that activates it).
    pub name: &'static str,
    /// The single upstream host, or a `{region}`-parameterized host *pattern*
    /// for region-scoped services (e.g. AWS Bedrock). The concrete host for a
    /// pattern comes from the operator's region config; the template never
    /// hardcodes a region.
    pub host: &'static str,
    /// The method/path scaffolding this integration uses.
    pub allow: &'static [ApiAllow],
}

/// The package-repository bundles. Each host is reachable over HTTPS; where a
/// subdomain is not covered by a listed parent it is listed explicitly, because
/// the policy's dot-boundary folding only folds a subdomain onto a parent that
/// is itself allowlisted.
pub const PACKAGE_BUNDLES: &[PackageBundle] = &[
    PackageBundle {
        name: "rust",
        hosts: &[
            "crates.io",        // registry API (HTTPS)
            "static.crates.io", // crate downloads (HTTPS)
            "index.crates.io",  // sparse index (HTTPS)
        ],
    },
    PackageBundle {
        name: "python",
        hosts: &[
            "pypi.org",               // PyPI index/API (HTTPS)
            "files.pythonhosted.org", // PyPI package files (HTTPS)
        ],
    },
    PackageBundle {
        name: "node",
        hosts: &[
            "registry.npmjs.org", // npm registry + tarballs (HTTPS)
        ],
    },
    PackageBundle {
        name: "go",
        hosts: &[
            "proxy.golang.org", // Go module proxy (HTTPS)
            "sum.golang.org",   // Go checksum database (HTTPS)
        ],
    },
    PackageBundle {
        name: "containers",
        hosts: &[
            "registry-1.docker.io",                 // Docker Hub registry API (HTTPS)
            "production.cloudflare.docker.com",     // Docker Hub blob storage/CDN (HTTPS)
            "ghcr.io",                              // GitHub Container Registry (HTTPS)
            "pkg-containers.githubusercontent.com", // GHCR blob storage (HTTPS)
            "quay.io",                              // Quay registry API (HTTPS)
            "cdn.quay.io",                          // Quay blob CDN (HTTPS)
            "cdn01.quay.io",                        // Quay blob CDN shard (HTTPS)
            "cdn02.quay.io",                        // Quay blob CDN shard (HTTPS)
            "cdn03.quay.io",                        // Quay blob CDN shard (HTTPS)
            "storage.googleapis.com",               // gcr.io-style blob storage (HTTPS)
        ],
    },
    PackageBundle {
        name: "github",
        hosts: &[
            "github.com",                    // git over HTTPS, release redirects (HTTPS)
            "codeload.github.com",           // archive/tarball downloads (HTTPS)
            "objects.githubusercontent.com", // release asset blob storage (HTTPS)
            "raw.githubusercontent.com",     // raw file content (HTTPS)
        ],
    },
    PackageBundle {
        name: "apt",
        hosts: &[
            "deb.debian.org",      // Debian apt mirror (HTTPS)
            "security.debian.org", // Debian security updates (HTTPS)
            "esm.ubuntu.com",      // Ubuntu ESM (HTTPS)
            "archive.ubuntu.com",  // Ubuntu archive mirror (HTTPS)
            "security.ubuntu.com", // Ubuntu security updates (HTTPS)
        ],
    },
    PackageBundle {
        name: "yum",
        hosts: &[
            "cdn.amazonlinux.com",       // Amazon Linux package CDN (HTTPS)
            "mirrors.fedoraproject.org", // Fedora mirror list (HTTPS)
            "dl.fedoraproject.org",      // Fedora package downloads (HTTPS)
        ],
    },
];

/// The API integration templates. Each carries host + method/path scaffolding
/// and NO credential; it is inert until named in `EGRESS_UPSTREAMS` (see the
/// module docs).
pub const API_TEMPLATES: &[ApiTemplate] = &[
    ApiTemplate {
        name: "anthropic",
        host: "api.anthropic.com",
        allow: &[
            ApiAllow {
                method: "POST",
                path: Some("/v1/messages"),
            },
            ApiAllow {
                method: "GET",
                path: Some("/v1/models"),
            },
        ],
    },
    ApiTemplate {
        name: "openai",
        host: "api.openai.com",
        allow: &[
            ApiAllow {
                method: "POST",
                path: Some("/v1/chat/completions"),
            },
            ApiAllow {
                method: "POST",
                path: Some("/v1/responses"),
            },
        ],
    },
    ApiTemplate {
        // Region-parameterized: the concrete host
        // (`bedrock-runtime.{region}.amazonaws.com`) and the SigV4 region come
        // from the operator's credential/region config. The template declares
        // only the host PATTERN and the `POST /model/*/invoke` scaffold (a `None`
        // path, since the model id is part of the path and is not fixed here).
        name: "aws-bedrock",
        host: "bedrock-runtime.{region}.amazonaws.com",
        allow: &[ApiAllow {
            method: "POST",
            path: None,
        }],
    },
];

/// The operator's selection of which default bundles are active, parsed from
/// `EGRESS_DEFAULT_BUNDLES` via the pure [`BundleSelection::parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleSelection {
    /// All default bundles are active (the `EGRESS_DEFAULT_BUNDLES`-unset
    /// default-on case).
    All,
    /// No default bundles are active (the explicit `none`/empty opt-out).
    None,
    /// Only the named bundles are active (unknown names were dropped with a
    /// warning during parsing). The names are normalized (trimmed, lowercased).
    Only(Vec<String>),
}

impl BundleSelection {
    /// Parse the `EGRESS_DEFAULT_BUNDLES` value into a selection. This is a
    /// **pure** function of the raw option (`None` = env var unset, `Some(raw)` =
    /// the raw value) so tests can drive every branch without mutating process
    /// env.
    ///
    /// Semantics (the documented contract):
    ///
    /// - **unset** (`None`) => [`BundleSelection::All`]: all default bundles on
    ///   (default-on).
    /// - **`none`** (case-insensitive) or **empty after trimming** =>
    ///   [`BundleSelection::None`]: all default bundles off (the opt-out switch).
    /// - a **comma-separated list** of bundle names => [`BundleSelection::Only`]
    ///   with the recognized names; **unknown** names are collected into
    ///   `unknown` for the caller to log as a warning and are otherwise ignored
    ///   (fail-safe, not fail-open). If every listed name is unknown the result
    ///   is `Only(vec![])` (nothing enabled), never "all".
    ///
    /// Returns the selection plus the list of unknown names encountered, so the
    /// caller can emit the warning near the startup log without this function
    /// touching `tracing`.
    pub fn parse(raw: Option<&str>) -> (Self, Vec<String>) {
        let Some(raw) = raw else {
            return (Self::All, Vec::new());
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            return (Self::None, Vec::new());
        }
        let mut selected = Vec::new();
        let mut unknown = Vec::new();
        for token in trimmed.split(',') {
            let name = token.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            if is_known_bundle(&name) {
                if !selected.contains(&name) {
                    selected.push(name);
                }
            } else if !unknown.contains(&name) {
                unknown.push(name);
            }
        }
        (Self::Only(selected), unknown)
    }

    /// Whether a package bundle with `name` is active under this selection.
    fn includes(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Only(names) => names.iter().any(|n| n == name),
        }
    }
}

/// Whether `name` is a known bundle (a package bundle or an API template). Used
/// by [`BundleSelection::parse`] to separate recognized names from unknown ones.
fn is_known_bundle(name: &str) -> bool {
    PACKAGE_BUNDLES.iter().any(|b| b.name == name) || API_TEMPLATES.iter().any(|t| t.name == name)
}

/// The `Connect` allowlist hosts contributed by the active package bundles,
/// normalized through [`normalize_host`] (the same normalization every host
/// flows through), deduplicated, in catalog order.
///
/// These are merged into the shared policy alongside the operator
/// `EGRESS_ALLOWLIST` so they seed `permit`-`Connect` rules and participate in
/// the same dot-boundary subdomain folding and the single authorization path.
///
/// API templates deliberately contribute **no** Connect host here: they are
/// fail-closed and only become reachable (as `permit`-`HttpRequest` rules via
/// `EGRESS_UPSTREAMS`) once the operator supplies a credential.
pub fn connect_hosts(selection: &BundleSelection) -> Vec<String> {
    let mut hosts = Vec::new();
    for bundle in PACKAGE_BUNDLES {
        if !selection.includes(bundle.name) {
            continue;
        }
        for host in bundle.hosts {
            let normalized = normalize_host(host);
            if !normalized.is_empty() && !hosts.contains(&normalized) {
                hosts.push(normalized);
            }
        }
    }
    hosts
}

/// The number of active package bundles and their contributed host count, for
/// startup logging. Returns `(bundle_name, host_count)` pairs in catalog order
/// for the active bundles only.
pub fn active_package_bundles(selection: &BundleSelection) -> Vec<(&'static str, usize)> {
    PACKAGE_BUNDLES
        .iter()
        .filter(|b| selection.includes(b.name))
        .map(|b| (b.name, b.hosts.len()))
        .collect()
}

/// The names of the active API templates under this selection, in catalog
/// order, for startup logging. A listed template is still inert until the
/// operator names it in `EGRESS_UPSTREAMS` with a credential; this reports which
/// templates the selection would make eligible.
pub fn active_api_templates(selection: &BundleSelection) -> Vec<&'static str> {
    API_TEMPLATES
        .iter()
        .filter(|t| selection.includes(t.name))
        .map(|t| t.name)
        .collect()
}

/// Build the `permit`-`HttpRequest` rules for an API template, scoped to each of
/// its `allow` entries, with a stable id `bundle_<name>_<i>`. The `host` is
/// normalized via [`normalize_host`] identically to both the request host
/// (`decide_http`) and the reverse-proxy forward target, so a template rule can
/// never silently default-deny on case.
///
/// This is used ONLY for a template whose credential the operator has supplied
/// out-of-band (not via `EGRESS_UPSTREAMS`, which already compiles its own
/// rules). For a `{region}`-parameterized host pattern, callers must substitute
/// the concrete host before calling, since the policy matches a concrete host by
/// equality. Returns an empty vec for a template with no fixed host.
pub fn template_http_rules(template: &ApiTemplate, host: &str) -> Vec<PolicyRule> {
    let host = normalize_host(host);
    if host.is_empty() {
        return Vec::new();
    }
    template
        .allow
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let id = format!("bundle_{}_{i}", template.name);
            PolicyRule::permit_http(&host, entry.method, entry.path.map(str::to_string), id)
        })
        .collect()
}

/// Look up a package bundle by name (used by tests and callers that need a
/// specific bundle's host list).
pub fn package_bundle(name: &str) -> Option<&'static PackageBundle> {
    PACKAGE_BUNDLES.iter().find(|b| b.name == name)
}

/// Look up an API template by name.
pub fn api_template(name: &str) -> Option<&'static ApiTemplate> {
    API_TEMPLATES.iter().find(|t| t.name == name)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use crate::policy::EgressPolicy;

    /// Build a policy from the operator allowlist CSV seeded with the active
    /// bundle Connect hosts, exactly as `EgressShared::from_env` does, so tests
    /// exercise the real merge (one authorization path) without process env.
    fn policy_with_bundles(operator_csv: &str, selection: &BundleSelection) -> EgressPolicy {
        let mut hosts: Vec<String> = operator_csv
            .split(',')
            .map(normalize_host)
            .filter(|h| !h.is_empty())
            .collect();
        for host in connect_hosts(selection) {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        EgressPolicy::from_allowlist_csv(&hosts.join(",")).unwrap()
    }

    #[test]
    fn catalog_version_is_non_empty() {
        assert!(!CATALOG_VERSION.is_empty());
    }

    #[test]
    fn required_hosts_present_in_catalog() {
        // Each package bundle exists and contains its required representative
        // hosts (guards against an accidental host removal).
        let required: &[(&str, &[&str])] = &[
            (
                "rust",
                &["crates.io", "static.crates.io", "index.crates.io"],
            ),
            ("python", &["pypi.org", "files.pythonhosted.org"]),
            ("node", &["registry.npmjs.org"]),
            ("go", &["proxy.golang.org", "sum.golang.org"]),
            (
                "containers",
                &["registry-1.docker.io", "ghcr.io", "quay.io"],
            ),
            (
                "github",
                &[
                    "github.com",
                    "codeload.github.com",
                    "objects.githubusercontent.com",
                    "raw.githubusercontent.com",
                ],
            ),
            ("apt", &["deb.debian.org", "security.debian.org"]),
            ("yum", &["cdn.amazonlinux.com", "mirrors.fedoraproject.org"]),
        ];
        for (name, hosts) in required {
            let bundle = package_bundle(name);
            assert!(bundle.is_some(), "missing bundle {name}");
            let bundle = bundle.unwrap();
            for host in *hosts {
                assert!(
                    bundle.hosts.contains(host),
                    "bundle {name} missing host {host}"
                );
            }
        }
    }

    #[test]
    fn api_templates_carry_no_credential_and_expected_scaffold() {
        let anthropic = api_template("anthropic").unwrap();
        assert_eq!(anthropic.host, "api.anthropic.com");
        assert!(
            anthropic
                .allow
                .iter()
                .any(|a| a.method == "POST" && a.path == Some("/v1/messages"))
        );
        assert!(
            anthropic
                .allow
                .iter()
                .any(|a| a.method == "GET" && a.path == Some("/v1/models"))
        );

        let openai = api_template("openai").unwrap();
        assert_eq!(openai.host, "api.openai.com");
        assert!(
            openai
                .allow
                .iter()
                .any(|a| a.method == "POST" && a.path == Some("/v1/chat/completions"))
        );
        assert!(
            openai
                .allow
                .iter()
                .any(|a| a.method == "POST" && a.path == Some("/v1/responses"))
        );

        let bedrock = api_template("aws-bedrock").unwrap();
        // Host is a region PATTERN, not a concrete host.
        assert!(bedrock.host.contains("{region}"));
        assert!(bedrock.allow.iter().any(|a| a.method == "POST"));
    }

    #[test]
    fn parse_unset_is_all() {
        let (sel, unknown) = BundleSelection::parse(None);
        assert_eq!(sel, BundleSelection::All);
        assert!(unknown.is_empty());
    }

    #[test]
    fn parse_none_and_empty_are_off() {
        for raw in ["none", "NONE", "  none  ", "", "   "] {
            let (sel, unknown) = BundleSelection::parse(Some(raw));
            assert_eq!(sel, BundleSelection::None, "raw {raw:?}");
            assert!(unknown.is_empty());
        }
    }

    #[test]
    fn parse_list_selects_only_named_known() {
        let (sel, unknown) = BundleSelection::parse(Some("rust, python"));
        assert_eq!(
            sel,
            BundleSelection::Only(vec!["rust".to_string(), "python".to_string()])
        );
        assert!(unknown.is_empty());
    }

    #[test]
    fn parse_unknown_names_are_dropped_and_reported() {
        let (sel, unknown) = BundleSelection::parse(Some("rust, bogus, python"));
        assert_eq!(
            sel,
            BundleSelection::Only(vec!["rust".to_string(), "python".to_string()])
        );
        assert_eq!(unknown, vec!["bogus".to_string()]);
    }

    #[test]
    fn parse_all_unknown_enables_nothing_not_all() {
        let (sel, unknown) = BundleSelection::parse(Some("bogus"));
        assert_eq!(sel, BundleSelection::Only(Vec::new()));
        assert_eq!(unknown, vec!["bogus".to_string()]);
        // And nothing is reachable under it.
        assert!(connect_hosts(&sel).is_empty());
    }

    #[test]
    fn default_on_allows_representative_host_from_each_bundle() {
        // No operator allowlist, default-on selection: a representative host from
        // every package bundle is permitted to Connect, and an unrelated host is
        // denied.
        let policy = policy_with_bundles("", &BundleSelection::All);
        for host in [
            "crates.io",
            "static.crates.io",
            "pypi.org",
            "files.pythonhosted.org",
            "registry.npmjs.org",
            "proxy.golang.org",
            "registry-1.docker.io",
            "ghcr.io",
            "github.com",
            "deb.debian.org",
        ] {
            assert!(
                policy.decide_connect("pool", host).allowed,
                "default-on must allow {host}"
            );
        }
        assert!(
            !policy.decide_connect("pool", "evil.com").allowed,
            "unrelated host must be denied"
        );
    }

    #[test]
    fn opt_out_none_denies_all_bundle_hosts() {
        // With no operator allowlist and the explicit opt-out, every bundle host
        // is denied (deny-all).
        let policy = policy_with_bundles("", &BundleSelection::None);
        for host in ["crates.io", "pypi.org", "github.com", "registry.npmjs.org"] {
            assert!(
                !policy.decide_connect("pool", host).allowed,
                "opt-out must deny {host}"
            );
        }
    }

    #[test]
    fn selective_enable_only_named_bundle() {
        // EGRESS_DEFAULT_BUNDLES=rust: crates.io allowed, pypi.org denied.
        let (sel, _) = BundleSelection::parse(Some("rust"));
        let policy = policy_with_bundles("", &sel);
        assert!(policy.decide_connect("pool", "crates.io").allowed);
        assert!(policy.decide_connect("pool", "static.crates.io").allowed);
        assert!(!policy.decide_connect("pool", "pypi.org").allowed);
    }

    #[test]
    fn unknown_bundle_ignored_rest_applied() {
        // A list with an unknown name still enables the known ones.
        let (sel, unknown) = BundleSelection::parse(Some("rust, bogus"));
        assert_eq!(unknown, vec!["bogus".to_string()]);
        let policy = policy_with_bundles("", &sel);
        assert!(policy.decide_connect("pool", "crates.io").allowed);
        assert!(!policy.decide_connect("pool", "pypi.org").allowed);
    }

    #[test]
    fn forbid_overrides_a_bundled_host() {
        // An operator forbid rule for a bundled host still denies it, even with
        // the bundle default-on (forbid-overrides-permit). Build a policy source
        // that permits all the bundle Connect hosts and then forbids one.
        let mut source = String::new();
        for (i, host) in connect_hosts(&BundleSelection::All).iter().enumerate() {
            source.push_str(&format!(
                "@id(\"bundle_connect_{i}\")\n\
                 permit (principal, action == Egress::Action::\"Connect\", resource)\n\
                 when {{ context.input.host == \"{host}\" }};\n\n"
            ));
        }
        source.push_str(
            "@id(\"forbid_crates\")\n\
             forbid (principal, action == Egress::Action::\"Connect\", resource)\n\
             when { context.input.host == \"crates.io\" };\n\n",
        );
        let policy = EgressPolicy::from_policy_source(&source, Vec::new()).unwrap();
        // crates.io is forbidden despite the bundle permit.
        assert!(!policy.decide_connect("pool", "crates.io").allowed);
        // Another bundled host is still allowed.
        assert!(policy.decide_connect("pool", "pypi.org").allowed);
    }

    #[test]
    fn api_template_is_fail_closed_until_credential_supplied() {
        // Default-on but NO credential: the anthropic template contributes no
        // permit-HttpRequest rule, so decide_http is denied. connect_hosts never
        // includes an API-template host.
        let selection = BundleSelection::All;
        assert!(
            !connect_hosts(&selection).contains(&"api.anthropic.com".to_string()),
            "API template host must not be a Connect host"
        );
        let mut policy = policy_with_bundles("", &selection);
        let denied = policy.decide_http("pool", "api.anthropic.com", "POST", "/v1/messages");
        assert!(
            !denied.allowed,
            "template must be fail-closed without a credential"
        );

        // Once the operator supplies a credential (modeled here by compiling the
        // template's http rules into the policy, exactly as the EGRESS_UPSTREAMS
        // seam does), the scoped request is allowed.
        let anthropic = api_template("anthropic").unwrap();
        policy.extend_rules(template_http_rules(anthropic, anthropic.host));
        let allowed = policy.decide_http("pool", "api.anthropic.com", "POST", "/v1/messages");
        assert!(
            allowed.allowed,
            "template must be live once a credential is supplied"
        );
        // Scoping still holds: an undeclared path is denied.
        assert!(
            !policy
                .decide_http("pool", "api.anthropic.com", "DELETE", "/v1/messages")
                .allowed
        );
    }

    #[test]
    fn template_http_rules_substitutes_concrete_bedrock_host() {
        // The bedrock template is a region PATTERN; rules are built against the
        // operator-substituted concrete host.
        let bedrock = api_template("aws-bedrock").unwrap();
        let host = "bedrock-runtime.us-east-1.amazonaws.com";
        let mut policy = EgressPolicy::from_allowlist_csv("").unwrap();
        policy.extend_rules(template_http_rules(bedrock, host));
        assert!(
            policy
                .decide_http("pool", host, "POST", "/model/anthropic.claude/invoke")
                .allowed,
            "bedrock POST /model/*/invoke must be allowed once realized (None path => any path)"
        );
    }
}
