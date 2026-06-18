# Requirements Document

> **⚠️ Superseded.** These requirements target the original direct-`RunInstances`
> reconciler. Pool management is now ASG-based — see
> [`../asg-pool-management/`](../asg-pool-management/) and
> [`/CLAUDE.md`](../../../CLAUDE.md). Retained for history.

## Introduction

This document specifies the requirements for the Pool Reconciliation system and EC2 Client in the devbox server. The server operates in a stateless, multi-instance deployment model where multiple replicas may be running concurrently; all state is persisted in the database and no in-memory state is assumed to survive across ticks or restarts. The Pool Reconciliation system is a background task that maintains a configurable number of ready-to-use devbox instances by launching new EC2 instances when the pool is below target, advancing instances through their lifecycle states, terminating instances that are no longer needed, and recovering from stuck states. A distributed leader election mechanism ensures that only one Reconciler instance actively performs actions at a time, preventing duplicate launches across server replicas. The EC2 Client provides the interface to AWS for instance management, with both a real AWS SDK implementation and a mock implementation for testing.

## Glossary

- **Reconciler**: The background tokio task that periodically inspects the pool state and takes corrective actions to maintain the target pool size.
- **Pool**: The collection of all DevboxDoc records in the DocumentStore, representing EC2 instances at various lifecycle stages.
- **Target_Pool_Size**: The configured number of devbox instances that should be in the Ready state at any given time.
- **DevboxDoc**: The document type persisted in the DocumentStore representing a single devbox instance and its lifecycle state.
- **DocumentStore**: The persistence layer providing typed CRUD operations with optimistic concurrency via versioned compare-and-update.
- **Ec2Client**: The trait defining EC2 operations (launch, terminate, describe) needed by the Reconciler.
- **RealEc2Client**: The production implementation of Ec2Client using the aws-sdk-ec2 crate.
- **MockEc2Client**: The test implementation of Ec2Client that simulates instance state transitions without calling AWS.
- **Reconciliation_Tick**: A single iteration of the Reconciler's periodic loop where it inspects state and takes actions.
- **Stuck_Threshold**: The maximum duration an instance may remain in the Launching or Warming state before being considered stuck and transitioned to Terminating.
- **Pool_Metrics**: A summary of the current pool composition: counts of instances in each lifecycle state and the configured target.
- **CancellationToken**: A tokio-util primitive used to signal the Reconciler to shut down gracefully.
- **Leader_Lock**: A database-persisted advisory lock that ensures only one Reconciler instance across all server replicas performs reconciliation actions at a time.
- **Lock_TTL**: The maximum duration a Leader_Lock remains valid before it expires, allowing another server instance to acquire leadership.

## Requirements

### Requirement 1: Pool Size Maintenance

**User Story:** As an operator, I want the Reconciler to automatically launch new instances when the pool falls below the target size, so that users always have ready devboxes available for claiming.

#### Acceptance Criteria

1. WHEN the number of Ready DevboxDoc records is below Target_Pool_Size, THE Reconciler SHALL create a new DevboxDoc in the Launching state and invoke Ec2Client launch_instance with the configured instance type, AMI ID, and subnet ID.
2. WHEN launching a new instance, THE Reconciler SHALL store the EC2 instance ID returned by Ec2Client launch_instance in the DevboxDoc instance_id field.
3. WHILE the combined count of DevboxDoc records in Launching, Warming, and Ready states equals or exceeds Target_Pool_Size, THE Reconciler SHALL NOT launch additional instances.
4. WHEN Ec2Client launch_instance returns an error, THE Reconciler SHALL log the error at the error level and skip launching for that Reconciliation_Tick without crashing.

### Requirement 2: Instance Lifecycle Advancement

**User Story:** As an operator, I want the Reconciler to advance instances through their lifecycle states as they become ready, so that launched instances eventually become available for users.

#### Acceptance Criteria

1. WHEN a DevboxDoc is in Launching state and Ec2Client describe_instance reports the instance as "running", THE Reconciler SHALL transition the DevboxDoc state from Launching to Warming using compare_and_update.
2. WHEN a DevboxDoc is in Warming state and Ec2Client describe_instance reports the instance as "running", THE Reconciler SHALL transition the DevboxDoc state from Warming to Ready using compare_and_update.
3. WHEN compare_and_update returns false during a state transition, THE Reconciler SHALL log the version conflict at the warn level and skip that instance for the current Reconciliation_Tick.
4. WHEN Ec2Client describe_instance returns an error for an instance, THE Reconciler SHALL log the error and skip that instance for the current Reconciliation_Tick.

### Requirement 3: Instance Termination

**User Story:** As an operator, I want the Reconciler to terminate instances in the Terminating state and clean up their records, so that released devboxes are properly deprovisioned.

#### Acceptance Criteria

1. WHEN a DevboxDoc is in Terminating state, THE Reconciler SHALL invoke Ec2Client terminate_instance with the instance's EC2 instance ID.
2. WHEN Ec2Client terminate_instance succeeds, THE Reconciler SHALL delete the DevboxDoc from the DocumentStore.
3. WHEN a DevboxDoc in Terminating state has no instance_id, THE Reconciler SHALL delete the DevboxDoc from the DocumentStore without calling Ec2Client terminate_instance.
4. WHEN Ec2Client terminate_instance returns an error, THE Reconciler SHALL log the error at the error level and retry termination on the next Reconciliation_Tick.

### Requirement 4: Stuck Instance Recovery

**User Story:** As an operator, I want the Reconciler to detect and recover instances stuck in transitional states, so that broken instances do not consume pool capacity indefinitely.

#### Acceptance Criteria

