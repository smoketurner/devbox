//! Web UI module using Askama templates and rust-embed for static assets.
//!
//! Templates are defined in the `templates/` directory and compiled into the
//! binary. Static assets are embedded via rust-embed and served with
//! appropriate cache headers.

use askama::Template;
use axum::Form;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rust_embed::Embed;

use crate::db::document_type::Document;
use crate::documents::devbox::DevboxDoc;
use crate::routes::AppState;
use devbox_common::DevboxState;

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
// Handlers
// ============================================================================

/// Build the UI router.
pub fn build_ui_router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/devboxes/claim", get(claim_form).post(submit_claim))
        .route("/devboxes/{id}", get(devbox_detail))
        .route("/devboxes/{id}/release", post(submit_release))
        .route("/static/{*path}", get(static_asset))
}

/// Render the dashboard page.
///
/// GET /
async fn dashboard(State(state): State<AppState>) -> Response {
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
                error: None,
            }
            .into_response()
        }
        Err(e) => DashboardTemplate {
            devboxes: Vec::new(),
            error: Some(format!("Failed to load devboxes: {e}")),
        }
        .into_response(),
    }
}

/// Render the devbox detail page.
///
/// GET /devboxes/{id}
async fn devbox_detail(State(state): State<AppState>, Path(id): Path<String>) -> Response {
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
async fn claim_form() -> Response {
    ClaimFormTemplate {
        instance_type: None,
        error: None,
    }
    .into_response()
}

/// Resolve the submitter's owner: the ALB-OIDC principal when auth is enabled,
/// otherwise the form-supplied owner. `Err` carries a user-facing message.
async fn ui_owner(
    state: &AppState,
    headers: &HeaderMap,
    form_owner: &str,
) -> Result<String, String> {
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
    if let Some(auth) = &state.auth {
        let owner = doc.data.owner.clone().unwrap_or_default();
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
