//! EC2 wrapper module (placeholder).
//!
//! This module will contain the EC2 API client for managing instances.

use anyhow::Result;

/// Trait defining EC2 operations needed by the devbox service.
pub trait Ec2Client: Send + Sync {
    /// Launch a new EC2 instance.
    fn launch_instance(
        &self,
        instance_type: &str,
        ami_id: &str,
        subnet_id: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;

    /// Terminate an EC2 instance.
    fn terminate_instance(
        &self,
        instance_id: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Get the status of an EC2 instance.
    fn describe_instance(
        &self,
        instance_id: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
}
