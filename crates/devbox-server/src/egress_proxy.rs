//! Credential-injecting reverse proxy for server-configured integrations.
//!
//! This is the **exe.dev-style integrations layer**: an operator-configurable
//! registry of outbound integrations where the *control plane* holds the
//! credentials and injects them into upstream requests, so a secret (an API key,
//! an AWS signature) never reaches the box or the agent running on it. A box
//! points a specific flow at `/egress/{integration}/{*rest}`, authenticates with
//! its web-identity token exactly as the git proxy does ([`crate::github::git_proxy`],
//! so the server resolves a verified [`crate::auth::AgentIdentity`]), and the
//! control plane:
//!
//! 1. resolves the named integration to its single configured upstream host
//!    (unknown name => 404);
//! 2. applies the shared [`crate::policy::EgressPolicy`] on that host +
//!    the request `method` + `path` ([`crate::policy::EgressPolicy::decide_http`])
//!    — default-deny, the **same policy instance** as the CONNECT proxy
//!    ([`crate::egress`]), so there is exactly one authorization path (policy
//!    deny => 403);
//! 3. on allow, forwards the request to the configured upstream host over
//!    `https` with a **server-held** credential injected, after stripping any
//!    client-supplied `Authorization`;
//! 4. streams the upstream response back; and
//! 5. emits a decision record for every allow and deny (the same JSONL record
//!    type and sink as the CONNECT proxy, [`crate::egress::DecisionRecord`] /
//!    [`crate::egress::DecisionSink`]).
//!
//! # Separation of concerns
//!
//! The proxy owns **transport + credential injection only**. The allow/deny
//! decision belongs entirely to the [`crate::policy::EgressPolicy`]. The
//! per-integration `method`/`path` scoping declared in `EGRESS_UPSTREAMS` is
//! **compiled into that shared policy at startup** (see
//! [`crate::policy::EgressPolicy::extend_rules`] and [`EgressConfig::policy_rules`]),
//! as `permit`-`HttpRequest` rules for the integration's host. The handler then
//! authorizes through `decide_http` like any other request — it does not run a
//! second, parallel method/path check. The policy host for an integration and
//! the host the proxy forwards to are both derived from the one configured host
//! by the **same** normalization ([`crate::policy::normalize_host`]: trim, strip
//! dots, lowercase), so they are guaranteed identical regardless of the host's
//! case. A configured integration host is matched as-is after that
//! normalization and is **not** folded onto an `EGRESS_ALLOWLIST` parent even
//! when it is a subdomain of one — allowlist subdomain folding never rewrites a
//! host that is itself the subject of a rule (see
//! [`crate::policy`] "Host normalization and the allowlist bridge"). So an
//! integration configured as `API.Example.com`, or as `api.example.com` while
//! `example.com` is allowlisted, still matches its own `permit`-`HttpRequest`
//! rule.
//!
//! # `EGRESS_UPSTREAMS` schema
//!
//! `EGRESS_UPSTREAMS` is a JSON object mapping a stable **integration name** (the
//! `/egress/{name}/` path segment — pick a short, DNS-ish token) to a spec:
//!
//! - `host` (string, required): the single upstream host requests are forwarded
//!   to, e.g. `api.anthropic.com`. The proxy only ever speaks to this host over
//!   `https`; it is never attacker-controlled. Must be non-empty.
//! - `allow` (array, required, non-empty): the method/path scoping for this
//!   integration. Each entry is `{ "method": "POST", "path": "/v1/messages" }`.
//!   `method` is required; `path` is an optional exact match (omit it to permit
//!   any path for that method). These compile into the shared policy as
//!   `permit`-`HttpRequest` rules, so a request whose method/path matches no
//!   entry is denied (default-deny => 403). An empty `allow` is rejected at
//!   startup so an integration can never be configured wide-open by omission.
//! - `credential` (object, required): how the server-held credential is injected
//!   (`kind: "static_header"` or `kind: "aws_sigv4"`, see below).
//!
//! ## Worked example 1 — Anthropic (static header from an SSM SecureString)
//!
//! Inject `x-api-key: <secret>` where the secret is read once at startup from an
//! SSM SecureString, and scope the integration to the Messages API only:
//!
//! ```json
//! {
//!   "anthropic": {
//!     "host": "api.anthropic.com",
//!     "allow": [
//!       { "method": "POST", "path": "/v1/messages" },
//!       { "method": "GET",  "path": "/v1/models" }
//!     ],
//!     "credential": {
//!       "kind": "static_header",
//!       "header": "x-api-key",
//!       "ssm_param": "/devbox/egress/anthropic-key"
//!     }
//!   }
//! }
//! ```
//!
//! The box calls `POST /egress/anthropic/v1/messages`; the control plane forwards
//! it to `https://api.anthropic.com/v1/messages` with `x-api-key` injected. A
//! `Bearer`-style header is expressed with the optional `prefix`, e.g.
//! `{ "kind": "static_header", "header": "authorization", "prefix": "Bearer ",
//! "ssm_param": "..." }`.
//!
//! ## Worked example 2 — Bedrock (AWS SigV4)
//!
//! Sign requests with the control plane task role's own AWS credentials (no
//! stored secret), scoped to model invocation:
//!
//! ```json
//! {
//!   "bedrock": {
//!     "host": "bedrock-runtime.us-east-1.amazonaws.com",
//!     "allow": [
//!       { "method": "POST", "path": "/model/anthropic.claude-3-sonnet/invoke" }
//!     ],
//!     "credential": {
//!       "kind": "aws_sigv4",
//!       "service": "bedrock",
//!       "region": "us-east-1"
//!     }
//!   }
//! }
//! ```
//!
//! The box calls `POST /egress/bedrock/model/.../invoke`; the control plane signs
//! the outbound request with SigV4 for `service`/`region` and forwards it.
//!
//! # Why a reverse proxy (not MITM)
//!
//! The CONNECT proxy tunnels opaque TLS, so it cannot inspect the method/path or
//! inject credentials. MITMing it would require a trusted CA on every box (a
//! large AMI/infra change). Instead a box *opts* a flow through this reverse
//! proxy: the secret lives only on the control plane, is injected here, and is
//! never shipped to the box.
//!
//! # Subject identity
//!
//! Unlike the CONNECT hop (which has no per-connection box identity and uses a
//! coarse constant subject), this proxy authenticates the box per request and
//! uses its real instance id as the policy subject, so policy can discriminate
//! per box.
//!
//! # The credential-never-reaches-the-box guarantee
//!
//! Static-header secrets are read on the control plane from an SSM SecureString
//! via the task role at startup (mirroring [`crate::github::GitHubApp`]), wrapped
//! in [`secrecy::SecretString`], and never logged (they are not in any `Debug`
//! output and never serialized). SigV4 providers sign with the task role's own
//! credentials; no long-lived secret is stored for them. A client-supplied
//! `Authorization` header is **always** stripped before the server injects its
//! own credential, so a box can never smuggle or override the credential. When
//! SSM/AWS is unconfigured the whole proxy is absent ([`EgressProxy::from_env`]
//! returns `Ok(None)` for the proxy), so local/dev boots without AWS.
//!
//! # Validation (fail-closed at startup)
//!
//! [`EgressProxy::from_env`] rejects, at startup, before the listener binds: a
//! malformed `EGRESS_UPSTREAMS` JSON; an integration with an empty `host`; an
//! empty `allow` list; an invalid HTTP header name; an unknown credential `kind`
//! (serde rejects it); and a configured-but-unreadable SSM secret (a hard error
//! rather than a silently broken upstream).
//!
//! # Rollout ordering (devbox-infra)
//!
//! A box choosing to route a flow through `/egress/*` instead of the CONNECT
//! tunnel is a **box-facing** change: it is subject to the devbox-infra agent
//! contract (release the agent -> rebuild the AMI -> land infra). The server env
//! (`EGRESS_UPSTREAMS`) and any referenced SSM parameters must be provisioned in
//! devbox-infra **separately**; this crate only reads them. devbox-infra is not
//! edited here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use aws_config::SdkConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign as sigv4_sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use devbox_common::env_non_empty;

