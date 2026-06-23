//! Devbox server binary entry point.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use devbox_server::auth::{AuthConfig, Authenticator, OidcConfig};
use devbox_server::compute::ec2::Ec2;
use devbox_server::db::{DocumentStore, Pool, PoolConfig};
use devbox_server::reconcile::{ReconcilerConfig, spawn_reconciliation_loop};
use devbox_server::routes::{AppState, build_router};
use secrecy::SecretString;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite::memory:".to_string());
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .context("invalid PORT")?;

    tracing::info!(database_url = %redact_url(&database_url), port, "starting devbox server");

    // Connect to database
    let pool = Pool::connect(&database_url, &PoolConfig::default()).await?;

    // Run migrations
    match &pool {
        Pool::Sqlite(p) => {
            devbox_server::db::migrations::run_sqlite_migrations(p).await?;
        }
        Pool::Postgres(p) => {
            devbox_server::db::migrations::run_dsql_migrations(p).await?;
        }
    }

    let store = Arc::new(DocumentStore::new(pool));

    // Load reconciler config from environment. Instance type, AMI, subnets,
    // security groups, and pool max are owned by Terraform on the Launch Template
    // and ASG; the control plane only adopts the ASG and maintains warm capacity.
    let reconciler_config = ReconcilerConfig {
        pool_id: std::env::var("POOL_ID").unwrap_or_else(|_| "default".to_string()),
        server_id: uuid::Uuid::now_v7().to_string(),
        target_warm_pool_size: std::env::var("POOL_TARGET_WARM_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        polling_interval: Duration::from_secs(
            std::env::var("POOL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        ),
        lock_ttl: Duration::from_secs(
            std::env::var("POOL_LOCK_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        ),
        ready_timeout: Duration::from_secs(
            std::env::var("POOL_READY_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        ),
    };

    reconciler_config
        .validate()
        .context("invalid reconciler configuration")?;

    let reconciler_config = Arc::new(reconciler_config);

    // Load AWS config and create EC2 client. Retries are configured here, at the
    // SDK layer, so every compute call absorbs transient throttling/brownouts
    // without per-call retry loops — a momentary EC2 API failure must not stall
    // the warm pool (the reconciler simply continues on the next tick).
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(5))
        .load()
        .await;
    let ec2_client = Arc::new(Ec2::new(&aws_config));

    // Spawn reconciliation loop with cancellation support
    let cancel = CancellationToken::new();
    let reconcile_handle = spawn_reconciliation_loop(
        Arc::clone(&store),
        ec2_client,
        (*reconciler_config).clone(),
        cancel.clone(),
    );

    // Build router
    let app = build_router(AppState {
        store: Arc::clone(&store),
        reconciler_config: Arc::clone(&reconciler_config),
        auth: build_authenticator(),
    });

    // Start server
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {}", addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Signal the reconciliation loop to stop and wait for it
    cancel.cancel();
    let _join = reconcile_handle.await;

    // Cleanup
    store.pool().close().await;
    tracing::info!("server shut down");

    Ok(())
}

/// Build the request authenticator from the environment.
///
/// Authentication is always on: every claim/release binds `owner` to the
/// authenticated principal (the Unix login derived from the token's `email`
/// claim), so there is no unauthenticated path. OIDC endpoints default to
/// Vouch; override with `AUTH_OIDC_ISSUER` / `AUTH_OIDC_JWKS_URI`.
fn build_authenticator() -> Arc<Authenticator> {
    let oidc = build_oidc_config();
    let config = AuthConfig {
        issuer: std::env::var("AUTH_OIDC_ISSUER")
            .unwrap_or_else(|_| "https://us.vouch.sh".to_string()),
        jwks_uri: std::env::var("AUTH_OIDC_JWKS_URI")
            .unwrap_or_else(|_| "https://us.vouch.sh/oauth/jwks".to_string()),
        alb_region: std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()),
        alb_arn: std::env::var("AUTH_ALB_ARN").ok().filter(|s| !s.is_empty()),
        oidc,
    };
    tracing::info!(
        issuer = %config.issuer,
        dashboard_login = config.oidc.is_some(),
        "API authentication enabled (owner = email local part)"
    );
    Arc::new(Authenticator::new(config))
}

/// Build the dashboard OIDC login config from the environment.
///
/// Returns `Some` only when `AUTH_OIDC_CLIENT_ID`, `AUTH_OIDC_CLIENT_SECRET`,
/// and `AUTH_OIDC_REDIRECT_URI` are all set; otherwise the dashboard is left
/// ungated (e.g. local dev, or when an ALB does the OIDC gating). Endpoints and
/// scope default to Vouch.
fn build_oidc_config() -> Option<OidcConfig> {
    let nonempty = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());

    let client_id = nonempty("AUTH_OIDC_CLIENT_ID")?;
    let client_secret = nonempty("AUTH_OIDC_CLIENT_SECRET")?;
    let redirect_uri = nonempty("AUTH_OIDC_REDIRECT_URI")?;

    Some(OidcConfig {
        client_id,
        client_secret: SecretString::from(client_secret),
        redirect_uri,
        authorize_endpoint: nonempty("AUTH_OIDC_AUTHORIZATION_ENDPOINT")
            .unwrap_or_else(|| "https://us.vouch.sh/oauth/authorize".to_string()),
        token_endpoint: nonempty("AUTH_OIDC_TOKEN_ENDPOINT")
            .unwrap_or_else(|| "https://us.vouch.sh/oauth/token".to_string()),
        scope: nonempty("AUTH_OIDC_SCOPE").unwrap_or_else(|| "openid email".to_string()),
    })
}

/// Wait for a shutdown signal (Ctrl+C or SIGTERM).
///
/// ECS/Fargate stops a task by sending SIGTERM (Docker's default `STOPSIGNAL`),
/// so listening only for SIGINT would skip the graceful-shutdown path and let the
/// kernel kill the process mid-request. A failed handler registration falls back
/// to a pending future rather than panicking.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending().await
            }
        }
    };

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT, initiating graceful shutdown"),
        () = terminate => tracing::info!("received SIGTERM, initiating graceful shutdown"),
    }
}

/// Redact password from database URL for logging.
fn redact_url(url: &str) -> String {
    if url.starts_with("sqlite:") {
        return url.to_string();
    }
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                let _set = parsed.set_password(Some("***"));
            }
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}