1. WHEN a DevboxDoc has been in Launching state for longer than the configured Stuck_Threshold, THE Reconciler SHALL transition the DevboxDoc state to Terminating.
2. WHEN a DevboxDoc has been in Warming state for longer than the configured Stuck_Threshold, THE Reconciler SHALL transition the DevboxDoc state to Terminating.
3. THE Reconciler SHALL determine the duration in a transitional state by comparing the DevboxDoc updated_at timestamp with the current time.

### Requirement 5: Reconciler Configuration

**User Story:** As an operator, I want to configure the Reconciler's behavior through environment variables or a configuration struct, so that I can tune pool sizing and timing parameters for different environments.

#### Acceptance Criteria

1. THE Reconciler SHALL accept a configuration struct containing: target pool size, instance type, AMI ID, subnet ID, polling interval, stuck threshold, and lock TTL.
2. THE Reconciler SHALL use a default polling interval of 30 seconds when no value is configured.
3. THE Reconciler SHALL use a default stuck threshold of 10 minutes when no value is configured.
4. THE Reconciler SHALL use a default target pool size of 2 when no value is configured.
5. THE Reconciler SHALL use a default Lock_TTL of 60 seconds when no value is configured.

### Requirement 6: Graceful Shutdown

**User Story:** As an operator, I want the Reconciler to shut down gracefully when the server is stopping, so that in-progress operations complete without data corruption.

#### Acceptance Criteria

1. WHEN the CancellationToken is cancelled, THE Reconciler SHALL finish the current Reconciliation_Tick before exiting the loop.
2. WHEN the CancellationToken is cancelled between Reconciliation_Ticks, THE Reconciler SHALL exit without starting a new tick.
3. THE Reconciler SHALL log an informational message when shutdown is initiated.

### Requirement 7: Reconciler Idempotency

**User Story:** As an operator, I want the Reconciler to be safe to restart at any point, so that crashes or deployments do not cause duplicate instance launches or orphaned resources.

#### Acceptance Criteria

1. THE Reconciler SHALL use the DevboxDoc records in the DocumentStore as the sole source of truth for what instances exist and their states.
2. THE Reconciler SHALL use compare_and_update for all state transitions to prevent concurrent modification.
3. WHEN the Reconciler starts, THE Reconciler SHALL resume processing existing DevboxDoc records in their current states without duplicating prior actions.
4. THE Reconciler SHALL NOT hold any in-memory state between Reconciliation_Ticks that cannot be reconstructed from the DocumentStore.

### Requirement 8: Real EC2 Client Implementation

**User Story:** As a developer, I want a production EC2 client that uses the AWS SDK, so that the Reconciler can manage real EC2 instances in AWS.

#### Acceptance Criteria

1. THE RealEc2Client SHALL implement the Ec2Client trait using the aws-sdk-ec2 crate.
2. WHEN launch_instance is called, THE RealEc2Client SHALL call the EC2 RunInstances API with the specified instance type, AMI ID, and subnet ID, and return the launched instance ID.
3. WHEN terminate_instance is called, THE RealEc2Client SHALL call the EC2 TerminateInstances API with the specified instance ID.
4. WHEN describe_instance is called, THE RealEc2Client SHALL call the EC2 DescribeInstances API and return the instance state name as a string.
5. IF the AWS SDK returns an error, THEN THE RealEc2Client SHALL propagate the error wrapped in an anyhow::Error.

### Requirement 9: Mock EC2 Client Implementation

**User Story:** As a developer, I want a mock EC2 client for testing, so that I can verify Reconciler behavior without making real AWS calls.

#### Acceptance Criteria

1. THE MockEc2Client SHALL implement the Ec2Client trait using in-memory state.
2. WHEN launch_instance is called, THE MockEc2Client SHALL generate a synthetic instance ID and store the instance in an internal map with an initial state of "pending".
3. WHEN describe_instance is called, THE MockEc2Client SHALL return the current state of the instance from the internal map, advancing the state from "pending" to "running" after a configurable number of calls.
4. WHEN terminate_instance is called, THE MockEc2Client SHALL remove the instance from the internal map.
5. THE MockEc2Client SHALL allow test code to inject errors for specific operations to verify error-handling paths.

### Requirement 10: Pool Metrics Endpoint

**User Story:** As an operator, I want to query the current pool status through an API endpoint, so that I can monitor pool health and capacity.

#### Acceptance Criteria

1. WHEN a GET request is made to the pool metrics endpoint, THE Server SHALL return a JSON response containing: the count of instances in each DevboxState, the configured Target_Pool_Size, and the current deficit or surplus of Ready instances.
2. THE Server SHALL compute metrics by querying the DocumentStore for all DevboxDoc records and aggregating counts by state.
3. IF the DocumentStore query fails, THEN THE Server SHALL return an HTTP 500 response with an error message.

### Requirement 11: Distributed Reconciler Coordination

**User Story:** As an operator running multiple server instances, I want only one Reconciler to actively perform actions at a time, so that multiple replicas do not launch duplicate instances or conflict with each other.

#### Acceptance Criteria

1. BEFORE performing any reconciliation actions in a Reconciliation_Tick, THE Reconciler SHALL acquire a Leader_Lock by inserting or updating a lock record in the DocumentStore with the current server's identity and a Lock_TTL expiration timestamp.
2. IF another server instance holds a non-expired Leader_Lock, THE Reconciler SHALL skip the current Reconciliation_Tick and wait until the next polling interval.
3. THE Reconciler SHALL renew the Leader_Lock at the start of each successful Reconciliation_Tick to prevent expiration during normal operation.
4. WHEN a Reconciler instance crashes or shuts down, THE Leader_Lock SHALL expire after Lock_TTL, allowing another server instance to acquire leadership.
5. THE Reconciler SHALL use a default Lock_TTL of 60 seconds when no value is configured.
