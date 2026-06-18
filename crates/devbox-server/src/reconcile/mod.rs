//! Pool reconciliation loop.
//!
//! This module contains the background task that ensures the devbox pool
//! maintains the desired number of warm instances. It acquires a distributed
//! leader lock before each tick to prevent duplicate actions across replicas.

pub mod config;
mod lock;
mod tick;

#[cfg(test)]
mod tests;

pub use config::ReconcilerConfig;

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::DocumentStore;
use crate::compute::Compute;

/// Spawn the pool reconciliation background task.
///
/// This task periodically checks the pool state and launches or terminates
/// instances to maintain the desired pool size. It acquires a distributed
/// leader lock before each tick to prevent duplicate actions across replicas.
pub fn spawn_reconciliation_loop<E: Compute + 'static>(
    store: Arc<DocumentStore>,
    ec2: Arc<E>,
    config: ReconcilerConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.polling_interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                () = cancel.cancelled() => {
                    tracing::info!("reconciliation loop shutting down");
                    break;
                }
            }

            // Try to acquire the leader lock
            match lock::try_acquire_lock(&store, &config).await {
                Ok(true) => {
                    // We hold the lock — perform reconciliation
                    if let Err(e) = tick::reconciliation_tick(&store, &*ec2, &config).await {
                        tracing::error!(error = %e, "reconciliation tick failed");
                    }
                }
                Ok(false) => {
                    tracing::debug!("another instance holds the leader lock, skipping tick");
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to acquire leader lock");
                }
            }
        }
    })
}
