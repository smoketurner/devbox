# Implementation Plan: Pool Reconciliation

## Overview

This plan implements the pool reconciliation system for the devbox-server. The work is structured around module restructuring first, then building the core abstractions (config, EC2 trait, leader lock), followed by the reconciliation tick logic, the metrics endpoint, and finally integration testing. Each task builds incrementally on the previous ones and references specific requirements.

## Tasks

- [ ] 1. Add dependencies and restructure modules
  - [x] 1.1 Add aws-sdk-ec2 and proptest dependencies to Cargo.toml
    - Add `aws-sdk-ec2` to `[dependencies]` in `crates/devbox-server/Cargo.toml` with features `["rt-tokio", "default-https-client", "behavior-version-latest"]`
    - Add `proptest` to `[dev-dependencies]`
    - _Requirements: 8.1_

  - [ ] 1.2 Restructure `ec2/` module into trait, real, and mock sub-modules
    - Replace `crates/devbox-server/src/ec2/mod.rs` with: `InstanceStatus` enum, refined `Ec2Client` trait (returning `InstanceStatus` from `describe_instance`)
    - Create `crates/devbox-server/src/ec2/real.rs` with `RealEc2Client` struct and implementation
    - Create `crates/devbox-server/src/ec2/mock.rs` with `MockEc2Client` struct and implementation (gated behind `#[cfg(test)]` or `test-utils` feature)
    - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 9.1, 9.2, 9.3, 9.4, 9.5_

  - [ ] 1.3 Convert `reconcile.rs` to `reconcile/` directory module
    - Create `crates/devbox-server/src/reconcile/mod.rs` re-exporting the public API (`spawn_reconciliation_loop`, `ReconcilerConfig`)
    - Create `crates/devbox-server/src/reconcile/config.rs` with `ReconcilerConfig` struct and `Default` impl
    - Create `crates/devbox-server/src/reconcile/lock.rs` (empty placeholder with module doc)
    - Create `crates/devbox-server/src/reconcile/tick.rs` (empty placeholder with module doc)
    - Create `crates/devbox-server/src/reconcile/tests.rs` (empty `#[cfg(test)]` module)
    - Delete the old `crates/devbox-server/src/reconcile.rs`
    - Ensure `crates/devbox-server/src/lib.rs` still declares `pub mod reconcile;` and compiles
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5_

  - [ ] 1.4 Create `documents/leader_lock.rs` with `LeaderLockDoc` type
    - Implement `DocumentType` for `LeaderLockDoc` with `DOC_TYPE = "leader_lock"`, index on `holder_id`, and `expires_at` returning `Some(self.expires_at)`
    - Register the module in `crates/devbox-server/src/documents/mod.rs`
    - _Requirements: 11.1_

- [ ] 2. Checkpoint - Ensure compilation passes
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 3. Implement leader lock acquisition
  - [ ] 3.1 Implement `try_acquire_lock` and `renew_lock` in `reconcile/lock.rs`
    - Implement the lock acquisition algorithm: get by well-known ID → insert if absent → compare_and_update if expired or owned → skip if held by another
    - Use the well-known document ID `"reconciler-leader-lock"`
    - Use `DocumentStore::compare_and_update` for atomic updates
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5_

  - [ ]* 3.2 Write property tests for leader lock (Properties 7, 8, 9)
    - **Property 7: Leader Lock Enforcement** — verify zero EC2 calls and zero state transitions when lock is held by another non-expired server
    - **Property 8: Expired Lock Acquisition** — verify a new server can acquire an expired lock
    - **Property 9: Lock Renewal** — verify expires_at is updated to at least lock_ttl into the future after a successful tick
    - **Validates: Requirements 11.1, 11.2, 11.3, 11.4**

