# Implementation Plan: ASG Pool Management

## Overview

Replace the current direct EC2 RunInstances/TerminateInstances approach with an ASG-backed pool. The implementation proceeds in layers: first update dependencies and types, then redesign the Compute trait, implement the production and mock clients, rewrite the reconciliation tick, update claiming/releasing logic, and finally wire everything together with tests.

## Tasks

- [x] 1. Add dependencies and update shared types
  - [x] 1.1 Add `aws-sdk-autoscaling` dependency to workspace and devbox-server
    - Add `aws-sdk-autoscaling` with pinned version to `[workspace.dependencies]` in root `Cargo.toml` with `default-features = false`
    - Add `aws-sdk-autoscaling = { workspace = true, features = ["rt-tokio", "default-https-client", "behavior-version-latest"] }` to `crates/devbox-server/Cargo.toml` under `[dependencies]`
    - _Requirements: 7.2, 7.3, 7.4, 7.5, 7.6_

  - [x] 1.2 Add `SecurityGroupId` type to `devbox-common`
    - Add a new `SecurityGroupId(pub String)` newtype in `crates/devbox-common/src/lib.rs` with the same derives and impls as `SubnetId` (Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Display, From<String>, AsRef<str>)
    - _Requirements: 9.5_

  - [x] 1.3 Add `owner_tag_applied` field to `DevboxDoc`
    - Add `pub owner_tag_applied: bool` field to `DevboxDoc` in `crates/devbox-server/src/documents/devbox.rs`
    - Default to `false` in all existing construction sites
    - Update existing tests to include the new field
    - _Requirements: 4.2, 4.3_

- [x] 2. Redesign the Compute trait and types
  - [x] 2.1 Define new Compute trait types in `crates/devbox-server/src/compute/mod.rs`
    - Replace the existing `InstanceStatus` enum and `Compute` trait with the new pool-level design
    - Add structs: `LaunchTemplateResult`, `AsgInstance`, `AsgDescription`, `LaunchTemplateConfig`, `AsgConfig`, `LifecycleHookConfig`
    - `LaunchTemplateConfig` must include `cpu: u32` and `memory_mib: u32` fields for flexible instance type selection via InstanceRequirements
    - `AsgConfig` must include `propagate_tags_at_launch: bool` field to control whether ASG tags are propagated to instances at launch
    - Define the new `Compute` trait with methods: `ensure_launch_template`, `ensure_asg`, `ensure_lifecycle_hook`, `set_desired_capacity`, `describe_asg`, `terminate_instance_in_asg`, `complete_lifecycle_action`, `tag_instance`, `set_scale_in_protection`
    - Add `use std::future::Future;` import
    - _Requirements: 1.1, 1.7, 2.8, 7.1, 7.2, 7.3, 7.4, 7.5, 7.6, 7.7, 9.7, 9.8_

  - [x] 2.2 Implement production `Ec2` struct with both clients
    - Rewrite `crates/devbox-server/src/compute/ec2.rs` to hold both `aws_sdk_ec2::Client` and `aws_sdk_autoscaling::Client`
    - Implement all new `Compute` trait methods using the AWS SDK
    - `ensure_launch_template`: DescribeLaunchTemplates → CreateLaunchTemplate or CreateLaunchTemplateVersion with IMDSv2, EBS encryption, tags. When `cpu` and `memory_mib` are set in `LaunchTemplateConfig`, configure `InstanceRequirements` with `VCpuCount` (min/max = cpu) and `MemoryMiB` (min/max = memory_mib) for flexible instance type selection instead of a fixed instance_type
    - `ensure_asg`: DescribeAutoScalingGroups → CreateAutoScalingGroup or UpdateAutoScalingGroup with EC2 health check, 300s grace period. When `propagate_tags_at_launch` is true, call CreateOrUpdateTags with `PropagateAtLaunch = true` for all ASG-level tags (pool_id, managed_by)
    - `ensure_lifecycle_hook`: PutLifecycleHook (idempotent)
    - `set_desired_capacity`: UpdateAutoScalingGroup
    - `describe_asg`: DescribeAutoScalingGroups returning AsgDescription
    - `terminate_instance_in_asg`: TerminateInstanceInAutoScalingGroup
    - `complete_lifecycle_action`: CompleteLifecycleAction
    - `tag_instance`: CreateTags
    - `set_scale_in_protection`: SetInstanceProtection
    - _Requirements: 1.1, 1.2, 1.3, 1.5, 1.6, 1.7, 2.1, 2.2, 2.3, 2.4, 2.6, 2.8, 3.2, 3.6, 4.2, 5.3, 6.1, 6.3, 7.1, 7.2, 7.3, 7.4, 7.5, 7.6, 7.7, 9.7, 9.8_

  - [x] 2.3 Implement new `MockCompute` for testing
    - Rewrite `crates/devbox-server/src/compute/mock.rs` with internal `MockAsgState` (launch template, ASG, instances HashMap)
    - Track launch template config (including cpu/memory_mib), ASG state (including propagate_tags_at_launch setting), per-instance lifecycle state, tags, scale-in protection
    - When `propagate_tags_at_launch` is true in `AsgConfig`, mock should record that tag propagation is enabled and apply ASG-level tags to new instances
    - Provide test helpers: `add_instance(lifecycle_state)`, `set_instance_lifecycle_state(id, state)`, `set_error(method, error)`, `get_instance_tags(id)`, `get_propagate_tags_at_launch()`
    - All trait methods operate on internal state without real AWS calls
    - _Requirements: 2.8, 7.1, 7.2, 7.3, 7.4, 7.5, 7.6, 7.7, 9.7, 9.8_

