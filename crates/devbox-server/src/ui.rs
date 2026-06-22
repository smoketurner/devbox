//! Web UI module using Askama templates and rust-embed for static assets.
//!
//! Templates are defined in the `templates/` directory and compiled into the
//! binary. Static assets are embedded via rust-embed and served with
//! appropriate cache headers.

use askama::Template;
use axum::Form;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rust_embed::Embed;

use crate::auth::random_token;
use crate::db::document_type::Document;
use crate::documents::devbox::DevboxDoc;
use crate::routes::AppState;
use devbox_common::DevboxState;

/// Cookie holding the Vouch OIDC ID token after a successful dashboard login.
const SESSION_COOKIE: &str = "devbox_session";
/// Short-lived cookie holding the OIDC `state` CSRF token between `/login` and
/// the callback.
const STATE_COOKIE: &str = "devbox_oidc_state";
/// Session cookie lifetime. The ID token's own `exp` is the authoritative gate
/// (re-verified per request); the browser drops the cookie at this bound too.
const SESSION_MAX_AGE_SECS: i64 = 28_800;
/// CSRF-state cookie lifetime (the login round-trip should take well under this).
const STATE_MAX_AGE_SECS: i64 = 600;

// ============================================================================
// Embedded Static Assets
// ============================================================================

/// Static assets embedded into the binary at compile time.
#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

// ============================================================================
// Template IntoResponse macro (following Vouch pattern)
// ============================================================================

/// Implement `IntoResponse` for Askama templates so handlers can return them
/// directly.
macro_rules! impl_template_into_response {
    ($($template:ty),* $(,)?) => {
        $(
            impl axum::response::IntoResponse for $template {
                fn into_response(self) -> axum::response::Response {
                    use askama::Template;
                    match self.render() {
                        Ok(html) => Html(html).into_response(),
                        Err(e) => {
                            tracing::error!("Template render error: {}", e);
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                }
            }
        )*
    };
}

// ============================================================================
// Templates
// ============================================================================

/// Dashboard template showing all devboxes.
#[derive(Template)]
#[template(path = "index.html")]
pub struct DashboardTemplate {
    pub devboxes: Vec<DashboardDevbox>,
    /// The signed-in principal, shown in the header. `None` when dashboard login
    /// is not enabled.
    pub principal: Option<String>,
    pub error: Option<String>,
}

/// Detail view for a single devbox.
pub struct DevboxDetail {
    pub id: String,
    pub state: String,
    pub instance_type: String,
    pub ami_id: String,
    pub subnet_id: String,
    pub instance_id: String,
    pub ebs_volume_id: String,
    pub owner: String,
    pub claimed_at: String,
    pub created_at: String,
}

impl From<Document<DevboxDoc>> for DevboxDetail {
    fn from(doc: Document<DevboxDoc>) -> Self {
        DevboxDetail {
            id: doc.id,
            state: doc.data.state.to_string(),
            instance_type: doc.data.instance_type.to_string(),
            ami_id: doc.data.ami_id.to_string(),
            subnet_id: doc.data.subnet_id.to_string(),
            instance_id: doc.data.instance_id.unwrap_or_default(),
            ebs_volume_id: doc.data.ebs_volume_id.unwrap_or_default(),
            owner: doc.data.owner.unwrap_or_default(),
            claimed_at: doc
                .data
                .claimed_at
                .map(|ts| ts.to_string())
                .unwrap_or_default(),
            created_at: doc.created_at.to_string(),
        }
    }
}

/// Detail page template.
#[derive(Template)]
#[template(path = "detail.html")]
pub struct DevboxDetailTemplate {
    pub devbox: DevboxDetail,
    pub error: Option<String>,
}

/// Error page template (404, etc.).
#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPageTemplate {
    pub title: String,
    pub message: String,
}

/// Claim form template.
#[derive(Template)]
#[template(path = "claim_form.html")]
pub struct ClaimFormTemplate {
    pub instance_type: Option<String>,
    pub error: Option<String>,
}

impl_template_into_response!(
    DashboardTemplate,
    DevboxDetailTemplate,
    ErrorPageTemplate,
    ClaimFormTemplate
);

/// A devbox entry for the dashboard template.
pub struct DashboardDevbox {
    pub id: String,
    pub state: String,
    pub instance_type: String,
    pub instance_id: String,
    pub owner: String,
    pub created_at: String,
}

// ============================================================================
// Form Data
// ============================================================================

/// Form data for claiming a devbox.
#[derive(serde::Deserialize)]
struct ClaimFormData {
    owner: String,
    instance_type: Option<String>,
}

// ============================================================================
// Session / OIDC dashboard login
// ============================================================================

/// Query parameters on the OIDC redirect callback.
#[derive(serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Whether app-side OIDC dashboard login is enabled.
fn dashboard_login_enabled(state: &AppState) -> bool {
    state
        .auth
        .as_ref()
        .is_some_and(|auth| auth.oidc().is_some())
}

/// Read a cookie value from the request `Cookie` header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// Build a hardened `Set-Cookie` value. `max_age` of 0 clears the cookie.
fn set_cookie(name: &str, value: &str, max_age: i64) -> String {
    format!("{name}={value}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age}")
}

/// Resolve the signed-in principal from the session cookie, if valid.
async fn current_session(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let auth = state.auth.as_ref()?;
    auth.oidc()?;
    let token = cookie_value(headers, SESSION_COOKIE)?;
    auth.verify_id_token(&token).await.ok().map(|p| p.0)
}

/// Gate a dashboard page on a valid session.
///
/// - `Ok(None)` — login is not enabled; the page is open.
/// - `Ok(Some(principal))` — a valid session is present.
/// - `Err(redirect)` — login is enabled but no valid session; redirect to `/login`.
async fn require_login(state: &AppState, headers: &HeaderMap) -> Result<Option<String>, Response> {
    if !dashboard_login_enabled(state) {
        return Ok(None);
    }
    match current_session(state, headers).await {
        Some(principal) => Ok(Some(principal)),
        None => Err(Redirect::to("/login").into_response()),
    }
}

/// Start the OIDC Authorization Code flow: set a CSRF `state` cookie and
/// redirect to the IdP.
///
/// GET /login
async fn login(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(auth) = state.auth.as_ref().filter(|a| a.oidc().is_some()) else {
        // Nothing to log into; already-valid sessions just go home.
        return Redirect::to("/").into_response();
    };
    if current_session(&state, &headers).await.is_some() {
        return Redirect::to("/").into_response();
    }
    let csrf = match random_token() {
        Ok(token) => token,
        Err(e) => {
            tracing::error!("failed to generate OIDC state: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(url) = auth.authorize_url(&csrf) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        AppendHeaders([(
            header::SET_COOKIE,
            set_cookie(STATE_COOKIE, &csrf, STATE_MAX_AGE_SECS),
        )]),
        Redirect::to(&url),
    )
        .into_response()
}

