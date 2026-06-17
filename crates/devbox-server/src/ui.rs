//! Web UI module using Askama templates and rust-embed for static assets.
//!
//! Templates are defined in the `templates/` directory and compiled into the
//! binary. Static assets are embedded via rust-embed and served with
//! appropriate cache headers.

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::routes::AppState;

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
    Router::new().route("/", get(dashboard))
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
