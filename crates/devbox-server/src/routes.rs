//! HTTP route handlers.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, HealthResponse, ReleaseRequest,
};

use crate::db::DocumentStore;
use crate::ui::build_ui_router;

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<DocumentStore>,
}

/// Build the Axum router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/v1/devboxes", get(list_devboxes))
        .route("/api/v1/devboxes/{id}", get(get_devbox))
        .route("/api/v1/devboxes/claim", post(claim_devbox))
        .route("/api/v1/devboxes/{id}/release", post(release_devbox))
        .merge(build_ui_router())
        .with_state(state)
}

/// Health check endpoint.
async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let db_status = match state.store.pool().is_healthy().await {
        Ok(()) => "healthy".to_string(),
        Err(e) => format!("unhealthy: {e}"),
    };

    Json(HealthResponse {
        status: "ok".to_string(),
        database: db_status,
    })
}

/// List all devboxes.
async fn list_devboxes(
    State(_state): State<AppState>,
) -> Result<Json<DevboxListResponse>, StatusCode> {
    // Placeholder: return empty list
    Ok(Json(DevboxListResponse {
        devboxes: Vec::new(),
    }))
}

/// Get a single devbox by ID.
async fn get_devbox(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> Result<Json<DevboxResponse>, StatusCode> {
    // Placeholder: return not found
    Err(StatusCode::NOT_FOUND)
}

/// Claim an available devbox.
async fn claim_devbox(
    State(_state): State<AppState>,
    Json(_req): Json<ClaimRequest>,
) -> Result<Json<DevboxResponse>, StatusCode> {
    // Placeholder: return not found (no available devboxes)
    Err(StatusCode::NOT_FOUND)
}

/// Release a claimed devbox.
async fn release_devbox(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Json(_req): Json<ReleaseRequest>,
) -> Result<Json<DevboxResponse>, StatusCode> {
    // Placeholder: return not found
    Err(StatusCode::NOT_FOUND)
}
