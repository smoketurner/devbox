//! Reconciler configuration.

use std::time::Duration;

use anyhow::{Result, bail};

/// Configuration for the pool reconciliation system.
///
/// The reconciler is adopt-only: instance type, AMI, subnets, security groups,
/// and pool sizing bounds (min/max) live in Terraform on the Launch Template and
/// ASG (see CLAUDE.md). The control plane keeps only its identity, the warm-pool
/// target, and timing.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// Pool identifier; the adopted ASG is `devbox-pool-<pool_id>`.
    pub pool_id: String,
    /// Unique identity of this server instance (for the leader lock).
    pub server_id: String,
    /// Number of unclaimed Ready instances to maintain.
    pub target_warm_pool_size: u32,
    /// Interval between reconciliation ticks.
    pub polling_interval: Duration,
    /// Leader lock time-to-live.
    pub lock_ttl: Duration,
}

impl ReconcilerConfig {
    /// Validate that all configuration fields are within acceptable ranges.
    ///
    /// # Errors
    ///
    /// Returns an error if any field is out of range or constraints are violated.
    pub fn validate(&self) -> Result<()> {
        if self.pool_id.trim().is_empty() {
            bail!("pool_id must not be empty");
        }
        if self.target_warm_pool_size < 1 || self.target_warm_pool_size > 100 {
            bail!(
                "target_warm_pool_size must be between 1 and 100, got {}",
                self.target_warm_pool_size
            );
        }
        Ok(())
    }

    /// Deterministic ASG name derived from the pool identifier (the adoption key).
    #[must_use]
    pub fn asg_name(&self) -> String {
        format!("devbox-pool-{}", self.pool_id)
    }
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            pool_id: "default".to_string(),
            server_id: uuid::Uuid::now_v7().to_string(),
            target_warm_pool_size: 2,
            polling_interval: Duration::from_secs(30),
            lock_ttl: Duration::from_secs(60),
        }
    }
}
