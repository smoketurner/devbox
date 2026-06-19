//! Compute abstraction for ASG-based pool management.

use std::future::Future;

use anyhow::Result;

pub mod ec2;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

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

/// EC2 instance metadata read from `DescribeInstances`, used to populate
/// `DevboxDoc` records for instances the reconciler adopts from the ASG.
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    /// EC2 instance ID.
    pub instance_id: String,
    /// Resolved instance type (e.g., "m7g.large").
    pub instance_type: String,
    /// AMI the instance launched from.
    pub ami_id: String,
    /// Subnet the instance is in.
    pub subnet_id: String,
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

/// Trait defining pool-level compute operations needed by the Reconciler.
///
/// The reconciler is **adopt-only**: Terraform provisions the Launch Template,
/// ASG, and lifecycle hook (see CLAUDE.md). The control plane only reads the ASG
/// and writes runtime state — desired capacity,
/// per-instance scale-in protection, owner tags, and terminations.
pub trait Compute: Send + Sync {
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
    fn describe_asg(&self, asg_name: &str) -> impl Future<Output = Result<AsgDescription>> + Send;

    /// Look up instance metadata (type, AMI, subnet) for the given instance IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS API call fails.
    fn describe_instances(
        &self,
        instance_ids: &[&str],
    ) -> impl Future<Output = Result<Vec<InstanceInfo>>> + Send;

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
