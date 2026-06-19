//! Production compute implementation using AWS EC2 and Auto Scaling APIs.

use anyhow::{Context, Result};

use super::{AsgDescription, AsgInstance, Compute, InstanceInfo};

/// Production compute client backed by the AWS EC2 and Auto Scaling SDKs.
pub struct Ec2 {
    ec2_client: aws_sdk_ec2::Client,
    asg_client: aws_sdk_autoscaling::Client,
}

impl Ec2 {
    /// Create a new compute client from an AWS SDK config.
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        Self {
            ec2_client: aws_sdk_ec2::Client::new(config),
            asg_client: aws_sdk_autoscaling::Client::new(config),
        }
    }
}

impl Compute for Ec2 {
    async fn set_desired_capacity(&self, asg_name: &str, desired: u32) -> Result<()> {
        let desired_i32 = i32::try_from(desired).context("desired capacity exceeds i32")?;

        self.asg_client
            .update_auto_scaling_group()
            .auto_scaling_group_name(asg_name)
            .desired_capacity(desired_i32)
            .send()
            .await
            .context("failed to set desired capacity")?;

        Ok(())
    }

    async fn describe_asg(&self, asg_name: &str) -> Result<AsgDescription> {
        let output = self
            .asg_client
            .describe_auto_scaling_groups()
            .auto_scaling_group_names(asg_name)
            .send()
            .await
            .context("failed to describe auto scaling group")?;

        let groups = output.auto_scaling_groups();
        let group = groups
            .first()
            .with_context(|| format!("ASG '{asg_name}' not found"))?;

        let desired_raw = group
            .desired_capacity
            .context("ASG missing desired_capacity")?;
        let desired_capacity =
            u32::try_from(desired_raw).context("desired_capacity is negative")?;
        let min_raw = group.min_size.context("ASG missing min_size")?;
        let min_size = u32::try_from(min_raw).context("min_size is negative")?;
        let max_raw = group.max_size.context("ASG missing max_size")?;
        let max_size = u32::try_from(max_raw).context("max_size is negative")?;

        let (launch_template_id, launch_template_version) =
            group.launch_template().map_or((None, None), |lt| {
                (
                    lt.launch_template_id().map(String::from),
                    lt.version().map(String::from),
                )
            });

        let instances = group
            .instances()
            .iter()
            .map(|inst| AsgInstance {
                instance_id: inst.instance_id().unwrap_or_default().to_string(),
                lifecycle_state: inst
                    .lifecycle_state()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default(),
                health_status: inst.health_status().unwrap_or_default().to_string(),
                protected_from_scale_in: inst.protected_from_scale_in().unwrap_or(false),
            })
            .collect();

        Ok(AsgDescription {
            desired_capacity,
            min_size,
            max_size,
            launch_template_id,
            launch_template_version,
            instances,
        })
    }

    async fn terminate_instance_in_asg(
        &self,
        instance_id: &str,
        should_decrement: bool,
    ) -> Result<()> {
        self.asg_client
            .terminate_instance_in_auto_scaling_group()
            .instance_id(instance_id)
            .should_decrement_desired_capacity(should_decrement)
            .send()
            .await
            .context("failed to terminate instance in auto scaling group")?;

        Ok(())
    }

    async fn describe_instances(&self, instance_ids: &[&str]) -> Result<Vec<InstanceInfo>> {
        if instance_ids.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = instance_ids.iter().map(|id| (*id).to_string()).collect();
        let output = self
            .ec2_client
            .describe_instances()
            .set_instance_ids(Some(ids))
            .send()
            .await
            .context("failed to describe instances")?;

        let mut infos = Vec::new();
        for reservation in output.reservations() {
            for instance in reservation.instances() {
                let Some(instance_id) = instance.instance_id() else {
                    continue;
                };
                infos.push(InstanceInfo {
                    instance_id: instance_id.to_string(),
                    instance_type: instance
                        .instance_type()
                        .map(|t| t.as_str().to_string())
                        .unwrap_or_default(),
                    ami_id: instance.image_id().unwrap_or_default().to_string(),
                    subnet_id: instance.subnet_id().unwrap_or_default().to_string(),
                });
            }
        }

        Ok(infos)
    }

    async fn tag_instance(&self, instance_id: &str, tags: &[(&str, &str)]) -> Result<()> {
        let ec2_tags: Vec<aws_sdk_ec2::types::Tag> = tags
            .iter()
            .map(|(key, value)| {
                aws_sdk_ec2::types::Tag::builder()
                    .key(*key)
                    .value(*value)
                    .build()
            })
            .collect();

        self.ec2_client
            .create_tags()
            .resources(instance_id)
            .set_tags(Some(ec2_tags))
            .send()
            .await
            .context("failed to tag instance")?;

        Ok(())
    }

    async fn set_scale_in_protection(
        &self,
        asg_name: &str,
        instance_ids: &[&str],
        protected: bool,
    ) -> Result<()> {
        let ids: Vec<String> = instance_ids.iter().map(|id| (*id).to_string()).collect();

        self.asg_client
            .set_instance_protection()
            .auto_scaling_group_name(asg_name)
            .set_instance_ids(Some(ids))
            .protected_from_scale_in(protected)
            .send()
            .await
            .context("failed to set instance protection")?;

        Ok(())
    }
}
