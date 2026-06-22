//! HTTP route handlers.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Json;
use axum::routing::{get, post};

use devbox_common::{
    ClaimRequest, DevboxListResponse, DevboxResponse, DevboxState, HealthResponse,
    PoolMetricsResponse, ReleaseRequest, is_valid_unix_username,
};

use crate::auth::Authenticator;
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
    /// When set, claim/release require an authenticated principal and bind
    /// `owner` to it. When `None` (local dev), the request body's `owner` is
    /// trusted.
    pub auth: Option<Arc<Authenticator>>,
}

/// Resolve the caller's `owner`: when auth is enabled, the Unix login derived
/// from the token's `email` claim; otherwise the body-supplied owner (local dev).
///
/// The owner doubles as the Unix login account the host provisions for the
/// claimant (see [`is_valid_unix_username`]). The authenticated path already
/// derives a valid login from the email; this guard catches a non-Unix-safe
/// body-supplied owner in the no-auth path rather than silently breaking SSH.
async fn resolve_owner(
    state: &AppState,
    headers: &HeaderMap,
    body_owner: &str,
) -> Result<String, AppError> {
    let owner = match &state.auth {
        Some(auth) => auth.authenticate(headers).await?.0,
        None => body_owner.trim().to_string(),
    };
    if !is_valid_unix_username(&owner) {
        return Err(AppError::BadRequest(format!(
            "owner '{owner}' is not a valid Unix login name (must match \
             ^[a-z_][a-z0-9_-]*$, at most 32 characters)"
        )));
    }
    Ok(owner)
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
    headers: HeaderMap,
    JsonBody(req): JsonBody<ClaimRequest>,
) -> Result<Json<DevboxResponse>, AppError> {
    let owner = resolve_owner(&state, &headers, &req.owner).await?;

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
        updated.owner = Some(owner.clone());
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
    headers: HeaderMap,
    JsonBody(req): JsonBody<ReleaseRequest>,
) -> Result<Json<DevboxResponse>, AppError> {
    let caller = resolve_owner(&state, &headers, &req.owner).await?;

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
    if current_owner != caller {
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

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::time::Duration;

    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use devbox_common::{AmiId, InstanceType, SubnetId};
    use jiff::Timestamp;

    use super::*;
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::Pool;

    /// Build an `AppState` over a single-connection in-memory SQLite store
    /// (`max_connections(1)`, so concurrent handler calls share one database)
    /// with auth disabled.
    async fn setup_state() -> AppState {
        let pool = Pool::new_test();
        if let Pool::Sqlite(ref p) = pool {
            run_sqlite_migrations(p).await.unwrap();
        }
        AppState {
            store: Arc::new(DocumentStore::new(pool)),
            reconciler_config: Arc::new(test_config()),
            auth: None,
        }
    }

    fn test_config() -> ReconcilerConfig {
        ReconcilerConfig {
            pool_id: "test".to_string(),
            server_id: "test-server".to_string(),
            target_warm_pool_size: 1,
            polling_interval: Duration::from_secs(30),
            lock_ttl: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(60),
        }
    }

    fn ready_devbox() -> DevboxDoc {
        DevboxDoc {
            instance_id: Some("i-1234567890abcdef0".to_string()),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            ebs_volume_id: None,
            owner: None,
            claimed_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
        }
    }

    async fn insert(state: &AppState, doc: DevboxDoc) -> String {
        state.store.insert(&doc).await.unwrap().id
    }

    /// Map a handler result to the HTTP status it would produce.
    fn status_of<T: IntoResponse>(result: Result<T, AppError>) -> StatusCode {
        match result {
            Ok(ok) => ok.into_response().status(),
            Err(err) => err.into_response().status(),
        }
    }

    fn claim(owner: &str) -> ClaimRequest {
        ClaimRequest {
            owner: owner.to_string(),
            instance_type: None,
        }
    }

    #[tokio::test]
    async fn claim_marks_box_claimed_with_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let body = claim_devbox(State(state), HeaderMap::new(), JsonBody(claim("jplock")))
            .await
            .ok()
            .unwrap()
            .0;

        assert_eq!(body.state, DevboxState::Claimed);
        assert_eq!(body.owner.as_deref(), Some("jplock"));
    }

    #[tokio::test]
    async fn claim_empty_pool_is_conflict() {
        let state = setup_state().await;
        let status = status_of(
            claim_devbox(State(state), HeaderMap::new(), JsonBody(claim("jplock"))).await,
        );
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn claim_rejects_non_unix_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;
        let status = status_of(
            claim_devbox(
                State(state),
                HeaderMap::new(),
                JsonBody(claim("justin@plock.net")),
            )
            .await,
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn claim_rejects_blank_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;
        let status =
            status_of(claim_devbox(State(state), HeaderMap::new(), JsonBody(claim("   "))).await);
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn concurrent_claims_yield_one_winner_one_conflict() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let (r1, r2) = tokio::join!(
            claim_devbox(
                State(state.clone()),
                HeaderMap::new(),
                JsonBody(claim("alice"))
            ),
            claim_devbox(
                State(state.clone()),
                HeaderMap::new(),
                JsonBody(claim("bob"))
            ),
        );

        let statuses = [status_of(r1), status_of(r2)];
        let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
        let conflict = statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count();
        assert_eq!(ok, 1, "exactly one claim must win");
        assert_eq!(conflict, 1, "the loser must get 409 Conflict");
    }

    #[tokio::test]
    async fn release_by_non_owner_is_forbidden() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some("alice".to_string());
        let id = insert(&state, doc).await;

        let status = status_of(
            release_devbox(
                State(state),
                Path(id),
                HeaderMap::new(),
                JsonBody(ReleaseRequest {
                    owner: "bob".to_string(),
                }),
            )
            .await,
        );
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn release_of_unclaimed_box_is_conflict() {
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let status = status_of(
            release_devbox(
                State(state),
                Path(id),
                HeaderMap::new(),
                JsonBody(ReleaseRequest {
                    owner: "alice".to_string(),
                }),
            )
            .await,
        );
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
