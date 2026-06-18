//! Devbox server binary entry point.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use devbox_common::{AmiId, InstanceType, SecurityGroupId, SubnetId};
use devbox_server::db::{DocumentStore, Pool, PoolConfig};
use devbox_server::compute::ec2::Ec2;
use devbox_server::reconcile::{ReconcilerConfig, spawn_reconciliation_loop};
use devbox_server::routes::{AppState, build_router};

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

    // Load reconciler config from environment
    let reconciler_config = ReconcilerConfig {
        pool_id: std::env::var("POOL_ID").unwrap_or_else(|_| "default".to_string()),
        server_id: uuid::Uuid::now_v7().to_string(),
        instance_type: InstanceType(
            std::env::var("POOL_INSTANCE_TYPE").unwrap_or_else(|_| "m5.large".to_string()),
        ),
        ami_id: AmiId(std::env::var("POOL_AMI_ID").unwrap_or_default()),
        cpu: std::env::var("POOL_CPU")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        memory_mib: std::env::var("POOL_MEMORY_MIB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8192),
        subnet_ids: std::env::var("POOL_SUBNET_IDS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| SubnetId(s.trim().to_string()))
            .collect(),
        security_group_ids: std::env::var("POOL_SECURITY_GROUP_IDS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| SecurityGroupId(s.trim().to_string()))
            .collect(),
        target_warm_pool_size: std::env::var("POOL_TARGET_WARM_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        max_pool_size: std::env::var("POOL_MAX_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        polling_interval: Duration::from_secs(
            std::env::var("POOL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        ),
        stuck_threshold: Duration::from_secs(
            std::env::var("POOL_STUCK_THRESHOLD_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        ),
        lock_ttl: Duration::from_secs(
            std::env::var("POOL_LOCK_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        ),
        lifecycle_hook_timeout: Duration::from_secs(
            std::env::var("POOL_LIFECYCLE_HOOK_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        ),
    };

    reconciler_config
        .validate()
        .context("invalid reconciler configuration")?;

    let reconciler_config = Arc::new(reconciler_config);

    // Load AWS config and create EC2 client
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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

/// Wait for shutdown signal (Ctrl+C).
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown signal received");
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
