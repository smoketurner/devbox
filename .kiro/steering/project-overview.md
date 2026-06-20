# Project Overview

## What is Devbox?

Devbox is a remote devbox orchestration service that manages a pool of pre-warmed EC2 instances for coding agents. Inspired by Stripe's Minions architecture, it provides ephemeral, isolated development environments that can be claimed on demand and released when done.

The core principle: always have warm instances ready so agents never wait for provisioning.

## Architecture

Devbox is a Rust workspace with these crates:

| Crate | Role |
|-------|------|
| `devbox-common` | Shared types (DevboxId, DevboxState, API request/response types, config) |
| `devbox-server` | Axum HTTP API, database layer (SQLite/DSQL), pool reconciliation, EC2 orchestration |
| `devbox-cli` | User-facing CLI for claiming, releasing, listing, and inspecting devboxes |
| `devbox-agent` | On-host binary baked into the AMI: principals resolver, owner-sync, warm-up hook |

## Key Concepts

### Devbox Lifecycle

Each devbox instance moves through a state machine:

```
Launching -> Warming -> Ready -> Claimed -> Terminating
```

- **Launching** - EC2 RunInstances has been called, instance is starting
- **Warming** - Instance is running but still initializing (installing tools, mounting EBS)
- **Ready** - Instance is warm and available for claim
- **Claimed** - A user/agent has claimed this instance and is using it
- **Terminating** - Instance is being torn down

### Pool Reconciliation (ASG-based)

Pool management is backed by an **Auto Scaling Group + Launch Template**, not
direct `RunInstances` calls. A leader-locked background loop reconciles each tick:
- Adopt the Terraform-provisioned ASG by name (skip the tick if absent)
- Set `DesiredCapacity = claimed_count + target_warm_pool_size`
- Sync `DevboxDoc` records against current ASG membership
- Apply the `devbox:owner` tag to newly claimed instances; manage warming,
  scale-in protection, and termination of released instances

The Launch Template, ASG, and lifecycle hook are provisioned by Terraform in
`devbox-infra`; the control plane only adopts the ASG and writes runtime state.

### Document Store

The database layer is document-oriented using a generic `DocumentStore`:
- Documents are stored as plain JSON (no encryption)
- Indexed fields enable efficient queries (state, owner, instance_id)
- Supports SQLite (local dev/test) and Aurora DSQL (production)
- Optimistic concurrency via version column
- Automatic expiration (TTL) support

## Server Architecture

Two route groups:
- **API routes** (`/api/v1/devboxes/*`, `/api/v1/pool/metrics`, `/health`) - JSON responses for programmatic access
- **UI routes** (`/`) - HTML dashboard via Askama templates + TailwindCSS

Devbox instances are reached over **SSH**, authenticated by **Vouch's SSH CA**
(short-lived certificates, no `authorized_keys`). Per-claim authorization is
dynamic via the `devbox:owner` instance tag + IMDSv2 + `AuthorizedPrincipalsCommand`.
See `.kiro/steering/security.md`.

Database: SQLite for local development, in-memory SQLite for tests, Aurora DSQL in production. The `Pool` enum dispatches at runtime based on `DATABASE_URL` scheme. Query building uses `sea-query`. Migrations in `crates/devbox-server/migrations/{sqlite,postgres}/`.

## Toolchain

- Rust 1.96.0, edition 2024 (pinned in `rust-toolchain.toml`)
- Max line width: 100 chars (`.rustfmt.toml`)
- Release profile: `lto = true`, `codegen-units = 1`, `opt-level = "z"`, `panic = "abort"`, `strip = true`

## License

All crates: Apache-2.0 OR MIT (dual-licensed).
