//! EC2 client abstraction for instance management.

use anyhow::Result;

pub mod real;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

/// EC2 instance status returned by describe_instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceStatus {
    Pending,
    Running,
    ShuttingDown,
    Terminated,
    Stopping,
    Stopped,
    Unknown(String),
}

impl InstanceStatus {
    /// Parse from the AWS EC2 state name string.
    pub fn from_state_name(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "shutting-down" => Self::ShuttingDown,
            "terminated" => Self::Terminated,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// Trait defining EC2 operations needed by the Reconciler.
pub trait Ec2Client: Send + Sync {
    /// Launch a new EC2 instance. Returns the instance ID.
    fn launch_instance(
        &self,
        instance_type: &str,
        ami_id: &str,
        subnet_id: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;

    /// Terminate an EC2 instance by instance ID.
    fn terminate_instance(
        &self,
        instance_id: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Describe the current status of an EC2 instance.
    fn describe_instance(
        &self,
        instance_id: &str,
    ) -> impl std::future::Future<Output = Result<InstanceStatus>> + Send;
}
