//! Reconciler configuration.

use std::time::Duration;

use devbox_common::{AmiId, InstanceType, SubnetId};

/// Configuration for the pool reconciliation system.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// Number of Ready instances to maintain.
    pub target_pool_size: u32,
    /// EC2 instance type for new launches.
    pub instance_type: InstanceType,
    /// AMI ID for new launches.
    pub ami_id: AmiId,
    /// Subnet ID for new launches.
    pub subnet_id: SubnetId,
    /// Interval between reconciliation ticks.
    pub polling_interval: Duration,
    /// Maximum time an instance may remain in Launching/Warming before stuck.
    pub stuck_threshold: Duration,
    /// Leader lock time-to-live.
    pub lock_ttl: Duration,
    /// Unique identity of this server instance (for leader lock).
    pub server_id: String,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            target_pool_size: 2,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId(String::new()),
            subnet_id: SubnetId(String::new()),
            polling_interval: Duration::from_secs(30),
            stuck_threshold: Duration::from_secs(600), // 10 minutes
            lock_ttl: Duration::from_secs(60),
            server_id: uuid::Uuid::now_v7().to_string(),
        }
    }
}