use crate::policy::{PolicyRule, normalize_host};
// Re-exported so the routes.rs call site `egress_proxy::into_axum_response` keeps
// resolving while the implementation lives in the shared proxy module.
pub(crate) use crate::proxy::into_axum_response;
use crate::proxy::{self, is_forwardable_request_header};

/// Env var holding the upstream registry as JSON (see [`UpstreamSpec`]).
const UPSTREAMS_ENV: &str = "EGRESS_UPSTREAMS";

/// This reverse proxy forwards no additional request headers beyond the shared
/// base safelist (unlike the git proxy, which also keeps `git-protocol`).
const EGRESS_EXTRA_REQUEST_HEADERS: &[&str] = &[];

/// Bound on the one-time SSM reads at startup so a stalled `ssm:GetParameter`
/// cannot block server start-up (this runs in `main` before the listener binds).
const KEY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How an upstream's credential is injected.
///
/// Secrets are held only here on the control plane and never sent to the box.
enum Credential {
    /// Inject a fixed header carrying a secret (e.g. `Authorization: Bearer <s>`
    /// or a custom `X-Api-Key: <s>`). The secret is wrapped so it is never logged.
    StaticHeader {
        header_name: HeaderName,
        /// The full header value to inject (already prefixed, e.g. `Bearer abc`).
        value: SecretString,
    },
    /// Sign the outbound request with the task role's credentials for `service`
    /// in `region` (AWS SigV4). No stored secret — the signer uses the ambient
    /// role credentials resolved from the environment at request time.
    AwsSigV4 {
        service: String,
        region: String,
        credentials: SharedCredentialsProvider,
    },
}

/// A boxed credentials provider shared across requests (the task role chain).
type SharedCredentialsProvider = Arc<dyn ProvideCredentials>;

/// A configured upstream: the single host requests are forwarded to and the
/// credential injected on the way out.
struct Upstream {
    /// The upstream host (e.g. `api.example.com`). Requests are built as
    /// `https://{host}/{rest}` — the proxy only ever speaks to this host.
    host: String,
    credential: Credential,
}

/// The upstream registry plus the HTTP client used to forward requests. Held in
/// [`crate::routes::AppState`] as an `Option<Arc<EgressProxy>>`, built in `main`
/// (absent when unconfigured, like the git proxy).
pub struct EgressProxy {
    client: reqwest::Client,
    upstreams: HashMap<String, Upstream>,
}

