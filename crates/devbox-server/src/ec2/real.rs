//! Real EC2 client implementation using the AWS SDK.

use anyhow::{Context, Result};
use aws_sdk_ec2::Client as AwsEc2Client;

use super::{Ec2Client, InstanceStatus};

/// Real EC2 client backed by the AWS SDK.
pub struct RealEc2Client {
    client: AwsEc2Client,
}

impl RealEc2Client {
    /// Create a new real EC2 client from an AWS SDK config.
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        Self {
            client: AwsEc2Client::new(config),
        }
    }
}

impl Ec2Client for RealEc2Client {
    async fn launch_instance(
        &self,
        instance_type: &str,
        ami_id: &str,
        subnet_id: &str,
    ) -> Result<String> {
        let resp = self
            .client
            .run_instances()
            .image_id(ami_id)
            .instance_type(instance_type.into())
            .subnet_id(subnet_id)
            .min_count(1)
            .max_count(1)
            .send()
            .await
            .context("EC2 RunInstances call failed")?;

        let instance = resp
            .instances()
            .first()
            .context("RunInstances returned no instances")?;

        instance
            .instance_id()
            .map(|id| id.to_string())
            .context("instance missing instance_id")
    }

    async fn terminate_instance(&self, instance_id: &str) -> Result<()> {
        self.client
            .terminate_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .context("EC2 TerminateInstances call failed")?;
        Ok(())
    }

    async fn describe_instance(&self, instance_id: &str) -> Result<InstanceStatus> {
        let resp = self
            .client
            .describe_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .context("EC2 DescribeInstances call failed")?;

        let instance = resp
            .reservations()
            .first()
            .and_then(|r| r.instances().first())
            .context("DescribeInstances returned no matching instance")?;

        let state_name = instance
            .state()
            .and_then(|s| s.name())
            .map_or_else(|| "unknown".to_string(), |n| n.as_str().to_string());

        Ok(InstanceStatus::from_state_name(&state_name))
    }
}
