//! Device-code OAuth login chain (RFC 8628 + RFC 7591 DCR + RFC 9728 discovery).
//!
//! The 5-step flow:
//!
//! 1. `GET {server}/.well-known/oauth-protected-resource` — discover the
//!    authorization server and scopes (RFC 9728). A 404 means the server does
//!    not expose OAuth discovery (wrong `--server`, or an out-of-date server).
//! 2. `GET {issuer}/.well-known/openid-configuration` — discover the OIDC
//!    endpoints (device_authorization, token, registration).
//! 3. `POST {registration_endpoint}` — anonymous DCR (RFC 7591). Cached in
//!    `~/.config/devbox/client.json`; re-used across logins.
//! 4. `POST {device_authorization_endpoint}` — request a device code and print
//!    the `user_code` / `verification_uri` for the user.
//! 5. Poll `{token_endpoint}` until the user approves or the code expires.
//!
//! The server is a pure OAuth *resource server*; all OAuth interactions (steps
//! 2–5) are between the CLI and Vouch directly.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::session::{self, Client, Session};

// ============================================================================
// Public surface
// ============================================================================

/// Run the full 5-step login chain.
///
/// # Errors
///
/// Returns an error for network failures, malformed responses, user denial, or
/// a server that does not expose the RFC 9728 discovery endpoint (404).
pub(crate) async fn login(client: &reqwest::Client, server: &str) -> Result<Session> {
    // Step 1 — RFC 9728 discovery.
    let prm = fetch_protected_resource(client, server).await?;

    let issuer = prm
        .authorization_servers
        .first()
        .context(
            "the server's oauth-protected-resource response has no authorization_servers entry",
        )?
        .clone();

    // Step 2 — OIDC discovery.
    let disco = fetch_oidc_config(client, &issuer).await?;

    // Step 3 — load or register a DCR client.
    let client_id = match session::load_client(&issuer)? {
        Some(c) => c.client_id,
        None => {
            let id = register_client(client, &disco).await?;
            session::save_client(&Client {
                issuer: issuer.clone(),
                client_id: id.clone(),
            })?;
            id
        }
    };

    // Scope: use what the server advertises; fall back to the minimum we need
    // if the PRM returned an empty list (critic item 7).
    let scope = if prm.scopes_supported.is_empty() {
        "openid email".to_string()
    } else {
        prm.scopes_supported.join(" ")
    };

    // Steps 4 + 5, with one bounded retry on invalid_client. On rejection the
    // re-register step (forget → register → cache the new client_id) is injected
    // so it stays out of the pure device-flow loop and never touches disk in
    // tests.
    let token = device_flow_with_retry(client, &disco, &client_id, &scope, async || {
        session::forget_client()?;
        let new_id = register_client(client, &disco).await?;
        session::save_client(&Client {
            issuer: issuer.clone(),
            client_id: new_id.clone(),
        })?;
        Ok(new_id)
    })
    .await?;

    let session = Session::from_token(token)?;
    // A freshly minted token that is already expired means a badly skewed clock;
    // fail loudly rather than write a session that `current()` immediately treats
    // as logged-out (so "logged in as ..." can never contradict the next command).
    if session.is_expired() {
        bail!(
            "the authorization server returned an already-expired token; \
             check this machine's clock and try again"
        );
    }
    session::save_session(server, &session)?;
    Ok(session)
}

// ============================================================================
// Poll-loop state machine
// ============================================================================

/// Device-flow poll outcome — only the cases the outer `device_flow_with_retry`
/// loop must act on. `Pending` and `SlowDown` are handled inside
/// `poll_for_token`'s own loop and never surfaced to the caller.
#[derive(Debug)]
enum PollOutcome {
    /// User approved; contains the bearer `access_token`.
    Approved(String),
    /// User denied.
    Denied,
    /// Code expired before approval.
    Expired,
    /// The client_id was rejected (triggers one re-register retry).
    InvalidClient,
    /// Unrecognized error code from the authorization server.
    Server(String),
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Deserialize)]
pub(crate) struct ProtectedResourceMeta {
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    /// The control plane's AWS account (a vendor extension to the RFC 9728
    /// document). `devbox ssh` reads it to auto-select the local AWS profile for
    /// the SSM tunnel; absent when the server has no `AWS_ACCOUNT_ID` configured.
    #[serde(default)]
    pub(crate) aws_account_id: Option<String>,
}

