//! Reconciler configuration.

use std::time::Duration;

use anyhow::{Result, bail};

use devbox_common::{AmiId, InstanceType, SecurityGroupId, SubnetId};

use crate::compute::LaunchTemplateConfig;

/// Configuration for the pool reconciliation system.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    // --- Pool identity ---
    /// Unique identifier for this pool (used in ASG/LT naming).
    pub pool_id: String,
    /// Unique identity of this server instance (for leader lock and tagging).
    pub server_id: String,

    // --- Instance configuration ---
    /// EC2 instance type for new launches.
    pub instance_type: InstanceType,
    /// AMI ID for new launches.
    pub ami_id: AmiId,
    /// Number of vCPUs required (for InstanceRequirements-based selection).
    pub cpu: u32,
    /// Memory in MiB required (for InstanceRequirements-based selection).
    pub memory_mib: u32,
    /// Subnet IDs for multi-AZ distribution.
    pub subnet_ids: Vec<SubnetId>,
    /// Security group IDs to apply via Launch Template.
    pub security_group_ids: Vec<SecurityGroupId>,

    // --- Pool sizing ---
    /// Number of unclaimed Ready instances to maintain.
    pub target_warm_pool_size: u32,
    /// Maximum number of instances the ASG may run.
    pub max_pool_size: u32,

    // --- Timing ---
    /// Interval between reconciliation ticks.
    pub polling_interval: Duration,
    /// Maximum time an instance may remain in Warming before considered stuck.
    pub stuck_threshold: Duration,
    /// Leader lock time-to-live.
    pub lock_ttl: Duration,
    /// Heartbeat timeout for the warm-up lifecycle hook.
    pub lifecycle_hook_timeout: Duration,
}

impl ReconcilerConfig {
    /// Validate that all configuration fields are within acceptable ranges.
    ///
    /// # Errors
    ///
    /// Returns an error if any field is out of range or constraints are violated.
    pub fn validate(&self) -> Result<()> {
        if self.subnet_ids.is_empty() || self.subnet_ids.len() > 20 {
            bail!(
                "subnet_ids must contain between 1 and 20 entries, got {}",
                self.subnet_ids.len()
            );
        }
        if self.security_group_ids.is_empty() || self.security_group_ids.len() > 5 {
            bail!(
                "security_group_ids must contain between 1 and 5 entries, got {}",
                self.security_group_ids.len()
            );
        }
        if self.target_warm_pool_size < 1 || self.target_warm_pool_size > 100 {
            bail!(
                "target_warm_pool_size must be between 1 and 100, got {}",
                self.target_warm_pool_size
            );
        }
        if self.max_pool_size < 1 || self.max_pool_size > 500 {
            bail!(
                "max_pool_size must be between 1 and 500, got {}",
                self.max_pool_size
            );
        }
        if self.target_warm_pool_size > self.max_pool_size {
            bail!(
                "target_warm_pool_size ({}) must not exceed max_pool_size ({})",
                self.target_warm_pool_size,
                self.max_pool_size
            );
        }
        let hook_secs = self.lifecycle_hook_timeout.as_secs();
        if !(60..=7200).contains(&hook_secs) {
            bail!(
                "lifecycle_hook_timeout must be between 60 and 7200 seconds, got {}",
                hook_secs
            );
        }
        Ok(())
    }

    /// Deterministic ASG name derived from pool identifier.
    #[must_use]
    pub fn asg_name(&self) -> String {
        format!("devbox-pool-{}", self.pool_id)
    }

    /// Deterministic Launch Template name derived from pool identifier.
    #[must_use]
    pub fn launch_template_name(&self) -> String {
        format!("devbox-lt-{}", self.pool_id)
    }

    /// Deterministic lifecycle hook name derived from pool identifier.
    #[must_use]
    pub fn lifecycle_hook_name(&self) -> String {
        format!("devbox-warmup-{}", self.pool_id)
    }

    /// Lifecycle hook timeout as seconds (u32), saturating on overflow.
    #[must_use]
    pub fn lifecycle_hook_timeout_secs(&self) -> u32 {
        u32::try_from(self.lifecycle_hook_timeout.as_secs()).unwrap_or(u32::MAX)
    }

    /// Build a `LaunchTemplateConfig` from this reconciler configuration.
    #[must_use]
    pub fn to_launch_template_config(&self) -> LaunchTemplateConfig {
        LaunchTemplateConfig {
            name: self.launch_template_name(),
            ami_id: self.ami_id.0.clone(),
            instance_type: self.instance_type.0.clone(),
            cpu: self.cpu,
            memory_mib: self.memory_mib,
            security_group_ids: self
                .security_group_ids
                .iter()
                .map(|sg| sg.0.clone())
                .collect(),
            pool_id: self.pool_id.clone(),
            managed_by: self.server_id.clone(),
        }
    }
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            pool_id: String::new(),
            server_id: uuid::Uuid::now_v7().to_string(),
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId(String::new()),
            cpu: 2,
            memory_mib: 8192,
            subnet_ids: Vec::new(),
            security_group_ids: Vec::new(),
            target_warm_pool_size: 2,
            max_pool_size: 10,
            polling_interval: Duration::from_secs(30),
            stuck_threshold: Duration::from_secs(600),
            lock_ttl: Duration::from_secs(60),
            lifecycle_hook_timeout: Duration::from_secs(300),
        }
    }
}
