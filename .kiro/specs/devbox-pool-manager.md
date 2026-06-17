# Devbox Pool Manager

**Status:** Draft

## Overview

The pool manager is the core background system that maintains a fixed number of pre-warmed EC2 instances ready for immediate claim by coding agents. It continuously reconciles the actual pool state with the desired state, launching new instances when the pool is depleted and terminating instances when they are released.

## Motivation

Coding agents need development environments on demand with near-zero latency. Cold-starting an EC2 instance takes 2-5 minutes (launch + boot + tool installation). By maintaining a warm pool, agents can claim a fully-ready devbox in under a second.

## Requirements

### Functional

1. **Fixed pool size** - Maintain a configurable number of Ready instances at all times
2. **Warm lifecycle** - Newly launched instances transition through Launching -> Warming -> Ready
3. **Claim semantics** - An agent claims a Ready instance, transitioning it to Claimed
4. **Release semantics** - A Claimed instance is released, transitioning to Terminating
5. **Replenishment** - When a Ready instance is claimed, the pool manager launches a replacement
6. **Timeout handling** - Instances stuck in Launching or Warming past a threshold are terminated
7. **Idle reclaim** - Claimed instances with no activity past a threshold are released automatically

### Non-Functional

1. **Reconciliation interval** - Every 30 seconds (configurable)
2. **Consistency** - Use optimistic concurrency (version column) to prevent double-claims
3. **Idempotency** - Multiple reconciliation ticks must be safe to run concurrently
4. **Observability** - Emit structured logs and metrics for pool state transitions
5. **Graceful shutdown** - Reconciliation loop stops cleanly via CancellationToken

## Design

### State Machine

```
                    +-----------+
                    | Launching |
                    +-----+-----+
                          |
                    (instance running)
                          |
                    +-----v-----+
                    |  Warming  |
                    +-----+-----+
                          |
                    (health check passes)
                          |
                    +-----v-----+
              +---->|   Ready   |<----+
              |     +-----+-----+     |
              |           |           |
              |     (claim request)   |
              |           |           |
              |     +-----v-----+    (never - instances
              |     |  Claimed  |     are terminated
              |     +-----+-----+     on release)
              |           |
              |     (release request)
              |           |
              |     +-----v-------+
              |     | Terminating |
              |     +-------------+
              |
        (pool reconciler launches
         replacement instance)
```

### Reconciliation Algorithm

Each tick:
1. Query all documents by state
2. Count Ready instances
3. If Ready < target_pool_size, launch (target - ready) new instances
4. For each Launching instance older than launch_timeout, mark as Terminating
5. For each Warming instance older than warming_timeout, mark as Terminating
6. For each Claimed instance idle longer than idle_timeout, mark as Terminating
7. For each Terminating instance, call EC2 TerminateInstances (idempotent)

### Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `target_pool_size` | 5 | Number of Ready instances to maintain |
| `reconcile_interval_secs` | 30 | Seconds between reconciliation ticks |
| `launch_timeout_secs` | 300 | Max time in Launching before timeout |
| `warming_timeout_secs` | 600 | Max time in Warming before timeout |
| `idle_timeout_secs` | 3600 | Max idle time for Claimed instances |

### Concurrency Safety

The claim operation uses compare-and-update (optimistic concurrency):

```
1. Fetch a Ready devbox document
2. Set state = Claimed, owner = requester, version = version + 1
3. compare_and_update(doc, expected_version)
4. If version conflict (another claim won), retry with next Ready instance
```

## Open Questions

1. Should the pool size be per-instance-type or a single pool?
2. How should "warming" be verified? (SSM health check? HTTP endpoint on instance?)
3. Should released instances be terminated immediately or recycled back to Ready?
4. What metrics should be emitted? (pool_ready_count, claim_latency, launch_duration)