#[derive(Deserialize)]
struct OidcDiscovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

// ============================================================================
// Step implementations
// ============================================================================

/// Fetch and parse the RFC 9728 discovery document. Public to the crate so the
/// `ssh` path can read its `aws_account_id` extension; a 404 is a hard error
/// (the login flow needs it) that `ssh` treats as best-effort and ignores.
pub(crate) async fn fetch_protected_resource(
    client: &reqwest::Client,
    server: &str,
) -> Result<ProtectedResourceMeta> {
    let url = format!("{server}/.well-known/oauth-protected-resource");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "{server} does not expose OAuth discovery at \
             /.well-known/oauth-protected-resource (is --server correct, and the \
             server up to date?)"
        );
    }

    if !resp.status().is_success() {
        bail!("GET {url} returned {}", resp.status());
    }

    resp.json::<ProtectedResourceMeta>()
        .await
        .with_context(|| format!("parse oauth-protected-resource from {url}"))
}

async fn fetch_oidc_config(client: &reqwest::Client, issuer: &str) -> Result<OidcDiscovery> {
    // OIDC Discovery 1.0 §4.1: strip any terminating slash before appending the
    // well-known path, else the URL gains a double slash and can 404.
    let issuer = issuer.trim_end_matches('/');
    let url = format!("{issuer}/.well-known/openid-configuration");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetch OIDC discovery {url}"))?;

    if !resp.status().is_success() {
        bail!("GET {url} returned {}", resp.status());
    }

    resp.json::<OidcDiscovery>()
        .await
        .with_context(|| format!("parse OIDC discovery from {url}"))
}

/// RFC 7591 anonymous Dynamic Client Registration.
///
/// Anonymous (no auth header) because Vouch's registration endpoint is
/// intentionally open: a `client_id` alone grants no access — the device-code
/// grant still requires the user to authenticate with FIDO2.
async fn register_client(client: &reqwest::Client, disco: &OidcDiscovery) -> Result<String> {
    let url = &disco.registration_endpoint;
    let body = serde_json::json!({
        "client_name": "devbox-cli",
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code"],
        "token_endpoint_auth_method": "none",
        "scope": "openid email"
    });

    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("DCR POST {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("DCR POST {url} returned {status}: {text}");
    }

    let reg: RegistrationResponse = resp
        .json()
        .await
        .with_context(|| format!("parse DCR response from {url}"))?;

    Ok(reg.client_id)
}

async fn request_device_code(
    client: &reqwest::Client,
    disco: &OidcDiscovery,
    client_id: &str,
    scope: &str,
) -> Result<DeviceAuthResponse> {
    let url = &disco.device_authorization_endpoint;
    let resp = client
        .post(url)
        .form(&[("client_id", client_id), ("scope", scope)])
        .send()
        .await
        .with_context(|| format!("device authorization POST {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("device authorization POST {url} returned {status}: {text}");
    }

    resp.json::<DeviceAuthResponse>()
        .await
        .with_context(|| format!("parse device authorization response from {url}"))
}

/// Print the user prompt (to stderr so stdout stays scriptable) and, in an
/// interactive terminal, open the verification page in the user's browser.
fn print_user_prompt(device: &DeviceAuthResponse) {
    eprintln!();
    eprintln!("Open the following URL and enter the code to authorize devbox:");
    eprintln!();
    eprintln!("  URL:  {}", device.verification_uri);
    eprintln!("  Code: {}", device.user_code);
    if let Some(ref complete) = device.verification_uri_complete {
        eprintln!();
        eprintln!("  Or visit this one-step link:");
        eprintln!("  {complete}");
    }
    eprintln!();

    // Best-effort browser launch, only when stderr is a TTY (skipped in scripts,
    // CI, and headless hosts). The `!cfg!(test)` guard suppresses it under
    // `cargo test`, which inherits the terminal's stderr and would otherwise
    // open a real browser. Prefer the one-step link — it carries the code so
    // Vouch can pre-fill it — and fall back to the plain page. Any failure is
    // silent: the URL and code are already printed above.
    if !cfg!(test) && std::io::stderr().is_terminal() {
        let target = device
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&device.verification_uri);
        if open::that_detached(target).is_ok() {
            eprintln!("Opening your browser...");
            eprintln!();
        }
    }

    eprintln!("Approving the code runs Vouch SSO + your FIDO2/YubiKey.");
    eprintln!();
}

