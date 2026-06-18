//! HTTP route handlers.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Json;
use axum::routing::{get, post};

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, HealthResponse,
    PoolMetricsResponse, ReleaseRequest,
};

use crate::db::DocumentStore;
use crate::documents::devbox::DevboxDoc;
use crate::error::{AppError, JsonBody};
use crate::reconcile::ReconcilerConfig;
use crate::ui::build_ui_router;

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<DocumentStore>,
    pub reconciler_config: Arc<ReconcilerConfig>,
}

/// Build the Axum router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/v1/devboxes", get(list_devboxes))
        .route("/api/v1/devboxes/{id}", get(get_devbox))
        .route("/api/v1/devboxes/claim", post(claim_devbox))
        .route("/api/v1/devboxes/{id}/release", post(release_devbox))
        .route("/api/v1/pool/metrics", get(pool_metrics))
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
    State(state): State<AppState>,
) -> Result<Json<DevboxListResponse>, AppError> {
    let docs = state.store.list_all::<DevboxDoc>().await?;
    let devboxes = docs.into_iter().map(DevboxResponse::from).collect();
    Ok(Json(DevboxListResponse { devboxes }))
}

/// Get a single devbox by ID.
async fn get_devbox(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DevboxResponse>, AppError> {
    let doc = state
        .store
        .get::<DevboxDoc>(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("devbox '{id}' not found")))?;
    Ok(Json(doc.into()))
}

/// Claim an available devbox.
async fn claim_devbox(
    State(state): State<AppState>,
    JsonBody(req): JsonBody<ClaimRequest>,
) -> Result<Json<DevboxResponse>, AppError> {
    if req.owner.trim().is_empty() || req.owner.len() > 256 {
        return Err(AppError::BadRequest(
            "owner must be non-empty and at most 256 characters".into(),
        ));
    }

    let ready_docs = state.store.find_all::<DevboxDoc>("state", "ready").await?;
    if ready_docs.is_empty() {
        return Err(AppError::Conflict("no devboxes available".into()));
    }

    // Sort candidates: prefer matching instance_type first, then by created_at
    // ascending (longest-waiting first).
    let mut candidates = ready_docs;
    candidates.sort_by(|a, b| {
        if let Some(ref pref) = req.instance_type {
            let a_match = a.data.instance_type == *pref;
            let b_match = b.data.instance_type == *pref;
            if a_match != b_match {
                return b_match.cmp(&a_match);
            }
        }
        a.data.created_at.cmp(&b.data.created_at)
    });

    for candidate in candidates {
        let mut updated = candidate.data.clone();
        updated.state = DevboxState::Claimed;
        updated.owner = Some(req.owner.clone());
        updated.claimed_at = Some(jiff::Timestamp::now());
        updated.owner_tag_applied = false;

        let success = state
            .store
            .compare_and_update(&candidate.id, candidate.version, &updated)
            .await?;

        if success {
            let refreshed = state
                .store
                .get::<DevboxDoc>(&candidate.id)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("devbox vanished after claim"))
                })?;
            return Ok(Json(refreshed.into()));
        }
    }

    Err(AppError::Conflict(
        "pool exhausted: all candidates failed concurrent claim".into(),
    ))
}

/// Release a claimed devbox.
async fn release_devbox(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<ReleaseRequest>,
) -> Result<Json<DevboxResponse>, AppError> {
    let doc = state
        .store
        .get::<DevboxDoc>(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("devbox '{id}' not found")))?;

    if doc.data.state != DevboxState::Claimed {
        return Err(AppError::Conflict(format!(
            "cannot release devbox in '{}' state",
            doc.data.state
        )));
    }

    let current_owner = doc.data.owner.as_deref().unwrap_or("");
    if current_owner != req.owner {
        return Err(AppError::Forbidden("ownership mismatch".into()));
    }

    let mut updated = doc.data.clone();
    updated.state = DevboxState::Terminating;

    let success = state
        .store
        .compare_and_update(&doc.id, doc.version, &updated)
        .await?;
    if !success {
        return Err(AppError::Conflict(
            "devbox was modified concurrently".into(),
        ));
    }

    let refreshed = state
        .store
        .get::<DevboxDoc>(&id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("devbox vanished after release")))?;
    Ok(Json(refreshed.into()))
}

/// Get pool metrics.
async fn pool_metrics(
    State(state): State<AppState>,
) -> Result<Json<PoolMetricsResponse>, AppError> {
    let docs = state.store.list_all::<DevboxDoc>().await?;

    let mut warming = 0u32;
    let mut ready = 0u32;
    let mut claimed = 0u32;
    let mut terminating = 0u32;

    for doc in &docs {
        match doc.data.state {
            DevboxState::Launching => {}
            DevboxState::Warming => warming = warming.saturating_add(1),
            DevboxState::Ready => ready = ready.saturating_add(1),
            DevboxState::Claimed => claimed = claimed.saturating_add(1),
            DevboxState::Terminating => terminating = terminating.saturating_add(1),
        }
    }

    let target = state.reconciler_config.target_warm_pool_size;
    let ready_delta = i32::try_from(target)
        .unwrap_or(i32::MAX)
        .saturating_sub(i32::try_from(ready).unwrap_or(0));

    Ok(Json(PoolMetricsResponse {
        warming,
        ready,
        claimed,
        terminating,
        target_warm_pool_size: target,
        ready_delta,
    }))
}
