//! Credential-injecting reverse proxy for arbitrary configured upstreams.
//!
//! This generalizes the GitHub git reverse proxy ([`crate::github::git_proxy`])
//! beyond the git case. A box points a specific flow at the control plane
//! (`/egress/{upstream}/{*rest}`), authenticates with its web-identity token
//! exactly as the git proxy does (so the server resolves a verified
//! [`crate::auth::AgentIdentity`]), and the control plane:
//!
//! 1. applies the shared Dogwood [`crate::policy::EgressPolicy`] on `host` +
//!    `method` + `path` ([`crate::policy::EgressPolicy::decide_http`]) —
//!    default-deny, same engine/event-history instance as the CONNECT proxy
//!    ([`crate::egress`]);
//! 2. on allow, forwards the request upstream with a **server-held** credential
//!    injected (a static header/bearer or an AWS SigV4 signature), after
//!    stripping any client-supplied `Authorization`;
//! 3. streams the upstream response back; and
//! 4. emits a Dogwood decision record for every allow and deny (the same JSONL
//!    record type and sink as the CONNECT proxy, [`crate::egress::DecisionRecord`]
//!    / [`crate::egress::DecisionSink`]).
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
//! # Secrets
//!
//! Static-header secrets are read on the control plane from an SSM SecureString
//! via the task role at startup (mirroring [`crate::github::GitHubApp`]), wrapped
//! in [`secrecy::SecretString`], and never logged. When SSM/AWS is unconfigured
//! the whole proxy is absent ([`EgressProxy::from_env`] returns `Ok(None)`), so
//! local/dev boots without AWS. SigV4 providers sign with the task role's own
//! credentials; no long-lived secret is stored for them.
//!
//! # Rollout ordering (devbox-infra)
//!
//! A box choosing to route a flow through `/egress/*` instead of the CONNECT
//! tunnel is a **box-facing** change: it is subject to the devbox-infra agent
//! contract (release the agent -> rebuild the AMI -> land infra). The new server
//! env (`EGRESS_UPSTREAMS`) and any referenced SSM parameters must be provisioned
//! in devbox-infra **separately**; this crate only reads them. devbox-infra is
//! not edited here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use aws_config::SdkConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign as sigv4_sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, USER_AGENT,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use devbox_common::env_non_empty;

/// Env var holding the upstream registry as JSON (see [`UpstreamSpec`]).
const UPSTREAMS_ENV: &str = "EGRESS_UPSTREAMS";

/// Upstream connect timeout. As with the git proxy there is no total timeout, so
/// a long streamed response is not cut off mid-flight.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the one-time SSM reads at startup so a stalled `ssm:GetParameter`
/// cannot block server start-up (this runs in `main` before the listener binds).
const KEY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a proxied request was refused before forwarding, mapped to a status by the
/// caller. Resolution/policy errors are modeled here so the thin route handler
/// stays a pure adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxyReject {
    /// The named upstream is not configured → 404.
    UnknownUpstream(String),
}

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

/// JSON shape of one entry in `EGRESS_UPSTREAMS` (a JSON object mapping an
/// upstream name to this spec).
///
/// ```json
/// {
///   "anthropic": { "host": "api.anthropic.com",
///                  "credential": { "kind": "static_header",
///                                  "header": "x-api-key",
///                                  "ssm_param": "/devbox/egress/anthropic-key" } },
///   "bedrock":   { "host": "bedrock-runtime.us-east-1.amazonaws.com",
///                  "credential": { "kind": "aws_sigv4",
///                                  "service": "bedrock", "region": "us-east-1" } }
/// }
/// ```
#[derive(Debug, Deserialize)]
struct UpstreamSpec {
    /// The allowed upstream host.
    host: String,
    /// How to inject the credential.
    credential: CredentialSpec,
}

