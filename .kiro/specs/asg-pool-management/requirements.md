# Requirements Document

## Introduction

Replace the current direct EC2 RunInstances/TerminateInstances approach in the pool reconciler with an Auto Scaling Group (ASG) backed by a Launch Template. The ASG manages the fleet of devbox instances, and the reconciler shifts from instance-level CRUD to pool-level capacity management. Claiming an instance means tagging one from the ASG's available pool, and DesiredCapacity is computed as claimed_count + target_warm_pool_size.

> **Provisioning boundary update.** The Launch Template, ASG, and warm-up lifecycle hook
> are now **provisioned by Terraform** in `smoketurner/devbox-infra`; the reconciler
> **adopts** them by name and manages only runtime capacity/per-instance state. See
> [`../infra-boundary/`](../infra-boundary/). Requirements 1 and 2 below have been updated
> from "create/update" to "adopt"; the create-path language is retained struck-through in
> spirit only where it documents the resulting AWS configuration that Terraform must match.

## Glossary

- **Reconciler**: The background loop that periodically adjusts the devbox pool to maintain desired state
- **ASG**: An AWS Auto Scaling Group that manages the lifecycle and count of EC2 instances
- **Launch_Template**: An AWS Launch Template that defines instance configuration (AMI, instance type, security groups, IMDSv2 settings, tags)
- **DesiredCapacity**: The target number of instances the ASG should maintain, computed as claimed_count + target_warm_pool_size
- **Warm_Pool_Size**: The configured number of unclaimed instances that should be kept in Ready state for immediate claim
- **Compute_Trait**: The Rust trait abstracting compute operations for the reconciler
- **DevboxDoc**: The document stored in DocumentStore tracking individual devbox instance metadata and state
- **Lifecycle_Hook**: An ASG lifecycle hook that pauses an instance at launch until a warm-up process completes
- **Claimed_Instance**: An instance from the ASG that has been assigned to a user via tagging
- **Available_Instance**: An instance in the ASG that is running, warm, and not claimed by any user

## Requirements

### Requirement 1: Launch Template (Terraform-provisioned, reconciler-agnostic)

**User Story:** As a platform operator, I want instance configuration defined in a Launch Template provisioned by Terraform, so that all devbox instances launch consistently and the control plane needs no Launch Template permissions.

#### Acceptance Criteria

1. THE Launch Template SHALL be provisioned by Terraform (`devbox-infra`), not by the Reconciler; the Reconciler SHALL NOT create, update, or version the Launch Template.
2. THE Launch Template SHALL resolve its AMI from SSM (`ImageId = resolve:ssm:/devbox/ami/latest`) so launches use the current AMI without a template edit.
3. THE Launch Template SHALL enforce IMDSv2 (HttpTokens "required", HttpPutResponseHopLimit 2) and enable instance metadata tags (`InstanceMetadataTags=enabled`, required by `../ssh-access/`).
4. THE Launch Template SHALL configure EBS volumes with encryption enabled and attach the pool security groups and base cost-tracking tags ("devbox:pool", "devbox:managed-by").
5. THE ASG SHALL reference a pinned Launch Template version managed by Terraform (not "$Latest"/"$Default"); the Reconciler SHALL NOT change the referenced version.

> These criteria describe the AWS configuration Terraform must produce; see
> [`../infra-boundary/requirements.md`](../infra-boundary/requirements.md) Requirement 1.

### Requirement 2: Auto Scaling Group (Terraform-provisioned, reconciler-adopted)

**User Story:** As a platform operator, I want the reconciler to adopt a Terraform-managed ASG, so AWS handles health checks, AZ distribution, and replacement while the two systems never fight over structure.

#### Acceptance Criteria

1. THE ASG SHALL be created and configured by Terraform with MinSize, MaxSize, subnet/AZ distribution, EC2 health check + 300s grace period, `propagate_tags_at_launch=true`, and `lifecycle { ignore_changes = [desired_capacity] }`.
2. WHEN the Reconciler runs, THE Reconciler SHALL adopt the ASG by its deterministic name (`devbox-pool-<pool_id>`) via describe, and SHALL NOT create or structurally update the ASG.
3. IF the ASG does not exist (Terraform not yet applied), THEN THE Reconciler SHALL log a warning and skip the tick without error, retrying on the next tick.
4. THE Reconciler SHALL read MaxSize from the adopted ASG and clamp desired capacity to it, rather than carrying a configured max_pool_size.
5. THE Reconciler SHALL NOT require ami_id, instance_type, cpu, memory_mib, subnet_ids, security_group_ids, or max_pool_size in its configuration; these belong to Terraform.
6. THE warm-up lifecycle hook (`devbox-warmup-<pool_id>`) SHALL be created by Terraform; the Reconciler SHALL only complete lifecycle actions against it, never create or update it.

### Requirement 3: Capacity Reconciliation

**User Story:** As a platform operator, I want the reconciler to adjust ASG DesiredCapacity based on claimed count and warm pool target, so that the pool always has the right number of available instances without managing individual launches.