- [ ] 4. Implement reconciliation tick logic
  - [ ] 4.1 Implement `reconciliation_tick` orchestrator in `reconcile/tick.rs`
    - Create the top-level `reconciliation_tick` function that calls the five steps in order: query all docs, stuck recovery, terminate, launch, advance lifecycle
    - Each step is a separate async helper function within `tick.rs`
    - _Requirements: 1.1, 2.1, 3.1, 4.1, 7.1, 7.4_

  - [ ] 4.2 Implement stuck instance recovery (Step 2)
    - For each DevboxDoc in Launching or Warming where `now - doc.updated_at > stuck_threshold`, transition to Terminating via `compare_and_update`
    - Log stuck transitions at warn level
    - _Requirements: 4.1, 4.2, 4.3_

  - [ ]* 4.3 Write property test for stuck recovery (Property 4)
    - **Property 4: Stuck Instance Recovery**
    - **Validates: Requirements 4.1, 4.2**

  - [ ] 4.4 Implement termination handler (Step 3)
    - For each DevboxDoc in Terminating state: call `terminate_instance` if `instance_id` is Some, then delete doc; delete directly if `instance_id` is None
    - Handle errors per-instance (log and skip)
    - _Requirements: 3.1, 3.2, 3.3, 3.4_

  - [ ]* 4.5 Write property test for termination cleanup (Property 5)
    - **Property 5: Terminating Document Cleanup**
    - **Validates: Requirements 3.1, 3.2, 3.3**

  - [ ] 4.6 Implement pool size check and launch (Step 4)
    - Count Launching + Warming + Ready docs; while count < target_pool_size, create DevboxDoc, call `launch_instance`, update with instance_id on success
    - Handle launch errors gracefully (log and skip)
    - _Requirements: 1.1, 1.2, 1.3, 1.4_

  - [ ]* 4.7 Write property tests for pool size maintenance (Properties 1, 2)
    - **Property 1: Pool Size Maintenance Invariant** — launch iff Launching+Warming+Ready < target
    - **Property 2: Launch Stores Instance ID** — returned instance ID stored in DevboxDoc
    - **Validates: Requirements 1.1, 1.2, 1.3**

  - [ ] 4.8 Implement lifecycle advancement (Step 5)
    - For each Launching doc with instance_id: describe → if Running, transition to Warming
    - For each Warming doc: describe → if Running, transition to Ready
    - Handle errors and version conflicts per-instance
    - _Requirements: 2.1, 2.2, 2.3, 2.4_

  - [ ]* 4.9 Write property test for lifecycle advancement (Property 3)
    - **Property 3: Lifecycle Advancement on Running Status**
    - **Validates: Requirements 2.1, 2.2**

  - [ ]* 4.10 Write property test for error resilience (Property 6)
    - **Property 6: Error Resilience** — single EC2 failure does not abort tick; other instances still processed
    - **Validates: Requirements 1.4, 2.4, 3.4**

- [ ] 5. Checkpoint - Ensure all reconciliation tick tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 6. Wire up the reconciler loop and update AppState
  - [ ] 6.1 Update `spawn_reconciliation_loop` in `reconcile/mod.rs`
    - Change signature to accept `Arc<dyn Ec2Client>`, `ReconcilerConfig`, and `CancellationToken`
    - Implement the loop: interval tick → try_acquire_lock → reconciliation_tick on success; graceful shutdown on cancel
    - _Requirements: 6.1, 6.2, 6.3, 7.1, 7.2, 7.3, 7.4_

  - [ ] 6.2 Update `AppState` in `routes.rs` to include `Arc<ReconcilerConfig>`
    - Add `reconciler_config: Arc<ReconcilerConfig>` field to `AppState`
    - _Requirements: 10.1_

  - [ ] 6.3 Update `main.rs` to construct config, EC2 client, and pass to reconciler
    - Load `ReconcilerConfig` from environment variables with defaults
    - Construct `RealEc2Client` from `aws_config::load_defaults`
    - Pass `Arc<dyn Ec2Client>`, config, and cancel token to `spawn_reconciliation_loop`
    - Update `AppState` construction to include `reconciler_config`
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 8.1_

- [ ] 7. Implement pool metrics endpoint
  - [ ] 7.1 Add `GET /api/v1/pool/metrics` route handler in `routes.rs`
    - Query `store.list_all::<DevboxDoc>()`, count by state, compute `ready_delta = target_pool_size - ready_count`
    - Return `PoolMetricsResponse` as JSON
    - Return HTTP 500 with error message on store failure
    - Register route in `build_router`
    - _Requirements: 10.1, 10.2, 10.3_

  - [ ]* 7.2 Write property test for metrics aggregation (Property 10)
    - **Property 10: Metrics Aggregation Correctness**
    - **Validates: Requirements 10.1**

- [ ] 8. Implement MockEc2Client state machine tests
  - [ ]* 8.1 Write property test for mock EC2 state machine (Property 11)
    - **Property 11: Mock EC2 State Machine Consistency** — unique IDs, Pending → Running after N calls, terminate removes instance
    - **Validates: Requirements 9.2, 9.3, 9.4**

- [ ] 9. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties from the design document
- Unit tests validate specific examples and edge cases
- The `reconcile.rs` → `reconcile/` conversion must be done carefully to preserve git history; rename and then split
- The `MockEc2Client` is gated behind `#[cfg(test)]` or the `test-utils` feature flag so it does not appear in production builds
- All state transitions use `compare_and_update` for optimistic concurrency safety

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2", "1.3", "1.4"] },
    { "id": 2, "tasks": ["3.1"] },
    { "id": 3, "tasks": ["3.2", "4.1"] },
    { "id": 4, "tasks": ["4.2", "4.4", "4.6", "4.8"] },
    { "id": 5, "tasks": ["4.3", "4.5", "4.7", "4.9", "4.10"] },
    { "id": 6, "tasks": ["6.1", "6.2"] },
    { "id": 7, "tasks": ["6.3", "7.1"] },
    { "id": 8, "tasks": ["7.2", "8.1"] }
  ]
}
```