/// JSON shape of an upstream's credential config. Tagged by `kind`.
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
    /// an upstream names an invalid header, or when a static-header SSM read fails
    /// or times out (a configured-but-unreadable secret is a hard startup error
    /// rather than a silently broken upstream).
    pub async fn from_env(aws_config: &SdkConfig) -> Result<Option<Arc<Self>>> {
        let Some(raw) = env_non_empty(UPSTREAMS_ENV) else {
            return Ok(None);
        };
        let specs: HashMap<String, UpstreamSpec> = serde_json::from_str(&raw)
            .with_context(|| format!("parse {UPSTREAMS_ENV} as a JSON upstream map"))?;
        if specs.is_empty() {
            return Ok(None);
        }

        let credentials: SharedCredentialsProvider = match aws_config.credentials_provider() {
            Some(provider) => Arc::new(provider),
            None => Arc::new(StaticNoCredentials),
        };

        let mut upstreams = HashMap::new();
        for (name, spec) in specs {
            let credential = build_credential(aws_config, &spec.credential, &credentials)
                .await
                .with_context(|| format!("configure egress upstream '{name}'"))?;
            upstreams.insert(
                name,
                Upstream {
                    host: spec.host,
                    credential,
                },
            );
        }

        let client = reqwest::Client::builder()
            .user_agent("devbox-server-egress-proxy")
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("build egress-proxy HTTP client")?;

        Ok(Some(Arc::new(Self { client, upstreams })))
    }

    /// The host for a configured upstream, or `None` when unknown. Used by the
    /// route handler to feed the policy before any forwarding decision.
    pub(crate) fn host_for(&self, upstream: &str) -> Option<&str> {
        self.upstreams.get(upstream).map(|u| u.host.as_str())
    }

    /// Forward an already-authorized request to `upstream`, injecting the
    /// configured credential and streaming the response back.
    ///
    /// `rest` is the path tail after `/egress/{upstream}/`; `query` is the raw
    /// query string. The caller must have applied the policy first; this method
    /// performs the credential injection and transport only.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyReject::UnknownUpstream`] when `upstream` is not configured.
    /// Propagates signing and upstream transport failures as `anyhow` errors.
    pub(crate) async fn forward(
        &self,
        upstream: &str,
        method: &Method,
        rest: &str,
        query: Option<&str>,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<std::result::Result<reqwest::Response, ProxyReject>> {
        let Some(up) = self.upstreams.get(upstream) else {
            return Ok(Err(ProxyReject::UnknownUpstream(upstream.to_string())));
        };
        let url = build_upstream_url(&up.host, rest, query);

        // Collect the forwardable client headers (Authorization always dropped).
        let mut forward_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        for (name, value) in headers {
            if is_forwardable_request_header(name) {
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
        Ok(Ok(resp))
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

/// Whether an incoming request header is forwarded upstream. Mirrors the git
/// proxy's safelist and, critically, **always drops `Authorization`** (the proxy
/// injects its own credential) and length/hop-by-hop headers so reqwest re-frames.
fn is_forwardable_request_header(name: &HeaderName) -> bool {
    name == ACCEPT
        || name == ACCEPT_ENCODING
        || name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == USER_AGENT
}

/// Whether an upstream response header is forwarded back. Keeps content framing
/// and cache directives; drops length/transfer headers so axum re-frames.
fn is_forwardable_response_header(name: &HeaderName) -> bool {
    name == CONTENT_TYPE
        || name == CONTENT_ENCODING
        || name == CACHE_CONTROL
        || name.as_str() == "expires"
        || name.as_str() == "pragma"
}

/// Convert an upstream response into a streaming axum response: the status, the
/// safelisted headers, and the body streamed (never buffered). Mirrors the git
/// proxy's helper.
pub(crate) fn into_axum_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_forwardable_response_header(name) {
            builder = builder.header(name.clone(), value.clone());
        }
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use axum::http::header::AUTHORIZATION;

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
        // Authorization must never be forwarded — the proxy injects its own.
        assert!(!is_forwardable_request_header(&AUTHORIZATION));
        assert!(is_forwardable_request_header(&CONTENT_TYPE));
        assert!(is_forwardable_request_header(&ACCEPT));
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
    async fn forward_rejects_unknown_upstream() {
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
            .await
            .unwrap();
        assert!(matches!(
            out,
            Err(ProxyReject::UnknownUpstream(name)) if name == "nope"
        ));
    }

    #[test]
    fn upstream_spec_parses_both_credential_kinds() {
        let raw = r#"{
            "anthropic": { "host": "api.anthropic.com",
                           "credential": { "kind": "static_header", "header": "x-api-key",
                                           "ssm_param": "/devbox/egress/anthropic" } },
            "bedrock":   { "host": "bedrock-runtime.us-east-1.amazonaws.com",
                           "credential": { "kind": "aws_sigv4", "service": "bedrock",
                                           "region": "us-east-1" } }
        }"#;
        let specs: HashMap<String, UpstreamSpec> = serde_json::from_str(raw).unwrap();
        assert_eq!(specs.len(), 2);

        let anthropic = specs.get("anthropic").map(|s| &s.credential);
        assert!(
            matches!(
                anthropic,
                Some(CredentialSpec::StaticHeader { header, ssm_param, .. })
                    if header == "x-api-key" && ssm_param == "/devbox/egress/anthropic"
            ),
            "anthropic must parse as a static_header: {anthropic:?}"
        );

        let bedrock = specs.get("bedrock").map(|s| &s.credential);
        assert!(
            matches!(
                bedrock,
                Some(CredentialSpec::AwsSigV4 { service, region })
                    if service == "bedrock" && region == "us-east-1"
            ),
            "bedrock must parse as aws_sigv4: {bedrock:?}"
        );
    }
}
