//! Devbox server binary entry point.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use devbox_server::db::{DocumentStore, Pool, PoolConfig};
use devbox_server::reconcile::spawn_reconciliation_loop;
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

    // Spawn reconciliation loop with cancellation support
    let cancel = CancellationToken::new();
    let reconcile_handle = spawn_reconciliation_loop(Arc::clone(&store), cancel.clone());

    // Build router
    let app = build_router(AppState {
        store: Arc::clone(&store),
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
    tokio::signal::ctrl_c()
        .await
        .ok();
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