#### Acceptance Criteria

1. WHEN a reconciliation tick runs, THE Reconciler SHALL compute the desired capacity as the sum of claimed_count (the number of DevboxDoc records in Claimed state) and target_warm_pool_size
2. WHEN the computed desired capacity differs from the current ASG DesiredCapacity, THE Reconciler SHALL call UpdateAutoScalingGroup to set the new DesiredCapacity
3. WHEN a reconciliation tick computes the desired capacity, THE Reconciler SHALL clamp the value to the inclusive range between ASG MinSize and MaxSize before applying
4. IF the UpdateAutoScalingGroup call fails, THEN THE Reconciler SHALL log the error and retry on the next reconciliation tick without modifying the current ASG DesiredCapacity
5. WHEN a reconciliation tick runs, THE Reconciler SHALL compare ASG instance membership against DevboxDoc records and delete any DevboxDoc whose instance_id is no longer present in the ASG
6. THE Reconciler SHALL enable scale-in protection on instances that are in Claimed state so that the ASG does not terminate actively claimed instances during scale-down
7. IF the computed desired capacity exceeds ASG MaxSize after clamping, THEN THE Reconciler SHALL log a warning indicating the pool cannot satisfy the full claimed_count plus target_warm_pool_size

### Requirement 4: Instance Claiming

**User Story:** As a coding agent, I want to claim a warm instance from the ASG pool, so that I get an isolated development environment without waiting for provisioning.

#### Acceptance Criteria

1. WHEN a claim request is received with a non-empty owner identifier of at most 256 characters, THE Reconciler SHALL select the longest-waiting Available_Instance from the ASG pool (preferring a matching instance_type if specified in the request) and transition its DevboxDoc state to Claimed using compare_and_update to prevent concurrent assignment
2. WHEN an instance is claimed, THE Reconciler SHALL apply a "devbox:owner" tag to the EC2 instance with the claiming user's identifier
3. IF the EC2 tagging operation fails after the DevboxDoc has been updated to Claimed, THEN THE Reconciler SHALL retain the Claimed state in the DevboxDoc and retry the tagging on the next reconciliation tick
4. WHEN an instance is claimed, THE Reconciler SHALL update the DevboxDoc state to Claimed and record the owner and claimed_at timestamp
5. IF no Available_Instance exists when a claim is received, THEN THE Reconciler SHALL return an error indicating the pool is exhausted
6. IF a concurrent claim request targets the same Available_Instance and the compare_and_update fails, THEN THE Reconciler SHALL retry selection with the next Available_Instance before returning a pool-exhausted error
7. WHEN an instance is claimed, THE Reconciler SHALL recompute desired capacity as claimed_count plus target_warm_pool_size on the next reconciliation tick to maintain warm pool size

### Requirement 5: Instance Release and Termination

**User Story:** As a coding agent, I want to release a claimed instance so that it is terminated and replaced, maintaining pool health.

#### Acceptance Criteria

1. WHEN a release request containing the owner identifier is received for a Claimed_Instance, THE Reconciler SHALL verify that the requesting owner matches the current DevboxDoc owner and transition the DevboxDoc state to Terminating
2. IF a release request is received for an instance that is not in Claimed state or the owner does not match, THEN THE Reconciler SHALL reject the request with an error indicating the reason for rejection
3. WHEN a DevboxDoc is in Terminating state and has an instance_id, THE Reconciler SHALL call TerminateInstanceInAutoScalingGroup with ShouldDecrementDesiredCapacity set to false
4. WHEN TerminateInstanceInAutoScalingGroup succeeds, THE Reconciler SHALL delete the corresponding DevboxDoc from the DocumentStore
5. IF the TerminateInstanceInAutoScalingGroup call fails, THEN THE Reconciler SHALL log the error and retry on the next reconciliation tick
6. WHEN a DevboxDoc is in Terminating state and has no instance_id, THE Reconciler SHALL delete the DevboxDoc from the DocumentStore without calling TerminateInstanceInAutoScalingGroup

### Requirement 6: Lifecycle Hook for Warm-Up

**User Story:** As a platform operator, I want a lifecycle hook to hold newly launched instances until warm-up completes, so that only fully initialized instances become available for claiming.

#### Acceptance Criteria

1. WHEN the ASG is created, THE Reconciler SHALL attach a lifecycle hook named with the pool identifier suffix, with transition "autoscaling:EC2_INSTANCE_LAUNCHING" and heartbeat timeout set to the value of the lifecycle_hook_timeout configuration field (default 300 seconds, minimum 60 seconds, maximum 3600 seconds)
2. WHEN the Reconciler detects a new instance in the ASG that is in the Pending:Wait lifecycle state, THE Reconciler SHALL create a DevboxDoc with state Warming and record the instance ID
3. WHEN the instance reports warm-up completion via a successful health check endpoint response on the instance, THE Reconciler SHALL call CompleteLifecycleAction with the lifecycle hook name, instance ID, and result "CONTINUE"
4. IF the CompleteLifecycleAction call fails, THEN THE Reconciler SHALL log the error and retry on the next reconciliation tick while the DevboxDoc remains in Warming state
5. IF the lifecycle hook heartbeat timeout expires before warm-up completes, THEN THE Reconciler SHALL remove the corresponding DevboxDoc from the DocumentStore when the instance is no longer present in the ASG
6. WHEN CompleteLifecycleAction succeeds and the instance transitions to InService, THE Reconciler SHALL update the DevboxDoc state from Warming to Ready