/// The result of configuring the integrations layer from the environment: the
/// transport-owning [`EgressProxy`] plus the per-integration `method`/`path`
/// scoping rules to **compile into the shared policy** at startup.
///
/// Keeping these together at the config seam is what guarantees a single
/// authorization path: the proxy forwards to an integration's host, and that
/// same host — run through the one [`crate::policy::normalize_host`] both sides
/// use — feeds the `permit` rules handed to [`crate::policy::EgressPolicy`], so
/// the policy and the transport cannot disagree about which host an integration
/// maps to.
pub struct EgressConfig {
    /// The configured proxy (transport + credential injection).
    pub proxy: Arc<EgressProxy>,
    /// `permit`-`HttpRequest` rules, one per `(integration host, allow entry)`,
    /// to merge into the shared [`crate::policy::EgressPolicy`].
    policy_rules: Vec<PolicyRule>,
}

impl EgressConfig {
    /// Take ownership of the compiled policy rules for merging into the shared
    /// policy (see [`crate::policy::EgressPolicy::extend_rules`]).
    pub fn policy_rules(&mut self) -> Vec<PolicyRule> {
        std::mem::take(&mut self.policy_rules)
    }
}

/// JSON shape of one entry in `EGRESS_UPSTREAMS` (a JSON object mapping an
/// integration name to this spec). See the module docs for the full schema and
/// two worked examples.
///
/// ```json
/// {
///   "anthropic": { "host": "api.anthropic.com",
///                  "allow": [ { "method": "POST", "path": "/v1/messages" } ],
///                  "credential": { "kind": "static_header",
///                                  "header": "x-api-key",
///                                  "ssm_param": "/devbox/egress/anthropic-key" } },
///   "bedrock":   { "host": "bedrock-runtime.us-east-1.amazonaws.com",
///                  "allow": [ { "method": "POST", "path": "/model/x/invoke" } ],
///                  "credential": { "kind": "aws_sigv4",
///                                  "service": "bedrock", "region": "us-east-1" } }
/// }
/// ```
#[derive(Debug, Deserialize)]
struct UpstreamSpec {
    /// The allowed upstream host.
    host: String,
    /// The method/path scoping for this integration, compiled into the shared
    /// policy as `permit`-`HttpRequest` rules. Must be non-empty.
    allow: Vec<RouteRuleSpec>,
    /// How to inject the credential.
    credential: CredentialSpec,
}

/// JSON shape of one `allow` entry: the method and optional exact path an
/// integration permits. `method` is required; a `None` `path` permits any path
/// for that method.
#[derive(Debug, Deserialize)]
struct RouteRuleSpec {
    /// The HTTP method this entry permits (e.g. `"POST"`).
    method: String,
    /// The exact request path this entry permits (e.g. `"/v1/messages"`). When
    /// omitted, any path is permitted for `method`.
    #[serde(default)]
    path: Option<String>,
}

/// JSON shape of an integration's credential config. Tagged by `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CredentialSpec {
    /// Inject a static header whose value is read from an SSM SecureString. The
    /// optional `prefix` is prepended to the secret (e.g. `"Bearer "`), so an
    /// `Authorization: Bearer <secret>` is expressed as
    /// `{ "header": "authorization", "prefix": "Bearer ", "ssm_param": "..." }`.
    StaticHeader {
        header: String,
        #[serde(default)]
        prefix: String,
        ssm_param: String,
    },
    /// Sign with AWS SigV4 using the task role's credentials.
    #[serde(rename = "aws_sigv4")]
    AwsSigV4 { service: String, region: String },
}

impl UpstreamSpec {
    /// Validate the non-credential parts of the spec and compile its `allow`
    /// entries into `permit`-`HttpRequest` [`PolicyRule`]s for `name`'s host.
    ///
    /// # Errors
    ///
    /// Returns an error when `host` is empty/whitespace or when `allow` is empty
    /// (an integration must be explicitly scoped — it is never wide-open by
    /// omission).
    fn compile_rules(&self, name: &str) -> Result<Vec<PolicyRule>> {
        if self.host.trim().is_empty() {
            anyhow::bail!("egress integration '{name}' has an empty host");
        }
        if self.allow.is_empty() {
            anyhow::bail!(
                "egress integration '{name}' has an empty 'allow' list; an integration must \
                 declare at least one permitted method/path (default-deny)"
            );
        }
        // Normalize the host the SAME way `decide_http` normalizes the request
        // host (and the same way the forwarded `Upstream.host` is stored), so the
        // policy rule host and the transport's forward target are guaranteed
        // identical and a host containing uppercase cannot silently default-deny.
        let host = normalize_host(&self.host);
        let rules = self
            .allow
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let id = format!("egress_{name}_{i}");
                PolicyRule::permit_http(&host, &entry.method, entry.path.clone(), id)
            })
            .collect();
        Ok(rules)
    }
}

