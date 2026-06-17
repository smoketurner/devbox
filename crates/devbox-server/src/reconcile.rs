//! Pool reconciliation loop (placeholder).
//!
//! This module contains the background task that ensures the devbox pool
//! maintains the desired number of warm instances.

use std::sync::Arc;

use crate::db::DocumentStore;

/// Spawn the pool reconciliation background task.
///
/// This task periodically checks the pool state and launches or terminates
/// instances to maintain the desired pool size.
pub fn spawn_reconciliation_loop(store: Arc<DocumentStore>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

        loop {
            interval.tick().await;
            tracing::debug!("reconciliation tick");

            // Placeholder: check pool state and reconcile
            let _store = &store;
        }
    });
}
