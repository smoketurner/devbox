//! Compute abstraction for ASG-based pool management.

use std::future::Future;

use anyhow::Result;

pub mod ec2;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

/// Result of ensuring a Launch Template exists.
#[derive(Debug, Clone)]
pub struct LaunchTemplateResult {
    /// Launch Template ID (e.g., "lt-0123456789abcdef0").
    pub id: String,
    /// Version number of the active template version.
    pub version: i64,
}

/// A single instance record from an ASG describe call.
#[derive(Debug, Clone)]
pub struct AsgInstance {
    /// EC2 instance ID.
    pub instance_id: String,
    /// ASG lifecycle state (e.g., "Pending:Wait", "InService", "Terminating").
    pub lifecycle_state: String,
    /// Health status ("Healthy" or "Unhealthy").
    pub health_status: String,
    /// Whether scale-in protection is enabled.
    pub protected_from_scale_in: bool,
}

/// Result of describing an ASG.
#[derive(Debug, Clone)]
pub struct AsgDescription {
    /// Current desired capacity.
    pub desired_capacity: u32,
    /// Configured minimum size.
    pub min_size: u32,
    /// Configured maximum size.
    pub max_size: u32,
    /// Launch Template ID currently referenced.
    pub launch_template_id: Option<String>,
    /// Launch Template version currently referenced.
    pub launch_template_version: Option<String>,
    /// Instances in the ASG.
    pub instances: Vec<AsgInstance>,
}

/// Configuration for creating/updating a Launch Template.
#[derive(Debug, Clone)]
pub struct LaunchTemplateConfig {
    /// Template name.
    pub name: String,
    /// AMI ID for the instance image.
    pub ami_id: String,
    /// EC2 instance type (e.g., "m5.large").
    pub instance_type: String,
    /// Number of vCPUs required (used for InstanceRequirements-based selection).
    pub cpu: u32,
    /// Memory in MiB required (used for InstanceRequirements-based selection).
    pub memory_mib: u32,
    /// Security group IDs to attach.
    pub security_group_ids: Vec<String>,
    /// Pool identifier for tagging.
    pub pool_id: String,
    /// Server identity for managed-by tagging.
    pub managed_by: String,
}

/// Configuration for creating/updating an ASG.
#[derive(Debug, Clone)]
pub struct AsgConfig {
    /// ASG name.
    pub name: String,
    /// Launch Template ID to reference.
    pub launch_template_id: String,
    /// Launch Template version to pin.
    pub launch_template_version: i64,
    /// Subnet IDs for multi-AZ distribution.
    pub subnet_ids: Vec<String>,
    /// Minimum number of instances.
    pub min_size: u32,
    /// Maximum number of instances.
    pub max_size: u32,
    /// Desired number of instances.
    pub desired_capacity: u32,
    /// Health check grace period in seconds.
    pub health_check_grace_period: u32,
    /// Whether to propagate ASG-level tags to instances at launch.
    pub propagate_tags_at_launch: bool,
    /// Pool identifier for tagging.
    pub pool_id: String,
    /// Server identity for managed-by tagging.
    pub managed_by: String,
}

/// Configuration for a lifecycle hook.
#[derive(Debug, Clone)]
pub struct LifecycleHookConfig {
    /// Name of the ASG to attach the hook to.
    pub asg_name: String,
    /// Name of the lifecycle hook.
    pub hook_name: String,
    /// Heartbeat timeout in seconds before the hook times out.
    pub heartbeat_timeout_secs: u32,
}

/// Trait defining pool-level compute operations needed by the Reconciler.
pub trait Compute: Send + Sync {
    /// Ensure a Launch Template exists with the given configuration.
    ///
    /// Creates a new version if config differs from current, or creates
    /// the template if it doesn't exist. When cpu and memory_mib are set,
    /// configures InstanceRequirements (VCpuCount, MemoryMiB) for flexible
    /// instance type selection.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn ensure_launch_template(
        &self,
        config: &LaunchTemplateConfig,
    ) -> impl Future<Output = Result<LaunchTemplateResult>> + Send;

    /// Ensure an ASG exists with the given configuration.
    ///
    /// Creates the ASG if absent, updates if config differs.
    /// When `propagate_tags_at_launch` is true, applies all ASG tags
    /// with PropagateAtLaunch=true via CreateOrUpdateTags.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn ensure_asg(
        &self,
        config: &AsgConfig,
    ) -> impl Future<Output = Result<String>> + Send;

    /// Ensure a lifecycle hook is attached to the ASG.
    ///
    /// Uses PutLifecycleHook which is idempotent (creates or updates).
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn ensure_lifecycle_hook(
        &self,
        config: &LifecycleHookConfig,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Set the desired capacity of the named ASG.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn set_desired_capacity(
        &self,
        asg_name: &str,
        desired: u32,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Describe the ASG, returning its current configuration and instance list.
    ///
    /// # Errors
    ///
    /// Returns an error if the ASG is not found or the API call fails.
    fn describe_asg(
        &self,
        asg_name: &str,
    ) -> impl Future<Output = Result<AsgDescription>> + Send;

    /// Terminate a specific instance in the ASG.
    ///
    /// The `should_decrement` parameter controls whether the ASG's desired
    /// capacity is decremented (false means the ASG will launch a replacement).
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn terminate_instance_in_asg(
        &self,
        instance_id: &str,
        should_decrement: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Complete a lifecycle action for the given hook and instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn complete_lifecycle_action(
        &self,
        asg_name: &str,
        hook_name: &str,
        instance_id: &str,
        action_result: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Apply key-value tags to an EC2 instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn tag_instance(
        &self,
        instance_id: &str,
        tags: &[(&str, &str)],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Set or remove scale-in protection on instances within an ASG.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn set_scale_in_protection(
        &self,
        asg_name: &str,
        instance_ids: &[&str],
        protected: bool,
    ) -> impl Future<Output = Result<()>> + Send;
}
