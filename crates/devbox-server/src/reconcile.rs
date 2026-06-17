//! Pool reconciliation loop (placeholder).
//!
//! This module contains the background task that ensures the devbox pool
//! maintains the desired number of warm instances.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::DocumentStore;

/// Spawn the pool reconciliation background task.
///
/// This task periodically checks the pool state and launches or terminates
/// instances to maintain the desired pool size. It listens to the provided
/// `CancellationToken` for graceful shutdown.
///
/// Returns a `JoinHandle` that the caller should await after cancelling the
/// token, to ensure the loop finishes its current tick cleanly.
pub fn spawn_reconciliation_loop(
    store: Arc<DocumentStore>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                () = cancel.cancelled() => {
                    tracing::info!("reconciliation loop shutting down");
                    break;
                }
            }

            tracing::debug!("reconciliation tick");

            // Placeholder: check pool state and reconcile
            let _store = &store;
        }
    })
}
