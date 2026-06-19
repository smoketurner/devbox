# Tasks: Infra / Control-Plane Boundary

## Task 1: Create the pool Terraform module skeleton

Create `modules/pool/` in the `devbox-infra` repository with the standard file layout:
`versions.tf`, `variables.tf`, `locals.tf`, `data.tf`, `main.tf`, `iam.tf`, `outputs.tf`.

### Sub-tasks

- [x] Create `modules/pool/versions.tf` with `required_version >= 1.0` and AWS provider `>= 6.0` (matching existing modules)
- [x] Create `modules/pool/variables.tf` with input variables: `name_prefix` (string), `environment` (string), `pool_id` (string), `vpc_id` (string), `subnet_ids` (list(string), validation: length > 0), `instance_type` (string, default "m5.large"), `min_size` (number, default 0), `max_size` (number, default 10), `health_check_type` (string, default "EC2"), `health_check_grace_period` (number, default 300), `warmup_heartbeat_timeout` (number, default 300), `security_group_ids` (list(string)), `ssm_ami_parameter` (string, default "/devbox/ami/latest"), `ebs_volume_size` (number, default 30), `ebs_encrypted` (bool, default true), `metadata_hop_limit` (number, default 2), `control_plane_role_name` (string, optional for external reference), `tags` (map(string), default {})
- [x] Create `modules/pool/locals.tf` with `local.name_prefix`, `local.aws_partition`, `local.aws_dns_suffix`, `local.aws_region`, `local.aws_account_id`, `local.asg_name` (`devbox-pool-${var.pool_id}`), `local.hook_name` (`devbox-warmup-${var.pool_id}`), and `local.tags` (merged from var.tags + module tags)
- [x] Create `modules/pool/data.tf` with `data.aws_caller_identity.current`, `data.aws_partition.current`, `data.aws_region.current`, and IAM policy documents for the control-plane role (assume role for EC2 service) and the scoped runtime policy

## Task 2: Create the Launch Template resource

Implement the Launch Template in `modules/pool/main.tf` per the design: SSM-resolved AMI, IMDSv2 enforced, instance metadata tags enabled, encrypted EBS, security groups, and base tags.

### Sub-tasks

- [x] Add `aws_launch_template` resource in `main.tf` with name `${local.name_prefix}-lt`, `image_id = "resolve:ssm:${var.ssm_ami_parameter}"`, metadata options (`http_tokens = "required"`, `http_put_response_hop_limit = var.metadata_hop_limit`, `http_endpoint = "enabled"`, `instance_metadata_tags = "enabled"`), EBS block device mapping (encrypted, gp3, configurable size), network interface with security groups, and `local.tags`
- [x] Set `instance_type` on the launch template from `var.instance_type`
- [x] Add `tag_specifications` for instances and volumes propagating `local.tags` plus a `Name` tag of `${local.asg_name}`

## Task 3: Create the Auto Scaling Group resource

Implement the ASG in `modules/pool/main.tf` with the naming contract, lifecycle ignore for desired_capacity, subnet distribution, health check, and propagate_tags_at_launch.

### Sub-tasks

- [x] Add `aws_autoscaling_group` resource with `name = local.asg_name`, launch template reference (id + `$Latest` version), `min_size = var.min_size`, `max_size = var.max_size`, `desired_capacity = var.min_size`, `vpc_zone_identifier = var.subnet_ids`, `health_check_type = var.health_check_type`, `health_check_grace_period = var.health_check_grace_period`
- [x] Add `lifecycle { ignore_changes = [desired_capacity] }` block so Terraform never reverts the reconciler's capacity changes
- [x] Add ASG tags using `dynamic "tag"` block iterating over `local.tags` with `propagate_at_launch = true`, plus a `Name` tag

## Task 4: Create the warm-up lifecycle hook

Attach the lifecycle hook to the ASG with the deterministic name from the naming contract.

### Sub-tasks

- [x] Add `aws_autoscaling_lifecycle_hook` resource with `name = local.hook_name`, `autoscaling_group_name = aws_autoscaling_group.pool.name`, `lifecycle_transition = "autoscaling:EC2_INSTANCE_LAUNCHING"`, `heartbeat_timeout = var.warmup_heartbeat_timeout`, `default_result = "ABANDON"`

## Task 5: Create the control-plane IAM role

Implement the scoped IAM role in `modules/pool/iam.tf` with only the runtime actions: DescribeAutoScalingGroups, SetDesiredCapacity, TerminateInstanceInAutoScalingGroup, SetInstanceProtection, CompleteLifecycleAction, and ec2:CreateTags.

### Sub-tasks

- [x] Add assume-role policy document in `data.tf` allowing EC2 service (or the control-plane server's execution environment) to assume the role
- [x] Add the runtime permissions IAM policy document in `data.tf` scoped to `autoscaling:DescribeAutoScalingGroups`, `autoscaling:SetDesiredCapacity`, `autoscaling:TerminateInstanceInAutoScalingGroup`, `autoscaling:SetInstanceProtection`, `autoscaling:CompleteLifecycleAction`, and `ec2:CreateTags` — resource-scoped to the ASG ARN where possible
- [x] Add `aws_iam_role` resource in `iam.tf` with name `${local.name_prefix}-control-plane` and the assume-role policy
- [x] Add `aws_iam_role_policy` inline policy attachment with the runtime permissions document
- [x] Add `aws_iam_instance_profile` if the control plane runs on EC2

## Task 6: Create module outputs

Define outputs in `modules/pool/outputs.tf` exposing the key resource identifiers needed by other modules and the control plane.

### Sub-tasks

- [x] Output `asg_name` (the deterministic ASG name)
- [x] Output `asg_arn` (ASG ARN)
- [x] Output `launch_template_id` and `launch_template_arn`
- [x] Output `lifecycle_hook_name`
- [x] Output `control_plane_role_arn` and `control_plane_role_name`
- [x] Output `control_plane_instance_profile_name` (if created)

## Task 7: Wire the pool module into the dev environment

Add a `module "pool"` call in `environments/dev/main.tf` passing the VPC subnets, security groups, pool_id, and tags.

### Sub-tasks

- [x] Add `module "pool"` block in `environments/dev/main.tf` sourcing `../../modules/pool`, passing `name_prefix = "devbox-${local.environment}"`, `environment = local.environment`, `pool_id = "default"`, `vpc_id = module.vpc.vpc_id`, `subnet_ids = module.vpc.private_subnets`, `security_group_ids = []` (placeholder, can be populated later), and `tags = local.tags`
- [x] Verify the module compiles with `terraform validate` (no provider initialization needed for structure check)