- [x] 3. Checkpoint - Ensure the project compiles
  - Ensure all tests pass, ask the user if questions arise.

- [x] 4. Redesign ReconcilerConfig with validation
  - [x] 4.1 Rewrite `ReconcilerConfig` in `crates/devbox-server/src/reconcile/config.rs`
    - Add new fields: `pool_id`, `subnet_ids: Vec<SubnetId>`, `security_group_ids: Vec<SecurityGroupId>`, `target_warm_pool_size`, `max_pool_size`, `lifecycle_hook_timeout: Duration`, `cpu: u32`, `memory_mib: u32`
    - The `cpu` field specifies the number of vCPUs required for pool instances (used for InstanceRequirements-based flexible instance selection)
    - The `memory_mib` field specifies the amount of memory in MiB required for pool instances (used for InstanceRequirements-based flexible instance selection)
    - Rename `target_pool_size` to `target_warm_pool_size`
    - Replace single `subnet_id` with `subnet_ids: Vec<SubnetId>`
    - Retain existing fields: `polling_interval`, `instance_type`, `ami_id`, `stuck_threshold`, `lock_ttl`, `server_id`
    - Add `validate() -> Result<()>` method enforcing: subnet_ids 1..=20, security_group_ids 1..=5, target_warm_pool_size 1..=100, max_pool_size 1..=500 and >= target_warm_pool_size, lifecycle_hook_timeout 60..=7200s
    - Add helper methods: `asg_name()`, `launch_template_name()`, `lifecycle_hook_name()`, `lifecycle_hook_timeout_secs()`, `to_launch_template_config()` (which passes cpu and memory_mib to `LaunchTemplateConfig`)
    - _Requirements: 1.1, 1.7, 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 9.7, 9.8_

  - [ ]* 4.2 Write property test for configuration validation
    - **Property 14: Configuration Validation**
    - Generate random config values across full ranges using proptest
    - Verify `validate()` returns Ok if and only if all constraints hold simultaneously
    - Verify `validate()` returns Err identifying the invalid field when any constraint is violated
    - Place tests in `crates/devbox-server/src/reconcile/config.rs` or a dedicated test module
    - **Validates: Requirements 9.1, 9.2, 9.3, 9.4, 9.5, 9.7, 9.8**