/// Poll the token endpoint until the code is approved, denied, or expires.
///
/// RFC 8628 §3.5 semantics:
/// - Sleep *before* the first poll (not after).
/// - `slow_down` → add 5 s to the interval (saturating).
/// - `authorization_pending` → continue.
/// - Transport/HTTP errors → hard-abort (a dead network won't recover in time).
/// - Non-200 with no known `error` field → hard-abort (critic item 3).
async fn poll_for_token(
    http: &reqwest::Client,
    disco: &OidcDiscovery,
    client_id: &str,
    device: &DeviceAuthResponse,
) -> Result<PollOutcome> {
    let url = &disco.token_endpoint;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(device.expires_in))
        .context("device expires_in overflow")?;
    // Clamp to 1 s minimum: RFC 8628 §3.5 says "wait at least interval seconds";
    // an explicit `"interval": 0` from the server must not cause a request flood.
    let mut interval = device.interval.max(1);

    loop {
        if Instant::now() >= deadline {
            return Ok(PollOutcome::Expired);
        }

        // RFC 8628 §3.5: sleep BEFORE polling.
        tokio::time::sleep(Duration::from_secs(interval)).await;

        // Re-check after sleeping.
        if Instant::now() >= deadline {
            return Ok(PollOutcome::Expired);
        }

        // Read status first, THEN body — never call error_for_status before
        // reading: device-flow errors arrive as a JSON body on 4xx responses
        // (critic item 3 / RFC 8628 §3.5).
        let resp = http
            .post(url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await
            .with_context(|| format!("token endpoint POST {url}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("parse token endpoint response from {url}"))?;

        if status.is_success() {
            let access_token = body
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .context(
                    "the token endpoint returned no access_token for the device grant; \
                     the device-code grant must return a bearer access token carrying \
                     an 'email' claim",
                )?;
            return Ok(PollOutcome::Approved(access_token.to_string()));
        }

        // Non-2xx: read the `error` field (critic item 3 — must not panic or
        // loop on a missing field).
        let error_code = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        match error_code {
            "authorization_pending" => continue,
            "slow_down" => {
                interval = interval.saturating_add(5);
                continue;
            }
            "access_denied" => return Ok(PollOutcome::Denied),
            "expired_token" => return Ok(PollOutcome::Expired),
            "invalid_client" => return Ok(PollOutcome::InvalidClient),
            other => {
                // A non-200 with no recognizable error must terminate, not loop
                // (critic item 3).
                let code = if other.is_empty() {
                    format!("unexpected {status} response from token endpoint")
                } else {
                    other.to_string()
                };
                return Ok(PollOutcome::Server(code));
            }
        }
    }
}

/// Run steps 4+5 with a single bounded retry on `invalid_client`.
///
/// On `invalid_client`: forget the cached client_id, re-register (step 3),
/// re-request a device code (step 4), and reprint the `user_code` — a second
/// `invalid_client` is a hard error (critic item 4).
async fn device_flow_with_retry(
    http: &reqwest::Client,
    disco: &OidcDiscovery,
    initial_client_id: &str,
    scope: &str,
    mut reregister: impl AsyncFnMut() -> Result<String>,
) -> Result<String> {
    // Use a bounded loop (max 2 iterations) to prevent an infinite re-register
    // cycle (critic item 4).
    let mut client_id = initial_client_id.to_string();
    let mut already_reregistered = false;

    loop {
        let device = request_device_code(http, disco, &client_id, scope).await?;
        print_user_prompt(&device);

        match poll_for_token(http, disco, &client_id, &device).await? {
            PollOutcome::Approved(token) => return Ok(token),

            PollOutcome::InvalidClient => {
                if already_reregistered {
                    bail!(
                        "the authorization server rejected the re-registered client; \
                         check the server configuration"
                    );
                }
                already_reregistered = true;
                eprintln!("The registered client was rejected; re-registering...");
                client_id = reregister().await?;
                // Loop: request a NEW device_code with the new client_id.
            }

            PollOutcome::Denied => bail!("device authorization was denied"),
            PollOutcome::Expired => {
                bail!("the login code expired before approval; run `devbox login` again")
            }
            PollOutcome::Server(code) => {
                bail!("authorization server error: {code}")
            }
        }
    }
}

// Make the scope-fallback logic testable without going through HTTP.
#[cfg(test)]
fn scope_for_prm(scopes_supported: &[String]) -> String {
    if scopes_supported.is_empty() {
        "openid email".to_string()
    } else {
        scopes_supported.join(" ")
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::response::IntoResponse;
    use axum::routing::{any, post};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Bind an ephemeral port, spawn the router in the background, and return
    /// the base URL (`http://127.0.0.1:<port>`).
    async fn serve(router: axum::Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// Build a minimal `OidcDiscovery` pointing all three endpoints at `base`.
    fn disco(base: &str) -> OidcDiscovery {
        OidcDiscovery {
            device_authorization_endpoint: format!("{base}/device"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: format!("{base}/register"),
        }
    }

    /// Build a minimal `DeviceAuthResponse` with a very short expiry so tests
    /// don't hang if a loop runs one extra iteration.
    fn dev(base: &str, interval: u64, expires_in: u64) -> DeviceAuthResponse {
        DeviceAuthResponse {
            device_code: "dc1".to_string(),
            user_code: "USER-CODE".to_string(),
            verification_uri: format!("{base}/activate"),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::new()
    }

    // -----------------------------------------------------------------------
    // Test 1: authorization_pending → continue, eventually returns access_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_pending_then_approved() {
        // First call returns authorization_pending; second returns success.
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        let router = axum::Router::new().route(
            "/token",
            post(move || {
                let c = count2.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({"error": "authorization_pending"})),
                        )
                            .into_response()
                    } else {
                        Json(json!({"access_token": "tok.abc.def"})).into_response()
                    }
                }
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        // interval = 0 → floored to 1 by poll_for_token, but we want fast tests.
        // Use a fresh DeviceAuthResponse with interval=0 (floored) and a short expiry.
        let device = dev(&base, 0, 30);

        let outcome = poll_for_token(&http(), &d, "client1", &device)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PollOutcome::Approved(ref t) if t == "tok.abc.def"),
            "expected Approved"
        );
        assert_eq!(count.load(Ordering::SeqCst), 2, "should have polled twice");
    }

    // -----------------------------------------------------------------------
    // Test 2: slow_down → interval +5, continue, eventually returns access_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_slow_down_then_approved() {
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        let router = axum::Router::new().route(
            "/token",
            post(move || {
                let c = count2.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({"error": "slow_down"})),
                        )
                            .into_response()
                    } else {
                        Json(json!({"access_token": "tok.slow.ok"})).into_response()
                    }
                }
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        let device = dev(&base, 0, 30);

        let outcome = poll_for_token(&http(), &d, "client1", &device)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PollOutcome::Approved(ref t) if t == "tok.slow.ok"),
            "expected Approved after slow_down"
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    // -----------------------------------------------------------------------
    // Test 3: access_denied → returns Denied
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_access_denied() {
        let router = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error": "access_denied"})),
                )
                    .into_response()
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        let device = dev(&base, 0, 30);

        let outcome = poll_for_token(&http(), &d, "client1", &device)
            .await
            .unwrap();
        assert!(matches!(outcome, PollOutcome::Denied), "expected Denied");
    }

    // -----------------------------------------------------------------------
    // Test 4: expired_token → returns Expired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_expired_token() {
        let router = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error": "expired_token"})),
                )
                    .into_response()
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        let device = dev(&base, 0, 30);

        let outcome = poll_for_token(&http(), &d, "client1", &device)
            .await
            .unwrap();
        assert!(matches!(outcome, PollOutcome::Expired), "expected Expired");
    }

    // -----------------------------------------------------------------------
    // Test 5: invalid_client → bounded retry once, bail on second
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_invalid_client_twice_returns_error() {
        let router = axum::Router::new()
            // Both poll calls return invalid_client.
            .route(
                "/token",
                post(|| async {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid_client"})),
                    )
                        .into_response()
                }),
            )
            // /device supplies a fresh code each loop; re-registration is injected,
            // so no /register route is needed.
            .route(
                "/device",
                post(|| async {
                    Json(json!({
                        "device_code": "dc2",
                        "user_code": "AAAA-BBBB",
                        "verification_uri": "http://example.com/activate",
                        "expires_in": 30,
                        "interval": 0
                    }))
                    .into_response()
                }),
            );

        let base = serve(router).await;
        let d = disco(&base);

        // Two invalid_client responses → device_flow_with_retry should bail on the
        // second. The re-register step is injected (returns a canned id) so the
        // test never touches the real ~/.config/devbox/client.json.
        let reregisters = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&reregisters);
        let result =
            device_flow_with_retry(&http(), &d, "old-client", "openid email", async move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok("new-client".to_string())
            })
            .await;
        assert!(result.is_err(), "expected error on second invalid_client");
        assert_eq!(
            reregisters.load(Ordering::SeqCst),
            1,
            "re-register must run exactly once (bounded retry)"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("rejected")
                || msg.contains("invalid_client")
                || msg.contains("authorization server"),
            "error should mention rejection, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: non-200, no `error` field → Server abort (not panic, not loop)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_non_200_no_error_field_aborts() {
        let router = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({})),
                )
                    .into_response()
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        let device = dev(&base, 0, 30);

        let outcome = poll_for_token(&http(), &d, "client1", &device)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PollOutcome::Server(_)),
            "expected Server outcome, not panic or loop"
        );
        if let PollOutcome::Server(ref msg) = outcome {
            assert!(
                !msg.is_empty(),
                "Server outcome should contain a description"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 7: empty scopes_supported → falls back to "openid email"
    // -----------------------------------------------------------------------

    #[test]
    fn scope_fallback_when_empty() {
        let result = scope_for_prm(&[]);
        assert_eq!(result, "openid email");
    }

    #[test]
    fn scope_uses_server_list_when_present() {
        let scopes = [
            "openid".to_string(),
            "email".to_string(),
            "profile".to_string(),
        ];
        let result = scope_for_prm(&scopes);
        assert_eq!(result, "openid email profile");
    }

    // -----------------------------------------------------------------------
    // Test 8: missing access_token on 200 → clear error mentioning "access_token"
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_missing_access_token_returns_error() {
        let router = axum::Router::new().route(
            "/token",
            post(|| async {
                // 200 OK but body has no access_token field.
                Json(json!({"token_type": "Bearer", "expires_in": 3600})).into_response()
            }),
        );

        let base = serve(router).await;
        let d = disco(&base);
        let device = dev(&base, 0, 30);

        let result = poll_for_token(&http(), &d, "client1", &device).await;
        assert!(
            result.is_err(),
            "expected error when access_token is missing"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("access_token"),
            "error should mention access_token, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 9: PRM 404 → hard error (server does not expose OAuth discovery)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn login_prm_404_is_error() {
        // Serve a minimal devbox "server" that returns 404 on the PRM endpoint.
        let router = axum::Router::new().route(
            "/.well-known/oauth-protected-resource",
            any(|| async { axum::http::StatusCode::NOT_FOUND.into_response() }),
        );

        let base = serve(router).await;
        let client = http();

        let result = login(&client, &base).await;
        assert!(result.is_err(), "PRM 404 must be a hard error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("OAuth discovery"),
            "error should explain the missing discovery endpoint, got: {msg}"
        );
    }
}
