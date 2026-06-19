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
    /// Maximum time a Warming instance may remain un-ready before being reaped.
    ///
    /// A Warming doc older than this whose instance has not set `devbox:ready=true`
    /// is terminated (ASG relaunches a replacement) and its doc set to Terminating.
    pub ready_timeout: Duration,
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
        let ready_secs = self.ready_timeout.as_secs();
        if !(60..=3600).contains(&ready_secs) {
            bail!("ready_timeout must be between 60 and 3600 seconds, got {ready_secs}");
        }
        if self.polling_interval.is_zero() {
            bail!("polling_interval must be greater than zero");
        }
        if self.lock_ttl.is_zero() {
            bail!("lock_ttl must be greater than zero");
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
            ready_timeout: Duration::from_secs(300),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid default config for use in boundary tests.
    fn valid_config() -> ReconcilerConfig {
        ReconcilerConfig {
            pool_id: "test".to_string(),
            server_id: "test-server".to_string(),
            target_warm_pool_size: 2,
            polling_interval: Duration::from_secs(30),
            lock_ttl: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(300),
        }
    }

    #[test]
    fn ready_timeout_at_minimum_is_valid() {
        let mut cfg = valid_config();
        cfg.ready_timeout = Duration::from_secs(60);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ready_timeout_at_maximum_is_valid() {
        let mut cfg = valid_config();
        cfg.ready_timeout = Duration::from_secs(3600);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ready_timeout_below_minimum_is_rejected() {
        let mut cfg = valid_config();
        cfg.ready_timeout = Duration::from_secs(59);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn ready_timeout_above_maximum_is_rejected() {
        let mut cfg = valid_config();
        cfg.ready_timeout = Duration::from_secs(3601);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_polling_interval_is_rejected() {
        let mut cfg = valid_config();
        cfg.polling_interval = Duration::ZERO;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_lock_ttl_is_rejected() {
        let mut cfg = valid_config();
        cfg.lock_ttl = Duration::ZERO;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valid_config_passes_validation() {
        assert!(valid_config().validate().is_ok());
    }
}