impl EgressProxy {
    /// Build the proxy from `EGRESS_UPSTREAMS`, or `Ok(None)` when it is unset so
    /// local/dev boots without AWS.
    ///
    /// Static-header secrets are read once here from SSM via the task role; the
    /// `aws_config` is the already-loaded SDK config (region + task-role
    /// credential chain). SigV4 upstreams capture the same credential provider for
    /// per-request signing.
    ///
    /// # Errors
    ///
    /// Returns an error when `EGRESS_UPSTREAMS` is set but is not valid JSON, when
    /// an integration has an empty host or an empty `allow` list, when it names an
    /// invalid header or unknown credential kind, or when a static-header SSM read
    /// fails or times out (a configured-but-unreadable secret is a hard startup
    /// error rather than a silently broken upstream).
    pub async fn from_env(aws_config: &SdkConfig) -> Result<Option<EgressConfig>> {
        let Some(raw) = env_non_empty(UPSTREAMS_ENV) else {
            return Ok(None);
        };
        let specs: HashMap<String, UpstreamSpec> = serde_json::from_str(&raw)
            .with_context(|| format!("parse {UPSTREAMS_ENV} as a JSON integration map"))?;
        if specs.is_empty() {
            return Ok(None);
        }

        let credentials: SharedCredentialsProvider = match aws_config.credentials_provider() {
            Some(provider) => Arc::new(provider),
            None => Arc::new(StaticNoCredentials),
        };

        let mut upstreams = HashMap::new();
        let mut policy_rules = Vec::new();
        for (name, spec) in specs {
            // Validate + compile the method/path scoping BEFORE touching AWS, so a
            // misconfigured integration fails fast without an SSM round-trip.
            let rules = spec
                .compile_rules(&name)
                .with_context(|| format!("configure egress integration '{name}'"))?;
            let credential = build_credential(aws_config, &spec.credential, &credentials)
                .await
                .with_context(|| format!("configure egress integration '{name}'"))?;
            policy_rules.extend(rules);
            upstreams.insert(
                name,
                Upstream {
                    // Normalize identically to the compiled policy rule host (see
                    // `compile_rules`) so the host the proxy forwards to and the
                    // host `decide_http` authorizes are guaranteed equal.
                    host: normalize_host(&spec.host),
                    credential,
                },
            );
        }

        let client = proxy::build_client("devbox-server-egress-proxy")?;

        Ok(Some(EgressConfig {
            proxy: Arc::new(Self { client, upstreams }),
            policy_rules,
        }))
    }

    /// The host for a configured upstream, or `None` when unknown. Used by the
    /// route handler to feed the policy before any forwarding decision.
    pub(crate) fn host_for(&self, upstream: &str) -> Option<&str> {
        self.upstreams.get(upstream).map(|u| u.host.as_str())
    }

    /// Test-only constructor: a proxy with a single static-header upstream
    /// (`name` -> `host`), needing no AWS. Lets route-handler tests exercise the
    /// policy gate (which runs before any forwarding) against a *known* upstream
    /// so the 404-unknown path does not short-circuit the 403-deny path.
    #[cfg(test)]
    pub(crate) fn for_test(name: &str, host: &str) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let mut upstreams = HashMap::new();
        upstreams.insert(
            name.to_string(),
            Upstream {
                host: host.to_string(),
                credential: Credential::StaticHeader {
                    header_name: HeaderName::from_static("x-api-key"),
                    value: SecretString::from("test-secret".to_string()),
                },
            },
        );
        Arc::new(Self { client, upstreams })
    }

    /// Forward an already-authorized request to `upstream`, injecting the
    /// configured credential and streaming the response back.
    ///
    /// `rest` is the path tail after `/egress/{upstream}/`; `query` is the raw
    /// query string. The caller must have resolved the upstream (via
    /// [`EgressProxy::host_for`], which is the single 404 gate) and applied the
    /// policy first; this method performs the credential injection and transport
    /// only. A `None` lookup here is therefore an internal invariant violation
    /// (the handler resolved it moments earlier), surfaced as an `anyhow` error
    /// rather than a second, drift-prone 404 path.
    ///
    /// # Errors
    ///
    /// Returns an error when `upstream` is not configured (unreachable in the
    /// wired path), or when signing or the upstream transport fails.
    pub(crate) async fn forward(
        &self,
        upstream: &str,
        method: &Method,
        rest: &str,
        query: Option<&str>,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<reqwest::Response> {
        let up = self
            .upstreams
            .get(upstream)
            .with_context(|| format!("egress upstream '{upstream}' resolved then vanished"))?;
        let url = build_upstream_url(&up.host, rest, query);

        // Collect the forwardable client headers (Authorization always dropped).
        let mut forward_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        for (name, value) in headers {
            if is_forwardable_request_header(name, EGRESS_EXTRA_REQUEST_HEADERS) {
                forward_headers.push((name.clone(), value.clone()));
            }
        }

        // Resolve any AWS credentials asynchronously here (off the signing path),
        // then compute the injected headers synchronously.
        let resolved = match &up.credential {
            Credential::AwsSigV4 { credentials, .. } => Some(
                credentials
                    .provide_credentials()
                    .await
                    .context("resolve AWS credentials for SigV4 signing")?,
            ),
            Credential::StaticHeader { .. } => None,
        };
        let injected = credential_headers(
            &up.credential,
            resolved,
            method,
            &url,
            &body,
            &forward_headers,
        )
        .context("compute injected credential headers")?;

        let mut builder = self.client.request(method.clone(), &url);
        for (name, value) in &forward_headers {
            builder = builder.header(name.clone(), value.clone());
        }
        for (name, value) in injected {
            builder = builder.header(name, value);
        }
        if !body.is_empty() {
            builder = builder.body(body);
        }

        let resp = builder
            .send()
            .await
            .with_context(|| format!("forward egress request to {url}"))?;
        Ok(resp)
    }
}

/// A no-op credentials provider used when the SDK resolved no credential chain
/// (local/dev without AWS). SigV4 signing then fails cleanly at request time
/// rather than at construction, keeping static-header upstreams usable.
#[derive(Debug)]
struct StaticNoCredentials;

impl ProvideCredentials for StaticNoCredentials {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::ready(Err(
            aws_credential_types::provider::error::CredentialsError::not_loaded(
                "no AWS credentials configured for egress SigV4 signing",
            ),
        ))
    }
}