### Requirement 7: Simplified Compute Trait

**User Story:** As a developer, I want the Compute trait to operate at pool level rather than instance level, so that the reconciler code is simpler and ASG-aware.

#### Acceptance Criteria

1. THE Compute_Trait SHALL expose a method to ensure a Launch Template exists with the specified AMI ID, instance type, cpu, memory, security group IDs, and metadata options, returning the Launch Template ID and version number on success
2. THE Compute_Trait SHALL expose a method to ensure an ASG exists with the specified Launch Template ID and version, subnet IDs, min size, max size, and desired capacity, returning the ASG name on success
3. THE Compute_Trait SHALL expose a method to set the DesiredCapacity of a named ASG, accepting the ASG name and new capacity value
4. THE Compute_Trait SHALL expose a method to terminate a specific instance within a named ASG by instance ID without decrementing desired capacity
5. THE Compute_Trait SHALL expose a method to complete a lifecycle action for a named lifecycle hook and instance ID with a specified result string
6. THE Compute_Trait SHALL expose a method to describe a named ASG, returning a list of instance records each containing instance ID, lifecycle state, and health status
7. THE Compute_Trait SHALL expose a method to apply a set of key-value tag pairs to an instance by instance ID

### Requirement 8: DocumentStore Integration

**User Story:** As a platform operator, I want the DocumentStore to track which ASG instances are claimed vs available, so that the system maintains accurate state across reconciler restarts.

#### Acceptance Criteria

1. WHEN a reconciliation tick runs, THE Reconciler SHALL compare the set of instance IDs reported by the ASG with the set of instance IDs referenced in DevboxDoc records and create, update, or delete DevboxDoc records so that the DocumentStore matches actual ASG membership
2. WHEN a new instance appears in the ASG and completes warm-up, THE Reconciler SHALL create a DevboxDoc with state Ready, the instance's EC2 instance ID, instance type, AMI ID, and subnet ID populated from the ASG instance metadata
3. WHEN an instance disappears from the ASG, THE Reconciler SHALL delete the corresponding DevboxDoc from the DocumentStore within the same reconciliation tick
4. THE Reconciler SHALL use compare_and_update with the document's current version number to prevent concurrent modifications to DevboxDoc records
5. IF a compare_and_update call returns a version conflict, THEN THE Reconciler SHALL skip the update for that document and retry on the next reconciliation tick
6. WHEN the Reconciler starts, THE Reconciler SHALL delete any DevboxDoc records whose instance ID is not present in the current ASG instance membership list

### Requirement 9: Configuration

**User Story:** As a platform operator, I want to configure the ASG pool parameters, so that I can tune warm pool size, instance limits, and timeouts for my workload.

#### Acceptance Criteria

1. THE ReconcilerConfig SHALL include a target_warm_pool_size field specifying the number of unclaimed Ready instances to maintain, accepting an integer value between 1 and 100 inclusive
2. THE ReconcilerConfig SHALL include a max_pool_size field specifying the MaxSize for the ASG, accepting an integer value between 1 and 500 inclusive that is greater than or equal to target_warm_pool_size
3. THE ReconcilerConfig SHALL include a lifecycle_hook_timeout field specifying the heartbeat timeout for the warm-up lifecycle hook, accepting a duration between 60 seconds and 7200 seconds inclusive
4. THE ReconcilerConfig SHALL include a subnet_ids field containing a non-empty list of at least 1 and at most 20 subnet IDs for multi-AZ distribution
5. THE ReconcilerConfig SHALL include a security_group_ids field containing a non-empty list of at least 1 and at most 5 security group IDs to apply via the Launch Template
6. THE ReconcilerConfig SHALL retain the existing polling_interval, instance_type, ami_id, stuck_threshold, lock_ttl, and server_id fields with their current default values
7. THE ReconcilerConfig SHALL include a cpu field specifying the number of vCPUs required for pool instances, accepting a positive integer value, to support flexible instance type selection
8. THE ReconcilerConfig SHALL include a memory field specifying the amount of memory in MiB required for pool instances, accepting a positive integer value, to support flexible instance type selection
9. IF target_warm_pool_size exceeds max_pool_size, THEN THE ReconcilerConfig SHALL reject the configuration with an error indicating that target_warm_pool_size must not exceed max_pool_size
10. IF any required field is missing or contains an out-of-range value, THEN THE ReconcilerConfig SHALL reject the configuration with an error indicating the invalid field and the accepted range
