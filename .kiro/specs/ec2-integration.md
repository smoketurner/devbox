# EC2 Integration

> **⚠️ Superseded.** This spec describes direct EC2 instance lifecycle operations
> (`RunInstances` / `TerminateInstances` / EBS attach). The implementation now
> manages instances through an Auto Scaling Group + Launch Template; see
> [`asg-pool-management/`](asg-pool-management/) and the `compute` module
> (`crates/devbox-server/src/compute/`). Retained for history; see
> [`/CLAUDE.md`](../../CLAUDE.md) for the current architecture.

**Status:** Superseded (was: Draft)

## Overview

The EC2 integration module handles all interactions with AWS EC2 for launching, terminating, and inspecting devbox instances. It manages the full instance lifecycle including EBS volume attachment, SSM access configuration, and instance health monitoring.

## Motivation

The devbox service needs to:
- Launch instances from pre-baked AMIs with development tools
- Attach snapshot-seeded EBS volumes for persistent workspace data
- Enable SSM Session Manager access (no SSH keys or open ports)
- Terminate instances cleanly when released
- Monitor instance health for the reconciliation loop

## Requirements

### Functional

1. **Launch instance** - Start an EC2 instance from a specified AMI in a given subnet
2. **Terminate instance** - Stop and terminate an instance by ID
3. **Describe instance** - Get current status of an instance
4. **Create EBS volume** - Create a volume from a snapshot for workspace data
5. **Attach volume** - Attach an EBS volume to a running instance
6. **Detach volume** - Detach volume before termination (for snapshot preservation)
7. **Health check** - Verify instance is reachable via SSM

### Non-Functional

1. **Idempotency** - Terminate calls must be safe to repeat
2. **Timeout** - All EC2 API calls must have timeouts
3. **Retry** - Transient AWS errors (throttling, 500s) must be retried with backoff
4. **IMDSv2** - All launched instances must require IMDSv2
5. **Encryption** - All EBS volumes must be encrypted at rest
6. **Tagging** - All resources must be tagged for identification and cost tracking

## Design

### EC2 Client Trait

```rust
pub trait Ec2Client: Send + Sync {
    /// Launch a new EC2 instance with the specified configuration.
    fn launch_instance(
        &self,
        instance_type: &str,
        ami_id: &str,
        subnet_id: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    /// Terminate an EC2 instance.
    fn terminate_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Get the status of an EC2 instance.
    fn describe_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<String>> + Send;
}
```

### Instance Launch Configuration

Each instance is launched with:
- **AMI** - Pre-baked with development tools (git, compilers, language runtimes)
- **Instance type** - Configurable, default `m5.large`
- **Subnet** - Placed in a private subnet (no public IP)
- **Security group** - Allows SSM agent traffic only (no inbound SSH)
- **IAM instance profile** - Grants SSM access and CodeArtifact/ECR read
- **IMDSv2 required** - `HttpTokens = required`
- **User data** - Runs initialization script (mount EBS, configure SSM)

### EBS Volume Strategy

```
1. Base snapshot contains pre-configured workspace:
   - /home/agent/ with dotfiles, tool configs
   - Pre-cloned repositories (optional)
   - Language package caches (cargo registry, pip cache)

2. On claim: create volume from snapshot, attach to instance
3. On release: detach volume, optionally snapshot for next use, delete volume
```

### SSM Access Pattern

```
Agent -> SSM Session Manager -> Devbox Instance
```

- No SSH keys required
- No inbound ports open
- Session logging to CloudWatch for audit
- Access controlled via IAM policy on the calling agent's role

### Resource Tagging

All EC2 resources are tagged with:

| Tag | Value |
|-----|-------|
| `devbox:managed` | `true` |
| `devbox:id` | Document ID from the store |
| `devbox:state` | Current lifecycle state |
| `devbox:owner` | Owner identifier (when claimed) |
| `devbox:created-at` | ISO 8601 timestamp |

### Error Handling

| AWS Error | Behavior |
|-----------|----------|
| `RequestLimitExceeded` | Retry with exponential backoff (max 3 retries) |
| `InsufficientInstanceCapacity` | Log warning, retry with different AZ |
| `InvalidAMIID.NotFound` | Fail immediately, alert operator |
| `InstanceLimitExceeded` | Fail immediately, alert operator |
| `InvalidInstanceID.NotFound` | Treat as already terminated (idempotent) |

### Testing Strategy

- Unit tests: mock EC2 client via trait (already defined)
- Integration tests: use localstack or real AWS with a test VPC
- The trait-based design allows the reconciliation loop to be tested without AWS access

## Open Questions

1. Should volumes be preserved across claim/release cycles or always fresh from snapshot?
2. How frequently should the base AMI be refreshed? (weekly? on dependency updates?)
3. Should multi-AZ placement be supported for resilience?
4. What instance types should be supported? (single type or multiple pools?)
5. Should spot instances be used for cost savings? (interruption handling needed)
6. How should the service discover available subnets and security groups? (config? tags?)