/// Build the runtime [`Credential`] for a spec, reading any SSM secret now.
async fn build_credential(
    aws_config: &SdkConfig,
    spec: &CredentialSpec,
    credentials: &SharedCredentialsProvider,
) -> Result<Credential> {
    match spec {
        CredentialSpec::StaticHeader {
            header,
            prefix,
            ssm_param,
        } => {
            let header_name = HeaderName::try_from(header.as_str())
                .with_context(|| format!("invalid header name '{header}'"))?;
            let secret =
                tokio::time::timeout(KEY_READ_TIMEOUT, read_ssm_secret(aws_config, ssm_param))
                    .await
                    .with_context(|| format!("SSM read of {ssm_param} timed out"))??;
            let value = SecretString::from(format!("{prefix}{secret}"));
            Ok(Credential::StaticHeader { header_name, value })
        }
        CredentialSpec::AwsSigV4 { service, region } => Ok(Credential::AwsSigV4 {
            service: service.clone(),
            region: region.clone(),
            credentials: Arc::clone(credentials),
        }),
    }
}

/// Read a secret from an SSM SecureString parameter, mirroring
/// [`crate::github::GitHubApp`]'s key read.
async fn read_ssm_secret(aws_config: &SdkConfig, param: &str) -> Result<String> {
    let ssm = aws_sdk_ssm::Client::new(aws_config);
    let out = ssm
        .get_parameter()
        .name(param)
        .with_decryption(true)
        .send()
        .await
        .with_context(|| format!("ssm:GetParameter {param}"))?;
    out.parameter()
        .and_then(aws_sdk_ssm::types::Parameter::value)
        .map(str::to_string)
        .with_context(|| format!("SSM parameter {param} has no value"))
}

/// Compute the credential headers to inject for a request, given the credential,
/// method, fully-built URL, body, and the already-forwardable client headers
/// (SigV4 signs over the headers it is given).
///
/// # Errors
///
/// Returns an error when a static header value is not a valid header value, or
/// when SigV4 signing fails (including when no AWS credentials are available).
fn credential_headers(
    credential: &Credential,
    resolved: Option<aws_credential_types::Credentials>,
    method: &Method,
    url: &str,
    body: &[u8],
    forward_headers: &[(HeaderName, HeaderValue)],
) -> Result<Vec<(HeaderName, HeaderValue)>> {
    match credential {
        Credential::StaticHeader { header_name, value } => {
            let header_value = HeaderValue::from_str(value.expose_secret())
                .context("static credential is not a valid header value")?;
            Ok(vec![(header_name.clone(), header_value)])
        }
        Credential::AwsSigV4 {
            service, region, ..
        } => {
            let creds = resolved.context("no AWS credentials resolved for SigV4 signing")?;
            sigv4_headers(service, region, creds, method, url, body, forward_headers)
        }
    }
}

/// Produce the AWS SigV4 signing headers (`Authorization`, `x-amz-date`,
/// `x-amz-content-sha256`, …) for a request, signing with already-resolved
/// `creds` (resolved by the async caller off this synchronous path).
///
/// # Errors
///
/// Returns an error when the signing params cannot be built, when a signed header
/// name/value is invalid, or when signing fails.
fn sigv4_headers(
    service: &str,
    region: &str,
    creds: aws_credential_types::Credentials,
    method: &Method,
    url: &str,
    body: &[u8],
    forward_headers: &[(HeaderName, HeaderValue)],
) -> Result<Vec<(HeaderName, HeaderValue)>> {
    let identity = Identity::from(creds);

    let signing_settings = SigningSettings::default();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .context("build SigV4 signing params")?
        .into();

    // SigV4 signs over a subset of headers; pass the forwardable client headers so
    // the canonical request matches what we actually send upstream.
    let header_pairs: Vec<(&str, &str)> = forward_headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str(), v)))
        .collect();
    let signable = SignableRequest::new(
        method.as_str(),
        url,
        header_pairs.iter().copied(),
        SignableBody::Bytes(body),
    )
    .context("build SigV4 signable request")?;

    let (instructions, _sig) = sigv4_sign(signable, &signing_params)
        .context("compute SigV4 signature")?
        .into_parts();

    let mut out = Vec::new();
    for (name, value) in instructions.headers() {
        let header_name =
            HeaderName::try_from(name).with_context(|| format!("signed header '{name}'"))?;
        let header_value = HeaderValue::from_str(value)
            .with_context(|| format!("signed header value for '{name}'"))?;
        out.push((header_name, header_value));
    }
    Ok(out)
}

