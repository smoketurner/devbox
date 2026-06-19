# Requirements Document: Infra / Control-Plane Boundary

**Status:** Active

## Introduction

The devbox system spans two repositories: **`smoketurner/devbox-infra`** (Terraform
for the AWS foundation) and this repo (`devbox`, the control plane + CLI). This spec
defines the ownership boundary for the **Launch Template, Auto Scaling Group, and
warm-up lifecycle hook**: Terraform *provisions* them as static infrastructure, and
the control-plane reconciler *adopts and operates* them at runtime.

This supersedes the earlier model (in `asg-pool-management/`) where the reconciler
created and updated these resources itself. Moving creation to Terraform gives
reproducible, auditable infrastructure; lets the control plane run with least
privilege (no `Create*`/`Update*` infra permissions); and enables a clean,
event-driven AMI rotation that does not disrupt in-use devboxes.

## Glossary

- **Terraform**: the IaC in `devbox-infra` that provisions AWS resources.
- **Reconciler**: the control-plane background loop (`devbox-server`).
- **Adopt**: look up an existing resource by deterministic name and operate on it,
  rather than creating it.
- **Runtime ops**: capacity and per-instance state changes that vary with claims —
  desired capacity, scale-in protection, owner tags, termination, lifecycle completion.

## Requirements

### Requirement 1: Terraform provisions static infrastructure

**User Story:** As a platform operator, I want the Launch Template, ASG, and lifecycle
hook defined in Terraform, so that the fleet's configuration is reproducible and
auditable and the control plane needs no infra-creation permissions.

#### Acceptance Criteria

1. THE Terraform configuration SHALL create the Launch Template, ASG, and warm-up
   lifecycle hook for each pool, using the deterministic names in Requirement 3.
2. THE Launch Template SHALL set `ImageId` to resolve from SSM
   (`resolve:ssm:/devbox/ami/latest`) so new launches use the current AMI without a
   template edit; AND SHALL enforce IMDSv2 (`HttpTokens=required`, hop limit 2), enable
   instance metadata tags (`InstanceMetadataTags=enabled`), encrypt EBS at rest, and
   attach the pool security groups and base tags.
3. THE ASG SHALL define `MinSize`, `MaxSize`, subnet/AZ distribution, health check type
   and grace period, and `propagate_tags_at_launch=true`.
4. THE ASG SHALL declare `lifecycle { ignore_changes = [desired_capacity] }` so Terraform
   never reverts the reconciler's capacity decisions.
5. THE warm-up lifecycle hook SHALL be attached to the ASG with the name in Requirement 3
   and a heartbeat timeout, launching new instances into a wait state until warm-up
   completes.
6. THE Terraform configuration SHALL define the control-plane IAM role scoped to the
   runtime actions in Requirement 4 only (no `Create*`/`Update*` LT/ASG/hook actions).

### Requirement 2: Control plane adopts, does not create

**User Story:** As an operator, I want the reconciler to attach to the Terraform-managed
ASG, so the two systems never fight over structure.

#### Acceptance Criteria

1. THE Reconciler SHALL locate the ASG by its deterministic name (`describe_asg`) and
   operate on it; it SHALL NOT create or update the Launch Template, ASG, or lifecycle hook.
2. IF the ASG does not exist (Terraform not yet applied), THEN the Reconciler SHALL log a
   warning and skip the tick without error, retrying on the next tick.
3. THE Reconciler SHALL read `MaxSize` from the adopted ASG and clamp desired capacity to
   it, rather than carrying a configured maximum.
4. THE Reconciler SHALL NOT require `ami_id`, `instance_type`, `cpu`, `memory_mib`,
   `subnet_ids`, `security_group_ids`, or `max_pool_size` in its configuration; these are
   owned by Terraform.

### Requirement 3: Shared naming contract

**User Story:** As an operator, I want Terraform and the control plane to agree on resource
names, so adoption is deterministic.

#### Acceptance Criteria

1. THE ASG name SHALL be `devbox-pool-<pool_id>`.
2. THE warm-up lifecycle hook name SHALL be `devbox-warmup-<pool_id>`.
3. Both sides SHALL derive these names from the same `pool_id`; the control plane receives
   `pool_id` (and therefore the ASG/hook names) via configuration.

### Requirement 4: Runtime ownership (control plane)

**User Story:** As an operator, I want claim-driven, dynamic behavior to stay in the
control plane, since Terraform cannot track it.

#### Acceptance Criteria

1. THE Reconciler SHALL own and continuously set the ASG `DesiredCapacity` as
   `min(claimed_count + target_warm_pool_size, asg.max_size)`.
2. THE Reconciler SHALL set scale-in protection on `Claimed` instances and remove it
   otherwise.
3. THE Reconciler SHALL apply the `devbox:owner` tag to claimed instances, terminate
   released instances, and complete the warm-up lifecycle action for instances that
   finish warming.
4. THE control-plane IAM role SHALL be limited to:
   `autoscaling:DescribeAutoScalingGroups`, `SetDesiredCapacity`,
   `TerminateInstanceInAutoScalingGroup`, `SetInstanceProtection`,
   `CompleteLifecycleAction`, and `ec2:CreateTags`.

## Out of Scope

- The AMI rotation automation itself (EventBridge + instance refresh) — see
  `../ami-image-builder/requirements.md`.
- The control-plane code refactor that removes the create paths — tracked as a follow-up
  against `asg-pool-management/`.
