//! Web UI module using Askama templates and rust-embed for static assets.
//!
//! Templates are defined in the `templates/` directory and compiled into the
//! binary. Static assets are embedded via rust-embed and served with
//! appropriate cache headers.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

use crate::routes::AppState;

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
}

impl_template_into_response!(DashboardTemplate);

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
// Handlers
// ============================================================================

/// Build the UI router.
pub fn build_ui_router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/static/{*path}", get(static_asset))
}

/// Render the dashboard page.
///
/// GET /
async fn dashboard(State(_state): State<AppState>) -> Response {
    let template = DashboardTemplate {
        devboxes: Vec::new(),
    };
    template.into_response()
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