/// Build the upstream URL for a request: `https://{host}/{rest}` with the raw
/// query re-attached. `rest` is used verbatim (already a URL path tail from the
/// router), with a leading slash ensured and the host trimmed of any slashes.
fn build_upstream_url(host: &str, rest: &str, query: Option<&str>) -> String {
    let host = host.trim_matches('/');
    let rest = rest.trim_start_matches('/');
    let base = format!("https://{host}/{rest}");
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{base}?{q}"),
        None => base,
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};

    use super::*;

    fn static_upstream() -> Upstream {
        Upstream {
            host: "api.example.com".to_string(),
            credential: Credential::StaticHeader {
                header_name: HeaderName::from_static("x-api-key"),
                value: SecretString::from("super-secret-key".to_string()),
            },
        }
    }

    fn proxy_with(upstreams: Vec<(&str, Upstream)>) -> EgressProxy {
        let client = reqwest::Client::builder().build().unwrap();
        let mut map = HashMap::new();
        for (name, up) in upstreams {
            map.insert(name.to_string(), up);
        }
        EgressProxy {
            client,
            upstreams: map,
        }
    }

    #[test]
    fn host_for_resolves_known_and_unknown() {
        let proxy = proxy_with(vec![("example", static_upstream())]);
        assert_eq!(proxy.host_for("example"), Some("api.example.com"));
        assert_eq!(proxy.host_for("missing"), None);
    }

    #[test]
    fn upstream_url_is_built_under_the_configured_host() {
        assert_eq!(
            build_upstream_url("api.example.com", "v1/data", Some("q=1")),
            "https://api.example.com/v1/data?q=1"
        );
        // Leading/trailing slashes are normalized; empty query is dropped.
        assert_eq!(
            build_upstream_url("api.example.com/", "/v1/data", Some("")),
            "https://api.example.com/v1/data"
        );
        assert_eq!(
            build_upstream_url("api.example.com", "", None),
            "https://api.example.com/"
        );
    }

    #[test]
    fn request_header_forwarding_drops_authorization() {
        // Authorization must never be forwarded — the proxy injects its own. This
        // exercises the shared safelist through egress's (empty) extra list.
        assert!(!is_forwardable_request_header(
            &AUTHORIZATION,
            EGRESS_EXTRA_REQUEST_HEADERS
        ));
        assert!(is_forwardable_request_header(
            &CONTENT_TYPE,
            EGRESS_EXTRA_REQUEST_HEADERS
        ));
        assert!(is_forwardable_request_header(
            &ACCEPT,
            EGRESS_EXTRA_REQUEST_HEADERS
        ));
    }

    #[test]
    fn static_header_provider_injects_configured_header() {
        let credential = Credential::StaticHeader {
            header_name: HeaderName::from_static("authorization"),
            value: SecretString::from("Bearer abc123".to_string()),
        };
        let injected = credential_headers(
            &credential,
            None,
            &Method::GET,
            "https://api.example.com/v1",
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(injected.len(), 1);
        let (name, value) = injected.first().unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer abc123");
    }

    #[test]
    fn static_header_provider_injects_custom_api_key_header() {
        let injected = credential_headers(
            &static_upstream().credential,
            None,
            &Method::POST,
            "https://api.example.com/v1/data",
            b"{}",
            &[],
        )
        .unwrap();
        let (name, value) = injected.first().unwrap();
        assert_eq!(name.as_str(), "x-api-key");
        assert_eq!(value.to_str().unwrap(), "super-secret-key");
    }

    #[test]
    fn sigv4_helper_produces_authorization_signature() {
        // Sign a sample request in isolation with fixed test credentials and
        // assert an AWS SigV4 Authorization header is produced.
        let creds = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        );
        let headers = sigv4_headers(
            "bedrock",
            "us-east-1",
            creds,
            &Method::GET,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            &[],
            &[],
        )
        .unwrap();
        let auth = headers
            .iter()
            .find(|(n, _)| n.as_str() == "authorization")
            .unwrap();
        let value = auth.1.to_str().unwrap();
        assert!(
            value.starts_with("AWS4-HMAC-SHA256 "),
            "unexpected signature scheme: {value}"
        );
        assert!(value.contains("Credential=AKIDEXAMPLE/"));
        assert!(value.contains("/us-east-1/bedrock/aws4_request"));
        // The signing instructions also carry the amz date header.
        assert!(headers.iter().any(|(n, _)| n.as_str() == "x-amz-date"));
    }

    #[test]
    fn sigv4_signs_over_the_forwarded_header_set() {
        // Regression guard for a signed-vs-sent header mismatch: sign with a
        // NON-EMPTY forward-header set (exactly the headers `forward` collects and
        // then hands to reqwest) and assert each forwarded header name appears in
        // the SigV4 `SignedHeaders` list. If the signed set ever diverged from the
        // headers actually sent, the upstream would reject with
        // SignatureDoesNotMatch; pinning the SignedHeaders here makes that visible.
        let creds = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        );
        let forward_headers = vec![
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (
                USER_AGENT,
                HeaderValue::from_static("devbox-server-egress-proxy"),
            ),
            (ACCEPT, HeaderValue::from_static("application/json")),
        ];
        // AWS SigV4's default settings deliberately EXCLUDE a few headers from the
        // signature (notably `user-agent`, `x-amzn-trace-id`, `authorization`),
        // because clients and intermediaries routinely rewrite them — reqwest
        // sets its own `user-agent`, so signing over it would guarantee a
        // SignatureDoesNotMatch. So `user-agent` is forwarded but NOT signed,
        // which is exactly what the upstream expects; `content-type` and `accept`
        // ARE signed and must match what is sent.
        let signable_names = ["content-type", "accept"];
        let headers = sigv4_headers(
            "bedrock",
            "us-east-1",
            creds,
            &Method::POST,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            b"{\"prompt\":\"hi\"}",
            &forward_headers,
        )
        .unwrap();
        let auth = headers
            .iter()
            .find(|(n, _)| n.as_str() == "authorization")
            .map(|(_, v)| v.to_str().unwrap().to_string())
            .unwrap();
        // The Authorization header carries a `SignedHeaders=a;b;c,` segment that
        // lists every signed header (lowercased, semicolon-joined, sorted).
        let signed_segment = auth
            .split("SignedHeaders=")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_default();
        assert!(
            signed_segment.contains("host"),
            "host must always be signed: {auth}"
        );
        for name in signable_names {
            assert!(
                signed_segment.contains(name),
                "forwarded signable header '{name}' must be in SignedHeaders ({signed_segment})"
            );
        }
        // user-agent is forwarded on the wire but intentionally NOT signed (see
        // the SigV4 default-exclusion note above), so it must be absent from the
        // signed set — proving the signed set matches AWS's expectation rather
        // than naively over-signing every forwarded header.
        assert!(
            !signed_segment.contains("user-agent"),
            "user-agent must NOT be in the signed set (SigV4 excludes it): {signed_segment}"
        );
    }

    #[test]
    fn sigv4_without_resolved_credentials_fails_cleanly() {
        // When the async resolution produced no credentials (local/dev without
        // AWS), the sync header computation must error rather than sign unsigned.
        let credential = Credential::AwsSigV4 {
            service: "bedrock".to_string(),
            region: "us-east-1".to_string(),
            credentials: Arc::new(StaticNoCredentials),
        };
        let err = credential_headers(
            &credential,
            None,
            &Method::GET,
            "https://bedrock-runtime.us-east-1.amazonaws.com/",
            &[],
            &[],
        );
        assert!(err.is_err(), "missing credentials must fail signing");
    }

    #[tokio::test]
    async fn static_no_credentials_provider_resolves_to_error() {
        // The placeholder provider used when AWS is unconfigured never yields a
        // credential, so an attempted SigV4 forward fails at resolution.
        let provider: SharedCredentialsProvider = Arc::new(StaticNoCredentials);
        assert!(provider.provide_credentials().await.is_err());
    }

    #[tokio::test]
    async fn forward_errors_on_unknown_upstream() {
        // The handler resolves the upstream via `host_for` first (the single 404
        // gate), so reaching `forward` with an unknown name is an internal
        // invariant violation, surfaced as an error rather than a second 404.
        let proxy = proxy_with(vec![("example", static_upstream())]);
        let out = proxy
            .forward(
                "nope",
                &Method::GET,
                "v1/data",
                None,
                &HeaderMap::new(),
                Bytes::new(),
            )
            .await;
        assert!(out.is_err(), "unknown upstream must error");
    }

    #[test]
    fn upstream_spec_parses_both_credential_kinds() {
        let raw = r#"{
            "anthropic": { "host": "api.anthropic.com",
                           "allow": [ { "method": "POST", "path": "/v1/messages" },
                                      { "method": "GET", "path": "/v1/models" } ],
                           "credential": { "kind": "static_header", "header": "x-api-key",
                                           "ssm_param": "/devbox/egress/anthropic" } },
            "bedrock":   { "host": "bedrock-runtime.us-east-1.amazonaws.com",
                           "allow": [ { "method": "POST", "path": "/model/x/invoke" } ],
                           "credential": { "kind": "aws_sigv4", "service": "bedrock",
                                           "region": "us-east-1" } }
        }"#;
        let specs: HashMap<String, UpstreamSpec> = serde_json::from_str(raw).unwrap();
        assert_eq!(specs.len(), 2);

        let anthropic = specs
            .iter()
            .find(|(k, _)| *k == "anthropic")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(anthropic.allow.len(), 2);
        assert!(
            matches!(
                &anthropic.credential,
                CredentialSpec::StaticHeader { header, ssm_param, .. }
                    if header == "x-api-key" && ssm_param == "/devbox/egress/anthropic"
            ),
            "anthropic must parse as a static_header: {:?}",
            anthropic.credential
        );

        let bedrock = specs
            .iter()
            .find(|(k, _)| *k == "bedrock")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(bedrock.allow.len(), 1);
        assert!(
            matches!(
                &bedrock.credential,
                CredentialSpec::AwsSigV4 { service, region }
                    if service == "bedrock" && region == "us-east-1"
            ),
            "bedrock must parse as aws_sigv4: {:?}",
            bedrock.credential
        );
    }

    #[test]
    fn unknown_credential_kind_is_rejected() {
        // An unknown credential kind must not parse — serde's tagged enum rejects
        // it, so a typo'd/invalid kind is a hard startup error, not a silently
        // credential-less upstream.
        let raw = r#"{
            "x": { "host": "api.example.com",
                   "allow": [ { "method": "GET" } ],
                   "credential": { "kind": "telepathy" } }
        }"#;
        let parsed: Result<HashMap<String, UpstreamSpec>, _> = serde_json::from_str(raw);
        assert!(parsed.is_err(), "unknown credential kind must not parse");
    }

    #[test]
    fn compile_rules_rejects_empty_host() {
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{ "host": "   ", "allow": [ { "method": "GET" } ],
                 "credential": { "kind": "aws_sigv4", "service": "s", "region": "r" } }"#,
        )
        .unwrap();
        assert!(
            spec.compile_rules("bad").is_err(),
            "an empty host must be rejected at startup"
        );
    }

    #[test]
    fn compile_rules_rejects_empty_allow() {
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{ "host": "api.example.com", "allow": [],
                 "credential": { "kind": "aws_sigv4", "service": "s", "region": "r" } }"#,
        )
        .unwrap();
        assert!(
            spec.compile_rules("open").is_err(),
            "an empty allow list must be rejected (never wide-open by omission)"
        );
    }

    #[test]
    fn compile_rules_produces_permit_rules_enforced_by_the_policy() {
        // The compiled rules, merged into an EgressPolicy, must permit exactly the
        // declared method+path for the integration's host and default-deny the
        // rest — proving the inline EGRESS_UPSTREAMS scoping flows through the
        // single decide_http authorization path.
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{
                "host": "api.anthropic.com",
                "allow": [ { "method": "POST", "path": "/v1/messages" },
                           { "method": "GET" } ],
                "credential": { "kind": "static_header", "header": "x-api-key",
                                "ssm_param": "/devbox/egress/anthropic" }
            }"#,
        )
        .unwrap();
        let rules = spec.compile_rules("anthropic").unwrap();
        assert_eq!(rules.len(), 2);

        let mut policy = crate::policy::EgressPolicy::from_allowlist_csv("").unwrap();
        policy.extend_rules(rules);

        // Exact declared POST /v1/messages is permitted.
        assert!(
            policy
                .decide_http("i-1", "api.anthropic.com", "POST", "/v1/messages")
                .allowed
        );
        // A path-less GET entry permits any path for GET.
        assert!(
            policy
                .decide_http("i-1", "api.anthropic.com", "GET", "/v1/anything")
                .allowed
        );
        // A method not declared is denied (default-deny).
        assert!(
            !policy
                .decide_http("i-1", "api.anthropic.com", "DELETE", "/v1/messages")
                .allowed
        );
        // The right method but a non-declared exact path is denied.
        assert!(
            !policy
                .decide_http("i-1", "api.anthropic.com", "POST", "/v1/other")
                .allowed
        );
        // A different host is denied even for the declared method/path.
        assert!(
            !policy
                .decide_http("i-1", "evil.example.com", "POST", "/v1/messages")
                .allowed
        );
    }

    #[test]
    fn compile_rules_normalizes_an_uppercase_host_so_it_is_reachable() {
        // Regression (host-case asymmetry): an integration configured with an
        // uppercase host must be reachable. The compiled rule host is normalized
        // (lowercased) identically to the request host decide_http sees, so the
        // permitted method/path is allowed and others are denied — rather than
        // every request silently default-denying with a 403.
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{
                "host": "API.Example.com",
                "allow": [ { "method": "POST", "path": "/v1/messages" } ],
                "credential": { "kind": "aws_sigv4", "service": "s", "region": "r" }
            }"#,
        )
        .unwrap();
        let rules = spec.compile_rules("up").unwrap();
        let mut policy = crate::policy::EgressPolicy::from_allowlist_csv("").unwrap();
        policy.extend_rules(rules);

        // The lowercased request host (what the HTTP stack delivers) matches.
        let allow = policy.decide_http("i-1", "api.example.com", "POST", "/v1/messages");
        assert!(
            allow.allowed,
            "uppercase-host integration must be reachable: {allow:?}"
        );
        // Declared scoping still holds.
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "GET", "/v1/messages")
                .allowed
        );
    }

    #[test]
    fn compile_rules_host_that_is_subdomain_of_allowlist_matches_its_own_rule() {
        // Regression (folding asymmetry): with EGRESS_ALLOWLIST "example.com",
        // an integration whose host is "api.example.com" must still match its own
        // compiled HttpRequest rule for the permitted method/path (and be denied
        // for others) — folding must not rewrite the request host onto the
        // allowlisted parent before matching the child-host rule.
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{
                "host": "api.example.com",
                "allow": [ { "method": "POST", "path": "/v1/messages" } ],
                "credential": { "kind": "aws_sigv4", "service": "s", "region": "r" }
            }"#,
        )
        .unwrap();
        let rules = spec.compile_rules("up").unwrap();
        // Allowlist contains the PARENT of the integration host.
        let mut policy = crate::policy::EgressPolicy::from_allowlist_csv("example.com").unwrap();
        policy.extend_rules(rules);

        let allow = policy.decide_http("i-1", "api.example.com", "POST", "/v1/messages");
        assert!(
            allow.allowed,
            "subdomain-of-allowlist integration host must match its own rule: {allow:?}"
        );
        // A non-declared method/path on that host is still denied.
        assert!(
            !policy
                .decide_http("i-1", "api.example.com", "GET", "/v1/messages")
                .allowed
        );
    }

    #[test]
    fn stored_upstream_host_matches_the_normalized_policy_host() {
        // The forward target (`Upstream.host`) and the policy rule host must be
        // byte-for-byte identical so decide_http and forward can never diverge.
        // Both are produced by `normalize_host`, so an uppercase configured host
        // is stored (and thus forwarded to) lowercased, exactly as the compiled
        // rule host.
        let up = Upstream {
            host: normalize_host("API.Example.com"),
            credential: Credential::StaticHeader {
                header_name: HeaderName::from_static("x-api-key"),
                value: SecretString::from("k".to_string()),
            },
        };
        assert_eq!(up.host, "api.example.com");

        let spec: UpstreamSpec = serde_json::from_str(
            r#"{
                "host": "API.Example.com",
                "allow": [ { "method": "GET" } ],
                "credential": { "kind": "aws_sigv4", "service": "s", "region": "r" }
            }"#,
        )
        .unwrap();
        let rules = spec.compile_rules("up").unwrap();
        let mut policy = crate::policy::EgressPolicy::from_allowlist_csv("").unwrap();
        policy.extend_rules(rules);
        // The forward host (up.host) is exactly the host decide_http allows.
        assert!(
            policy
                .decide_http("i-1", &up.host, "GET", "/anything")
                .allowed
        );
    }
}
