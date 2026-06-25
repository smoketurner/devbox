//! HTTP route handlers.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};

use devbox_common::{
    ClaimRequest, DEVBOX_NAME_MAX_LEN, DevboxListResponse, DevboxResponse, DevboxState,
    HealthResponse, PoolMetricsResponse, ProtectedResourceMetadata, is_valid_devbox_name,
};

use crate::auth::{Authenticator, Principal};
use crate::db::DocumentStore;
use crate::documents::devbox::DevboxDoc;
use crate::error::{AppError, JsonBody};
use crate::reconcile::ReconcilerConfig;
use crate::ui::build_ui_router;

/// Application state shared across handlers.
///
/// Shared with handlers as a single `Arc<AppState>` (see [`SharedState`]), so the
/// per-request clone is one refcount bump and the fields need no individual
/// `Arc`. `store` is the exception: it is also held by the background reconciler
/// task, so it stays `Arc<DocumentStore>`.
pub struct AppState {
    pub store: Arc<DocumentStore>,
    pub reconciler_config: ReconcilerConfig,
    /// Every API endpoint requires an authenticated principal (only `/health` and
    /// the RFC 9728 discovery document are open). Claim/release additionally bind
    /// `owner` to that principal — the Unix login derived from the token's `email`
    /// claim.
    pub auth: Authenticator,
    /// AWS account the pool runs in (`AWS_ACCOUNT_ID`), advertised in the RFC
    /// 9728 discovery document so `devbox ssh` can auto-select the local AWS
    /// profile for the SSM tunnel. `None` leaves the field out of the document.
    pub aws_account_id: Option<String>,
}

/// Handle to the shared application state, passed to every handler.
pub type SharedState = Arc<AppState>;