- [x] 5. Implement the reconciliation tick for ASG model
  - [x] 5.1 Add pure `compute_desired_capacity` function
    - Create the pure function in `crates/devbox-server/src/reconcile/tick.rs`: `fn compute_desired_capacity(claimed_count: u32, target_warm_pool_size: u32, max_pool_size: u32) -> u32`
    - Formula: `min(claimed_count.saturating_add(target_warm_pool_size), max_pool_size)`
    - _Requirements: 3.1, 3.3_

  - [ ]* 5.2 Write property test for capacity computation
    - **Property 5: Capacity Computation**
    - Generate random triples `(claimed_count, target_warm_pool_size, max_pool_size)` with proptest
    - Verify result equals `min(claimed + warm, max)` and is always in `[0, max_pool_size]`
    - **Validates: Requirements 3.1, 3.3, 4.7**

  - [x] 5.3 Rewrite `reconciliation_tick` for ASG-based flow
    - Replace existing tick logic in `crates/devbox-server/src/reconcile/tick.rs`
    - Step 1: `ensure_launch_template` (abort tick on failure)
    - Step 2: Compute initial desired from DB state
    - Step 3: `ensure_asg` (abort tick on failure)
    - Step 4: `ensure_lifecycle_hook`
    - Step 5: `describe_asg` to get current instances
    - Step 6: Sync DevboxDoc records with ASG membership (create/delete)
    - Step 7: Handle Warming instances (complete_lifecycle_action for InService ones)
    - Step 8: Handle Terminating instances (terminate_instance_in_asg + delete doc)
    - Step 9: Recompute desired capacity and update if changed
    - Step 10: Update scale-in protection (enable for Claimed, disable for others)
    - Step 11: Apply pending owner tags (for docs with owner_tag_applied=false)
    - Log warning if computed desired exceeds max_pool_size
    - _Requirements: 1.1, 1.4, 2.1, 2.2, 2.5, 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 4.2, 4.3, 5.3, 5.4, 5.5, 5.6, 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 8.1, 8.2, 8.3, 8.4, 8.5, 8.6_

  - [ ]* 5.4 Write property test for Document-ASG sync invariant
    - **Property 6: Document-ASG Sync Invariant**
    - Generate random ASG instance sets and DevboxDoc record sets
    - After sync step: every DevboxDoc instance_id must be in ASG set, and every Pending:Wait/InService instance must have a corresponding DevboxDoc
    - **Validates: Requirements 3.5, 6.5, 8.1, 8.3, 8.6**

- [x] 6. Update claim and release logic
  - [x] 6.1 Update `claim_devbox` handler in `crates/devbox-server/src/routes.rs`
    - Add owner length validation (non-empty, at most 256 characters)
    - Sort Ready candidates by `created_at` ascending (longest-waiting first), then prefer matching instance_type
    - Use `compare_and_update` for optimistic concurrency on claim
    - Set `owner_tag_applied = false` on claim (tagging deferred to reconciler tick)
    - On all candidates failing compare_and_update, return pool-exhausted error
    - _Requirements: 4.1, 4.4, 4.5, 4.6, 4.7_

  - [x] 6.2 Update `release_devbox` handler in `crates/devbox-server/src/routes.rs`
    - Verify requesting owner matches current DevboxDoc owner
    - Reject if doc is not in Claimed state or owner mismatch (with reason in error)
    - Transition state to Terminating using `compare_and_update`
    - _Requirements: 5.1, 5.2_

  - [ ]* 6.3 Write property test for claim selects longest-waiting
    - **Property 8: Claim Selects Longest-Waiting Instance**
    - Generate sets of Ready DevboxDocs with distinct `created_at` timestamps
    - Verify claim selects the doc with earliest `created_at`
    - Verify after claim: state=Claimed, owner set, claimed_at is Some
    - **Validates: Requirements 4.1, 4.4**

  - [ ]* 6.4 Write property test for claim retry on conflict
    - **Property 9: Claim Retry on Conflict**
    - Generate N Ready docs, simulate K compare_and_update failures
    - Verify claim succeeds by selecting (K+1)th candidate
    - Verify claim fails with pool-exhausted only when all N candidates fail
    - **Validates: Requirements 4.6**

  - [ ]* 6.5 Write property test for release ownership verification
    - **Property 10: Release Ownership Verification**
    - Generate random owner pairs (current_owner, requesting_owner)
    - Verify release succeeds iff owners match and state is Claimed
    - Verify rejection otherwise with appropriate error
    - **Validates: Requirements 5.1, 5.2**