/// Complete the OIDC flow: verify `state`, exchange the code, and set the
/// session cookie.
///
/// GET /oauth2/idpresponse
async fn oauth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(auth) = state.auth.as_ref().filter(|a| a.oidc().is_some()) else {
        return Redirect::to("/").into_response();
    };

    // On any failure, redirect to a clean URL (no authorization code left in the
    // address bar, where it could leak via history/bookmarks/Referer) and clear
    // the single-use state cookie; the specific reason is logged server-side.
    let fail = |reason: &str| -> Response {
        tracing::warn!("dashboard sign-in failed: {reason}");
        (
            AppendHeaders([(header::SET_COOKIE, set_cookie(STATE_COOKIE, "", 0))]),
            Redirect::to("/login/error"),
        )
            .into_response()
    };

    if let Some(err) = query.error {
        return fail(&format!("identity provider returned an error: {err}"));
    }
    let (Some(code), Some(returned_state)) = (query.code, query.state) else {
        return fail("missing code or state on the callback");
    };
    // CSRF: the state echoed back must match the one we set at /login.
    match cookie_value(&headers, STATE_COOKIE) {
        Some(expected) if expected == returned_state => {}
        _ => return fail("invalid or missing login state"),
    }

    let id_token = match auth.exchange_code(&code).await {
        Ok(token) => token,
        Err(e) => return fail(&format!("token exchange failed: {e}")),
    };
    if let Err(e) = auth.verify_id_token(&id_token).await {
        return fail(&format!("id_token rejected: {e}"));
    }

    (
        AppendHeaders([
            (
                header::SET_COOKIE,
                set_cookie(SESSION_COOKIE, &id_token, SESSION_MAX_AGE_SECS),
            ),
            (header::SET_COOKIE, set_cookie(STATE_COOKIE, "", 0)),
        ]),
        Redirect::to("/"),
    )
        .into_response()
}

/// Render the sign-in error page. The callback redirects here on failure so the
/// one-time authorization code never lingers in the address bar; the page links
/// back to the dashboard, which restarts the flow.
///
/// GET /login/error
async fn login_error_page() -> Response {
    ErrorPageTemplate {
        title: "Sign-in failed".to_string(),
        message: "Sign-in did not complete. Please try again.".to_string(),
    }
    .into_response()
}

/// Clear the session cookie.
///
/// GET /logout
async fn logout() -> Response {
    (
        AppendHeaders([(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0))]),
        Redirect::to("/"),
    )
        .into_response()
}

