//! Production compute implementation using AWS EC2 and Auto Scaling APIs.

use anyhow::{Context, Result};

use super::{
    AsgConfig, AsgDescription, AsgInstance, Compute, LaunchTemplateConfig, LaunchTemplateResult,
    LifecycleHookConfig,
};

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
    async fn ensure_launch_template(
        &self,
        config: &LaunchTemplateConfig,
    ) -> Result<LaunchTemplateResult> {
        // Check if the launch template already exists by name.
        let describe_result = self
            .ec2_client
            .describe_launch_templates()
            .launch_template_names(&config.name)
            .send()
            .await;

        let template_data = build_launch_template_data(config)?;

        match describe_result {
            Ok(output) => {
                let templates = output.launch_templates();
                if templates.is_empty() {
                    // Template not found, create it.
                    create_launch_template(self, config, template_data).await
                } else {
                    // Template exists, create a new version.
                    let template = templates
                        .first()
                        .context("launch template list was unexpectedly empty")?;
                    let template_id = template
                        .launch_template_id()
                        .context("launch template missing ID")?;
                    create_launch_template_version(self, template_id, template_data).await
                }
            }
            Err(err) => {
                // If the error is "not found", create the template.
                let service_err = err.into_service_error();
                let msg = service_err.to_string();
                if msg.contains("not found") || msg.contains("InvalidLaunchTemplateName") {
                    create_launch_template(self, config, template_data).await
                } else {
                    Err(anyhow::Error::from(service_err))
                        .context("failed to describe launch templates")
                }
            }
        }
    }

    async fn ensure_asg(&self, config: &AsgConfig) -> Result<String> {
        let describe_result = self
            .asg_client
            .describe_auto_scaling_groups()
            .auto_scaling_group_names(&config.name)
            .send()
            .await
            .context("failed to describe auto scaling groups")?;

        let groups = describe_result.auto_scaling_groups();
        let version_str = config.launch_template_version.to_string();
        let vpc_zone_identifier = config.subnet_ids.join(",");
        let desired_i32 =
            i32::try_from(config.desired_capacity).context("desired_capacity exceeds i32")?;
        let min_i32 = i32::try_from(config.min_size).context("min_size exceeds i32")?;
        let max_i32 = i32::try_from(config.max_size).context("max_size exceeds i32")?;
        let grace_i32 = i32::try_from(config.health_check_grace_period)
            .context("health_check_grace_period exceeds i32")?;

        let lt_spec = aws_sdk_autoscaling::types::LaunchTemplateSpecification::builder()
            .launch_template_id(&config.launch_template_id)
            .version(&version_str)
            .build();

        if groups.is_empty() {
            // Create the ASG.
            let pool_tag = aws_sdk_autoscaling::types::Tag::builder()
                .key("devbox:pool")
                .value(&config.pool_id)
                .resource_id(&config.name)
                .resource_type("auto-scaling-group")
                .propagate_at_launch(config.propagate_tags_at_launch)
                .build();

            let managed_tag = aws_sdk_autoscaling::types::Tag::builder()
                .key("devbox:managed-by")
                .value(&config.managed_by)
                .resource_id(&config.name)
                .resource_type("auto-scaling-group")
                .propagate_at_launch(config.propagate_tags_at_launch)
                .build();

            self.asg_client
                .create_auto_scaling_group()
                .auto_scaling_group_name(&config.name)
                .launch_template(lt_spec)
                .min_size(min_i32)
                .max_size(max_i32)
                .desired_capacity(desired_i32)
                .vpc_zone_identifier(&vpc_zone_identifier)
                .health_check_type("EC2")
                .health_check_grace_period(grace_i32)
                .tags(pool_tag)
                .tags(managed_tag)
                .send()
                .await
                .context("failed to create auto scaling group")?;
        } else {
            // Update the existing ASG.
            self.asg_client
                .update_auto_scaling_group()
                .auto_scaling_group_name(&config.name)
                .launch_template(lt_spec)
                .min_size(min_i32)
                .max_size(max_i32)
                .desired_capacity(desired_i32)
                .vpc_zone_identifier(&vpc_zone_identifier)
                .health_check_type("EC2")
                .health_check_grace_period(grace_i32)
                .send()
                .await
                .context("failed to update auto scaling group")?;
        }

        // When propagate_tags_at_launch is enabled, call CreateOrUpdateTags
        // to ensure all ASG-level tags propagate to instances.
        if config.propagate_tags_at_launch {
            let pool_tag = aws_sdk_autoscaling::types::Tag::builder()
                .key("devbox:pool")
                .value(&config.pool_id)
                .resource_id(&config.name)
                .resource_type("auto-scaling-group")
                .propagate_at_launch(true)
                .build();

            let managed_tag = aws_sdk_autoscaling::types::Tag::builder()
                .key("devbox:managed-by")
                .value(&config.managed_by)
                .resource_id(&config.name)
                .resource_type("auto-scaling-group")
                .propagate_at_launch(true)
                .build();

            self.asg_client
                .create_or_update_tags()
                .tags(pool_tag)
                .tags(managed_tag)
                .send()
                .await
                .context("failed to create or update ASG tags with propagation")?;
        }

        Ok(config.name.clone())
    }

    async fn ensure_lifecycle_hook(&self, config: &LifecycleHookConfig) -> Result<()> {
        let timeout_i32 = i32::try_from(config.heartbeat_timeout_secs)
            .context("heartbeat_timeout_secs exceeds i32")?;

        self.asg_client
            .put_lifecycle_hook()
            .auto_scaling_group_name(&config.asg_name)
            .lifecycle_hook_name(&config.hook_name)
            .lifecycle_transition("autoscaling:EC2_INSTANCE_LAUNCHING")
            .heartbeat_timeout(timeout_i32)
            .default_result("ABANDON")
            .send()
            .await
            .context("failed to put lifecycle hook")?;

        Ok(())
    }

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

        let (launch_template_id, launch_template_version) = group
            .launch_template()
            .map_or((None, None), |lt| {
                (
                    lt.launch_template_id().map(String::from),
                    lt.version().map(String::from),
                )
            });

        let instances = group
            .instances()
            .iter()
            .map(|inst| {
                AsgInstance {
                    instance_id: inst
                        .instance_id()
                        .unwrap_or_default()
                        .to_string(),
                    lifecycle_state: inst
                        .lifecycle_state()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default(),
                    health_status: inst
                        .health_status()
                        .unwrap_or_default()
                        .to_string(),
                    protected_from_scale_in: inst.protected_from_scale_in().unwrap_or(false),
                }
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

    async fn complete_lifecycle_action(
        &self,
        asg_name: &str,
        hook_name: &str,
        instance_id: &str,
        action_result: &str,
    ) -> Result<()> {
        self.asg_client
            .complete_lifecycle_action()
            .auto_scaling_group_name(asg_name)
            .lifecycle_hook_name(hook_name)
            .instance_id(instance_id)
            .lifecycle_action_result(action_result)
            .send()
            .await
            .context("failed to complete lifecycle action")?;

        Ok(())
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

/// Build the launch template request data from config.
fn build_launch_template_data(
    config: &LaunchTemplateConfig,
) -> Result<aws_sdk_ec2::types::RequestLaunchTemplateData> {
    // IMDSv2 enforcement
    let metadata_options =
        aws_sdk_ec2::types::LaunchTemplateInstanceMetadataOptionsRequest::builder()
            .http_tokens(aws_sdk_ec2::types::LaunchTemplateHttpTokensState::Required)
            .http_put_response_hop_limit(2)
            .build();

    // EBS encryption
    let ebs = aws_sdk_ec2::types::LaunchTemplateEbsBlockDeviceRequest::builder()
        .encrypted(true)
        .build();

    let block_device_mapping =
        aws_sdk_ec2::types::LaunchTemplateBlockDeviceMappingRequest::builder()
            .device_name("/dev/xvda")
            .ebs(ebs)
            .build();

    // Tags for instances
    let instance_tags = vec![
        aws_sdk_ec2::types::Tag::builder()
            .key("devbox:pool")
            .value(&config.pool_id)
            .build(),
        aws_sdk_ec2::types::Tag::builder()
            .key("devbox:managed-by")
            .value(&config.managed_by)
            .build(),
    ];

    let tag_spec = aws_sdk_ec2::types::LaunchTemplateTagSpecificationRequest::builder()
        .resource_type(aws_sdk_ec2::types::ResourceType::Instance)
        .set_tags(Some(instance_tags))
        .build();

    let mut builder = aws_sdk_ec2::types::RequestLaunchTemplateData::builder()
        .image_id(&config.ami_id)
        .metadata_options(metadata_options)
        .block_device_mappings(block_device_mapping)
        .tag_specifications(tag_spec)
        .set_security_group_ids(Some(config.security_group_ids.clone()));

    // When cpu > 0 and memory_mib > 0, use InstanceRequirements for flexible
    // instance type selection instead of a fixed instance_type.
    if config.cpu > 0 && config.memory_mib > 0 {
        let cpu_i32 = i32::try_from(config.cpu).context("cpu exceeds i32")?;
        let mem_i32 = i32::try_from(config.memory_mib).context("memory_mib exceeds i32")?;

        let vcpu_count = aws_sdk_ec2::types::VCpuCountRangeRequest::builder()
            .min(cpu_i32)
            .max(cpu_i32)
            .build();

        let memory_mib = aws_sdk_ec2::types::MemoryMiBRequest::builder()
            .min(mem_i32)
            .max(mem_i32)
            .build();

        let instance_requirements = aws_sdk_ec2::types::InstanceRequirementsRequest::builder()
            .v_cpu_count(vcpu_count)
            .memory_mib(memory_mib)
            .build();

        builder = builder.instance_requirements(instance_requirements);
    } else if !config.instance_type.is_empty() {
        builder = builder.instance_type(
            aws_sdk_ec2::types::InstanceType::from(config.instance_type.as_str()),
        );
    }

    Ok(builder.build())
}

/// Create a new launch template.
async fn create_launch_template(
    ec2: &Ec2,
    config: &LaunchTemplateConfig,
    template_data: aws_sdk_ec2::types::RequestLaunchTemplateData,
) -> Result<LaunchTemplateResult> {
    let lt_tag = aws_sdk_ec2::types::Tag::builder()
        .key("devbox:pool")
        .value(&config.pool_id)
        .build();
    let managed_tag = aws_sdk_ec2::types::Tag::builder()
        .key("devbox:managed-by")
        .value(&config.managed_by)
        .build();
    let tag_spec = aws_sdk_ec2::types::TagSpecification::builder()
        .resource_type(aws_sdk_ec2::types::ResourceType::LaunchTemplate)
        .tags(lt_tag)
        .tags(managed_tag)
        .build();

    let output = ec2
        .ec2_client
        .create_launch_template()
        .launch_template_name(&config.name)
        .launch_template_data(template_data)
        .tag_specifications(tag_spec)
        .send()
        .await
        .context("failed to create launch template")?;

    let lt = output
        .launch_template()
        .context("create launch template response missing template")?;
    let id = lt
        .launch_template_id()
        .context("launch template missing ID")?
        .to_string();
    let version = lt.latest_version_number().unwrap_or(1);

    Ok(LaunchTemplateResult { id, version })
}

/// Create a new version of an existing launch template.
async fn create_launch_template_version(
    ec2: &Ec2,
    template_id: &str,
    template_data: aws_sdk_ec2::types::RequestLaunchTemplateData,
) -> Result<LaunchTemplateResult> {
    let output = ec2
        .ec2_client
        .create_launch_template_version()
        .launch_template_id(template_id)
        .launch_template_data(template_data)
        .send()
        .await
        .context("failed to create launch template version")?;

    let version_info = output
        .launch_template_version()
        .context("create launch template version response missing version")?;
    let version = version_info.version_number().unwrap_or(1);

    Ok(LaunchTemplateResult {
        id: template_id.to_string(),
        version,
    })
}