/// Authenticate the request once at the edge of the `/api/v1` router: reject with
/// 401 when no valid credential is present, otherwise stash the resolved
/// [`Principal`] in request extensions. Handlers that act as the caller
/// (claim/release) read it back via `Extension<Principal>`; read handlers ignore
/// it. Applied as a `route_layer`, so every current and future `/api/v1` route is
/// authenticated by construction — there is no per-handler opt-in to forget.
///
/// The principal is the Unix login derived from the verified token's `email`
/// claim. That derivation already gates on `is_valid_unix_username` (see
/// `auth::jwt::decode_owner`), so a malformed `email` yields a 401 here rather
/// than a downstream SSH break.
async fn require_auth(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let principal = state.auth.authenticate(req.headers()).await?;
    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

/// Build the Axum router with all routes.
///
/// Every `/api/v1` route sits behind [`require_auth`]; only `/health`
/// (infrastructure health checks present no credential) and the RFC 9728
/// discovery document (fetched pre-login to bootstrap auth) are open. The
/// dashboard routes carry their own OIDC-session gate (see [`build_ui_router`]).
pub fn build_router(state: SharedState) -> Router {
    let api = Router::new()
        .route("/api/v1/devboxes", get(list_devboxes))
        .route("/api/v1/devboxes/{id}", get(get_devbox))
        .route("/api/v1/devboxes/claim", post(claim_devbox))
        .route("/api/v1/devboxes/{id}/release", post(release_devbox))
        .route("/api/v1/pool/metrics", get(pool_metrics))
        .route_layer(from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/health", get(health_check))
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .merge(api)
        .merge(build_ui_router())
        .with_state(state)
}

/// RFC 9728 OAuth 2.0 Protected Resource Metadata endpoint.
///
/// Clients (the `devbox` CLI) fetch this to discover the authorization server
/// and scopes without out-of-band configuration.
///
/// `scopes_supported` is hardcoded to `["openid","email"]` — the minimum the
/// server requires — and NOT derived from `OidcConfig.scope`, which is `None`
/// on API-only deployments. `resource` is advisory/best-effort from the `Host`
/// header; the CLI only reads `authorization_servers`.
async fn protected_resource_metadata(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Json<ProtectedResourceMetadata> {
    // Best-effort: read Host header for the resource URL. The CLI ignores this
    // field; it is advisory per RFC 9728 §3.1.
    let resource = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty())
        .map_or_else(String::new, |h| format!("https://{h}"));

    Json(ProtectedResourceMetadata {
        resource,
        authorization_servers: vec![state.auth.issuer().to_string()],
        scopes_supported: vec!["openid".into(), "email".into()],
        aws_account_id: state.aws_account_id.clone(),
    })
}

/// Health check endpoint.
async fn health_check(State(state): State<SharedState>) -> Json<HealthResponse> {
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
    State(state): State<SharedState>,
) -> Result<Json<DevboxListResponse>, AppError> {
    let docs = state.store.list_all::<DevboxDoc>().await?;
    let devboxes = docs.into_iter().map(DevboxResponse::from).collect();
    Ok(Json(DevboxListResponse { devboxes }))
}

/// Get a single devbox by ID.
async fn get_devbox(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<DevboxResponse>, AppError> {
    let doc = state
        .store
        .get::<DevboxDoc>(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("devbox '{id}' not found")))?;
    Ok(Json(doc.into()))
}

/// Validate and check a requested name override for a claim.
///
/// A blank or absent value yields `None` (the box keeps its auto name). A
/// non-blank value must satisfy [`is_valid_devbox_name`] (`400` otherwise) and
/// must not already be in use by a non-`Terminating` box (`409` otherwise) —
/// names are globally unique so they unambiguously select a box.
async fn resolve_name_override(
    state: &AppState,
    raw: Option<&str>,
) -> Result<Option<String>, AppError> {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    if !is_valid_devbox_name(name) {
        return Err(AppError::BadRequest(format!(
            "invalid name '{name}': use 1-{DEVBOX_NAME_MAX_LEN} lowercase letters, \
             digits, '_' or '-', not starting with '-'"
        )));
    }

    let existing = state.store.find_all::<DevboxDoc>("name", name).await?;
    if existing
        .iter()
        .any(|d| d.data.state != DevboxState::Terminating)
    {
        return Err(AppError::Conflict(format!(
            "name '{name}' is already in use"
        )));
    }

    Ok(Some(name.to_string()))
}

/// Claim an available devbox.
async fn claim_devbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<Principal>,
    JsonBody(req): JsonBody<ClaimRequest>,
) -> Result<Json<DevboxResponse>, AppError> {
    let owner = principal.0;

    // Resolve the optional name override before consuming a box from the pool,
    // so an invalid or already-taken name fails fast.
    let name_override = resolve_name_override(&state, req.name.as_deref()).await?;

    let ready_docs = state.store.find_all::<DevboxDoc>("state", "ready").await?;
    if ready_docs.is_empty() {
        return Err(AppError::Conflict("no devboxes available".into()));
    }

    // Sort candidates by created_at ascending (longest-waiting first).
    let mut candidates = ready_docs;
    candidates.sort_by_key(|a| a.data.created_at);

    for candidate in candidates {
        let mut updated = candidate.data.clone();
        updated.state = DevboxState::Claimed;
        updated.owner = Some(owner.clone());
        updated.claimed_at = Some(jiff::Timestamp::now());
        updated.owner_tag_applied = false;
        if let Some(ref name) = name_override {
            updated.name = name.clone();
        }

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
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<DevboxResponse>, AppError> {
    let caller = principal.0;

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
    State(state): State<SharedState>,
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

    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use devbox_common::{AmiId, InstanceType, SubnetId};
    use jiff::Timestamp;
    use tower::ServiceExt;

    use super::*;
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::Pool;

    use crate::auth::Authenticator;

    /// Build an `AppState` over a single-connection in-memory SQLite store
    /// (`max_connections(1)`, so concurrent handler calls share one database)
    /// whose authenticator resolves every request to `owner` (the JWKS network
    /// boundary is mocked; the JWT verification path itself is covered by the
    /// `auth::jwt` `decode_owner` unit tests).
    async fn setup_state_as(owner: &str) -> SharedState {
        Arc::new(AppState {
            store: Arc::new(test_store().await),
            reconciler_config: test_config(),
            auth: Authenticator::with_test_owner(owner),
            aws_account_id: None,
        })
    }

    /// Default test state: every request authenticates as `jdoe`.
    async fn setup_state() -> SharedState {
        setup_state_as("jdoe").await
    }

    /// Build an `AppState` whose authenticator has no test principal, so a
    /// request without a credential fails with `AuthError::Missing` (no network
    /// touched). Used to assert the unauthenticated path returns 401.
    async fn setup_state_no_principal() -> SharedState {
        use crate::auth::AuthConfig;
        let auth = Authenticator::new(AuthConfig {
            issuer: "https://us.vouch.sh".to_string(),
            jwks_uri: "https://us.vouch.sh/oauth/jwks".to_string(),
            alb_region: None,
            alb_arn: None,
            oidc: None,
        });
        Arc::new(AppState {
            store: Arc::new(test_store().await),
            reconciler_config: test_config(),
            auth,
            aws_account_id: None,
        })
    }

    async fn test_store() -> DocumentStore {
        let pool = Pool::new_test();
        if let Pool::Sqlite(ref p) = pool {
            run_sqlite_migrations(p).await.unwrap();
        }
        DocumentStore::new(pool)
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
            instance_id: "i-1234567890abcdef0".to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            region: "us-east-1".to_string(),
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

    fn claim() -> ClaimRequest {
        ClaimRequest { name: None }
    }

    fn claim_named(name: &str) -> ClaimRequest {
        ClaimRequest {
            name: Some(name.to_string()),
        }
    }

    /// A second ready box with a distinct instance id and name, so tests that
    /// need two candidates don't collide on the unique `instance_id` index.
    fn ready_devbox_other() -> DevboxDoc {
        let mut doc = ready_devbox();
        doc.instance_id = "i-0987654321fedcba0".to_string();
        doc.name = "brave-otter".to_string();
        doc
    }

    /// `Extension<Principal>` the auth middleware would have injected — supplied
    /// directly here so handler-logic tests bypass the (separately tested) layer.
    fn principal(owner: &str) -> Extension<Principal> {
        Extension(Principal(owner.to_string()))
    }

    #[tokio::test]
    async fn claim_marks_box_claimed_and_binds_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let body = claim_devbox(State(state), principal("jdoe"), JsonBody(claim()))
            .await
            .ok()
            .unwrap()
            .0;

        assert_eq!(body.state, DevboxState::Claimed);
        assert_eq!(body.owner.as_deref(), Some("jdoe"));
        // The instance's region (from instance metadata, carried on the doc) is
        // surfaced so the CLI can open the SSM tunnel without client-side config.
        assert_eq!(body.region, "us-east-1");
    }

    #[tokio::test]
    async fn claim_keeps_auto_name_when_no_override() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let body = claim_devbox(State(state), principal("jdoe"), JsonBody(claim()))
            .await
            .ok()
            .unwrap()
            .0;

        assert_eq!(body.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_applies_valid_name_override() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let body = claim_devbox(
            State(state),
            principal("jdoe"),
            JsonBody(claim_named("my-project")),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert_eq!(body.name, "my-project");
        assert_eq!(body.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn claim_blank_override_keeps_auto_name() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let body = claim_devbox(
            State(state),
            principal("jdoe"),
            JsonBody(claim_named("   ")),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert_eq!(body.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_invalid_name_is_bad_request() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let status = status_of(
            claim_devbox(
                State(state),
                principal("jdoe"),
                JsonBody(claim_named("Bad Name")),
            )
            .await,
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn claim_duplicate_name_is_conflict() {
        let state = setup_state().await;
        // An already-claimed box named "taken".
        let mut existing = ready_devbox_other();
        existing.state = DevboxState::Claimed;
        existing.owner = Some("alice".to_string());
        existing.name = "taken".to_string();
        insert(&state, existing).await;
        // A ready box to claim with the colliding name.
        insert(&state, ready_devbox()).await;

        let status = status_of(
            claim_devbox(
                State(state),
                principal("jdoe"),
                JsonBody(claim_named("taken")),
            )
            .await,
        );
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn claim_empty_pool_is_conflict() {
        let state = setup_state().await;
        let status =
            status_of(claim_devbox(State(state), principal("jdoe"), JsonBody(claim())).await);
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn concurrent_claims_yield_one_winner_one_conflict() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let (r1, r2) = tokio::join!(
            claim_devbox(State(state.clone()), principal("jdoe"), JsonBody(claim())),
            claim_devbox(State(state.clone()), principal("jdoe"), JsonBody(claim())),
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
        // Box owned by alice; caller is bob.
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some("alice".to_string());
        let id = insert(&state, doc).await;

        let status = status_of(release_devbox(State(state), Path(id), principal("bob")).await);
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn protected_resource_metadata_returns_correct_shape() {
        let state = setup_state().await;
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "cp.example".parse().unwrap());

        let Json(meta) = protected_resource_metadata(State(state), headers).await;

        assert_eq!(
            meta.authorization_servers.first().map(String::as_str),
            Some("https://us.vouch.sh")
        );
        assert_eq!(meta.scopes_supported, ["openid", "email"]);
        assert_eq!(meta.resource, "https://cp.example");
        // No AWS_ACCOUNT_ID configured for the default test state.
        assert_eq!(meta.aws_account_id, None);
    }

    #[tokio::test]
    async fn protected_resource_metadata_includes_aws_account_id_when_set() {
        let state = Arc::new(AppState {
            store: Arc::new(test_store().await),
            reconciler_config: test_config(),
            auth: Authenticator::with_test_owner("jdoe"),
            aws_account_id: Some("123456789012".to_string()),
        });

        let Json(meta) = protected_resource_metadata(State(state), HeaderMap::new()).await;
        assert_eq!(meta.aws_account_id.as_deref(), Some("123456789012"));
    }

    #[tokio::test]
    async fn protected_resource_metadata_no_host_header_returns_empty_resource() {
        // Pins the documented behavior: missing Host → empty resource string
        // (advisory per RFC 9728 §3.1; CLI only reads authorization_servers).
        let state = setup_state().await;
        let Json(meta) = protected_resource_metadata(State(state), HeaderMap::new()).await;
        assert_eq!(
            meta.resource, "",
            "missing Host header must yield empty resource"
        );
        assert_eq!(meta.authorization_servers, ["https://us.vouch.sh"]);
    }

    #[tokio::test]
    async fn release_of_unclaimed_box_is_conflict() {
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let status = status_of(release_devbox(State(state), Path(id), principal("jdoe")).await);
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn list_returns_devboxes() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;
        let Json(body) = list_devboxes(State(state)).await.ok().unwrap();
        assert_eq!(body.devboxes.len(), 1);
    }

    /// Drive a request through the full router so the auth `route_layer` runs, and
    /// return the resulting status. The body is empty: unauthenticated requests are
    /// rejected by the layer before any handler or body parsing, so even POSTs
    /// surface as 401 here.
    async fn router_status(state: SharedState, method: &str, uri: &str) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        build_router(state).oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn unauthenticated_api_routes_are_rejected() {
        // With no principal configured, the edge layer rejects every /api/v1 route
        // — reads included — with 401 before the handler runs. This pins the
        // secure-by-default wiring: a route under the layer cannot serve anonymously.
        for (method, uri) in [
            ("GET", "/api/v1/devboxes"),
            ("GET", "/api/v1/devboxes/any-id"),
            ("GET", "/api/v1/pool/metrics"),
            ("POST", "/api/v1/devboxes/claim"),
            ("POST", "/api/v1/devboxes/any-id/release"),
        ] {
            let status = router_status(setup_state_no_principal().await, method, uri).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must be 401"
            );
        }
    }

    #[tokio::test]
    async fn health_and_discovery_stay_open() {
        // The two endpoints deliberately outside the auth layer answer without a
        // credential.
        for uri in ["/health", "/.well-known/oauth-protected-resource"] {
            let status = router_status(setup_state_no_principal().await, "GET", uri).await;
            assert_eq!(status, StatusCode::OK, "{uri} must stay open");
        }
    }

    #[tokio::test]
    async fn authenticated_api_read_passes_the_layer() {
        // with_test_owner authenticates every request, so the layer admits it and
        // the read handler responds 200 — proving the layer passes traffic through,
        // not just blocks it.
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;
        let status = router_status(state, "GET", "/api/v1/devboxes").await;
        assert_eq!(status, StatusCode::OK);
    }
}