// ============================================================================
// Handlers
// ============================================================================

/// Build the UI router.
pub fn build_ui_router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login))
        .route("/login/error", get(login_error_page))
        .route("/oauth2/idpresponse", get(oauth_callback))
        .route("/logout", get(logout))
        .route("/devboxes/claim", get(claim_form).post(submit_claim))
        .route("/devboxes/{id}", get(devbox_detail))
        .route("/devboxes/{id}/release", post(submit_release))
        .route("/static/{*path}", get(static_asset))
}

/// Render the dashboard page.
///
/// GET /
async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match require_login(&state, &headers).await {
        Ok(principal) => principal,
        Err(redirect) => return redirect,
    };
    match state.store.list_all::<DevboxDoc>().await {
        Ok(docs) => {
            let devboxes = docs
                .into_iter()
                .map(|doc| DashboardDevbox {
                    id: doc.id.clone(),
                    state: doc.data.state.to_string(),
                    instance_type: doc.data.instance_type.to_string(),
                    instance_id: doc.data.instance_id.clone().unwrap_or_default(),
                    owner: doc.data.owner.clone().unwrap_or_default(),
                    created_at: doc.created_at.to_string(),
                })
                .collect();
            DashboardTemplate {
                devboxes,
                principal,
                error: None,
            }
            .into_response()
        }
        Err(e) => DashboardTemplate {
            devboxes: Vec::new(),
            principal,
            error: Some(format!("Failed to load devboxes: {e}")),
        }
        .into_response(),
    }
}

/// Render the devbox detail page.
///
/// GET /devboxes/{id}
async fn devbox_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(redirect) = require_login(&state, &headers).await {
        return redirect;
    }
    match state.store.get::<DevboxDoc>(&id).await {
        Ok(Some(doc)) => DevboxDetailTemplate {
            devbox: doc.into(),
            error: None,
        }
        .into_response(),
        Ok(None) => ErrorPageTemplate {
            title: "Not Found".to_string(),
            message: format!("Devbox '{id}' not found."),
        }
        .into_response(),
        Err(e) => ErrorPageTemplate {
            title: "Error".to_string(),
            message: format!("Failed to load devbox: {e}"),
        }
        .into_response(),
    }
}

/// Render the claim form.
///
/// GET /devboxes/claim
async fn claim_form(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(redirect) = require_login(&state, &headers).await {
        return redirect;
    }
    ClaimFormTemplate {
        instance_type: None,
        error: None,
    }
    .into_response()
}

/// Resolve the submitter's owner: the dashboard-login session principal (or the
/// legacy ALB/bearer principal) when auth is enabled, otherwise the form-supplied
/// owner. `Err` carries a user-facing message.
async fn ui_owner(
    state: &AppState,
    headers: &HeaderMap,
    form_owner: &str,
) -> Result<String, String> {
    if dashboard_login_enabled(state) {
        return current_session(state, headers)
            .await
            .ok_or_else(|| "Your session has expired. Please sign in again.".to_string());
    }
    match &state.auth {
        Some(auth) => auth
            .authenticate(headers)
            .await
            .map(|principal| principal.0)
            .map_err(|e| format!("Authentication failed: {e}")),
        None => {
            if form_owner.trim().is_empty() {
                Err("Owner field is required.".to_string())
            } else {
                Ok(form_owner.to_string())
            }
        }
    }
}

/// Process the claim form submission.
///
/// POST /devboxes/claim
async fn submit_claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ClaimFormData>,
) -> Response {
    // Gate on a valid session when dashboard login is enabled — redirect to
    // /login (consistent with the GET claim form and submit_release) instead of
    // rendering the form with an error.
    if let Err(redirect) = require_login(&state, &headers).await {
        return redirect;
    }
    let owner = match ui_owner(&state, &headers, &form.owner).await {
        Ok(owner) => owner,
        Err(message) => {
            return ClaimFormTemplate {
                instance_type: form.instance_type,
                error: Some(message),
            }
            .into_response();
        }
    };

    let ready_docs = match state.store.find_all::<DevboxDoc>("state", "ready").await {
        Ok(docs) => docs,
        Err(e) => {
            return ClaimFormTemplate {
                instance_type: form.instance_type,
                error: Some(format!("Failed to query devboxes: {e}")),
            }
            .into_response();
        }
    };

    if ready_docs.is_empty() {
        return ClaimFormTemplate {
            instance_type: form.instance_type,
            error: Some("No devboxes available.".to_string()),
        }
        .into_response();
    }

    let mut candidates = ready_docs;
    if let Some(ref pref) = form.instance_type
        && !pref.is_empty()
    {
        candidates.sort_by(|a, b| {
            let a_match = a.data.instance_type.as_ref() == pref.as_str();
            let b_match = b.data.instance_type.as_ref() == pref.as_str();
            b_match.cmp(&a_match)
        });
    }

    for candidate in candidates {
        let mut updated = candidate.data.clone();
        updated.state = DevboxState::Claimed;
        updated.owner = Some(owner.clone());
        updated.claimed_at = Some(jiff::Timestamp::now());

        match state
            .store
            .compare_and_update(&candidate.id, candidate.version, &updated)
            .await
        {
            Ok(true) => {
                return Redirect::to(&format!("/devboxes/{}", candidate.id)).into_response();
            }
            Ok(false) => continue,
            Err(e) => {
                return ClaimFormTemplate {
                    instance_type: form.instance_type,
                    error: Some(format!("Claim failed: {e}")),
                }
                .into_response();
            }
        }
    }

    ClaimFormTemplate {
        instance_type: form.instance_type,
        error: Some("No devboxes available (all claimed concurrently).".to_string()),
    }
    .into_response()
}