- [x] 7. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Wire everything together and update main
  - [x] 8.1 Update `main.rs` to load new config fields and construct updated `ReconcilerConfig`
    - Load new env vars: `POOL_ID`, `POOL_SUBNET_IDS` (comma-separated), `POOL_SECURITY_GROUP_IDS` (comma-separated), `POOL_TARGET_WARM_SIZE`, `POOL_MAX_SIZE`, `POOL_LIFECYCLE_HOOK_TIMEOUT_SECS`, `POOL_CPU`, `POOL_MEMORY_MIB`
    - Parse `POOL_CPU` as u32 for vCPU count and `POOL_MEMORY_MIB` as u32 for memory in MiB; these feed into `ReconcilerConfig.cpu` and `ReconcilerConfig.memory_mib` for flexible instance type selection
    - Construct `ReconcilerConfig` with all new fields including `cpu` and `memory_mib`
    - Call `config.validate()` at startup, exit with error if invalid
    - Pass `aws_config` to `Ec2::new` (which now creates both ec2 and asg clients)
    - _Requirements: 1.1, 1.7, 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 9.7, 9.8_

  - [x] 8.2 Update `PoolMetricsResponse` and pool metrics handler
    - Remove `launching` count from metrics (Launching state is deprecated in ASG model)
    - Rename `target_pool_size` to `target_warm_pool_size` in response
    - Adjust `ready_delta` computation to use `target_warm_pool_size`
    - _Requirements: 3.1_

  - [ ]* 8.3 Write integration tests for full reconciliation tick
    - Test multi-tick lifecycle: Warming → Ready → Claimed → Terminating → deleted
    - Test ASG sync: new instances get DevboxDocs, removed instances get docs deleted
    - Test scale-in protection: enabled for Claimed, disabled for Ready
    - Test owner tag application on reconciler tick after claim
    - Test error scenarios: LT failure aborts tick, terminate failure retries next tick
    - Use MockCompute + in-memory SQLite DocumentStore
    - _Requirements: 1.4, 2.5, 3.4, 3.5, 3.6, 4.2, 4.3, 5.3, 5.4, 5.5, 5.6, 6.2, 6.4, 6.5, 6.6, 8.1, 8.3, 8.6_

- [ ] 9. Additional property tests
  - [ ]* 9.1 Write property test for scale-in protection invariant
    - **Property 7: Scale-In Protection for Claimed Instances**
    - Generate sets of DevboxDocs in various states
    - After reconciliation tick, verify Claimed instances have protection enabled, others disabled
    - **Validates: Requirements 3.6**

  - [ ]* 9.2 Write property test for termination cleanup
    - **Property 11: Termination Cleanup**
    - Generate Terminating DevboxDocs with and without instance_ids
    - Verify terminate_instance_in_asg called with should_decrement=false for docs with instance_id
    - Verify doc deleted after successful termination
    - Verify docs without instance_id deleted without AWS call
    - **Validates: Requirements 5.3, 5.4, 5.6**

  - [ ]* 9.3 Write property test for warming-to-ready lifecycle
    - **Property 12: Warming-to-Ready Lifecycle Progression**
    - Generate Warming DevboxDocs whose ASG instances transition to InService
    - Verify state transitions from Warming to Ready within same tick
    - Verify new Pending:Wait instances get Warming DevboxDocs created
    - **Validates: Requirements 6.2, 6.6**

  - [ ]* 9.4 Write property test for optimistic concurrency safety
    - **Property 13: Optimistic Concurrency Safety**
    - Generate random version pairs (expected vs actual)
    - Verify compare_and_update returns false when versions mismatch
    - Verify no data overwritten on conflict
    - **Validates: Requirements 8.4, 8.5**

  - [ ]* 9.5 Write property test for owner tag application
    - **Property 15: Owner Tag Application**
    - Generate Claimed DevboxDocs with owner_tag_applied=false
    - After reconciler tick, verify tag_instance called with correct owner tag
    - On tagging failure, verify doc remains Claimed with owner_tag_applied=false
    - **Validates: Requirements 4.2**

- [x] 10. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation of compilation and correctness
- Property tests validate universal correctness properties from the design document
- Unit/integration tests validate specific error scenarios and multi-step lifecycle flows
- The `Launching` state is kept in the enum for backward serde compatibility but is no longer used by the ASG reconciler
- All code must comply with the workspace's strict no-panic clippy lints (no unwrap, expect, indexing, unsafe, arithmetic side effects)
- Use `jiff` for timestamps, `sea-query` for SQL, `anyhow` for errors

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.2", "1.3"] },
    { "id": 1, "tasks": ["2.1"] },
    { "id": 2, "tasks": ["2.2", "2.3"] },
    { "id": 3, "tasks": ["4.1"] },
    { "id": 4, "tasks": ["4.2", "5.1"] },
    { "id": 5, "tasks": ["5.2", "5.3"] },
    { "id": 6, "tasks": ["5.4", "6.1", "6.2"] },
    { "id": 7, "tasks": ["6.3", "6.4", "6.5"] },
    { "id": 8, "tasks": ["8.1", "8.2"] },
    { "id": 9, "tasks": ["8.3", "9.1", "9.2", "9.3", "9.4", "9.5"] }
  ]
}
```