/// Process the release form submission from the detail page.
///
/// POST /devboxes/{id}/release
async fn submit_release(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let doc = match state.store.get::<DevboxDoc>(&id).await {
        Ok(Some(doc)) => doc,
        Ok(None) => {
            return ErrorPageTemplate {
                title: "Not Found".to_string(),
                message: format!("Devbox '{id}' not found."),
            }
            .into_response();
        }
        Err(e) => {
            return ErrorPageTemplate {
                title: "Error".to_string(),
                message: format!("Failed to load devbox: {e}"),
            }
            .into_response();
        }
    };

    if doc.data.state != DevboxState::Claimed {
        return DevboxDetailTemplate {
            devbox: doc.into(),
            error: Some("Cannot release devbox in current state.".to_string()),
        }
        .into_response();
    }

    // When auth is enabled, only the claimant may release.
    let owner = doc.data.owner.clone().unwrap_or_default();
    if dashboard_login_enabled(&state) {
        match current_session(&state, &headers).await {
            Some(principal) if principal == owner => {}
            Some(_) => {
                return DevboxDetailTemplate {
                    devbox: doc.into(),
                    error: Some("You can only release a devbox you claimed.".to_string()),
                }
                .into_response();
            }
            None => return Redirect::to("/login").into_response(),
        }
    } else if let Some(auth) = &state.auth {
        match auth.authenticate(&headers).await {
            Ok(principal) if principal.0 == owner => {}
            Ok(_) => {
                return DevboxDetailTemplate {
                    devbox: doc.into(),
                    error: Some("You can only release a devbox you claimed.".to_string()),
                }
                .into_response();
            }
            Err(e) => {
                return DevboxDetailTemplate {
                    devbox: doc.into(),
                    error: Some(format!("Authentication failed: {e}")),
                }
                .into_response();
            }
        }
    }

    let mut updated = doc.data.clone();
    updated.state = DevboxState::Terminating;
    updated.owner = None;

    match state.store.update(&id, &updated).await {
        Ok(()) => Redirect::to(&format!("/devboxes/{id}")).into_response(),
        Err(e) => {
            // Re-fetch for template
            let refreshed = state.store.get::<DevboxDoc>(&id).await.ok().flatten();
            match refreshed {
                Some(refreshed_doc) => DevboxDetailTemplate {
                    devbox: refreshed_doc.into(),
                    error: Some(format!("Release failed: {e}")),
                }
                .into_response(),
                None => ErrorPageTemplate {
                    title: "Error".to_string(),
                    message: format!("Release failed: {e}"),
                }
                .into_response(),
            }
        }
    }
}

/// Serve embedded static assets.
///
/// GET /static/*path
async fn static_asset(Path(path): Path<String>) -> Response {
    match StaticAssets::get(&path) {
        Some(content) => {
            let mime = content.metadata.mimetype();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.to_string())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn cookie_value_extracts_named_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; devbox_session=tok.en.jwt; b=2"),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE).as_deref(),
            Some("tok.en.jwt")
        );
        assert_eq!(cookie_value(&headers, "absent"), None);
    }

    #[test]
    fn cookie_value_none_without_header() {
        assert_eq!(cookie_value(&HeaderMap::new(), SESSION_COOKIE), None);
    }

    #[test]
    fn set_cookie_is_hardened_and_clearable() {
        let set = set_cookie(SESSION_COOKIE, "value", SESSION_MAX_AGE_SECS);
        assert!(set.contains("HttpOnly"));
        assert!(set.contains("Secure"));
        assert!(set.contains("SameSite=Lax"));
        assert!(set.contains("Path=/"));
        assert!(set_cookie(SESSION_COOKIE, "", 0).contains("Max-Age=0"));
    }
}
