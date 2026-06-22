# Devbox

[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue)](LICENSE-APACHE)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange)](https://www.rust-lang.org)

**Tooling to help any company adopt remote development machines — for engineers and the coding agents working alongside them.**

The control plane (HTTP API + pool reconciler) and CLI that manage a warm pool of ephemeral, isolated EC2 instances, claimable on demand over SSH. The AWS foundation (VPC, IAM, networking, AMI pipeline) is provisioned separately by Terraform in [`smoketurner/devbox-infra`](https://github.com/smoketurner/devbox-infra). Inspired by [Stripe's Minions architecture](https://www.tryprompt.ai/blog/how-stripe-built-an-ai-coding-assistant).

```bash
# Claim a ready devbox instantly (owner derived from your token)
$ devbox claim
Claimed devbox 01914a6b-... (i-0abc123def456)
Instance type: m5.large
Access: ssh dev@i-0abc123def456   # via your Vouch-issued SSH certificate

# When done, release it (ID defaults to your active claim)
$ devbox release
Released. Instance terminating.
```

## The Problem

Coding agents need isolated development environments but cannot wait minutes for provisioning:

| Approach | Latency | Isolation | Cost |
|----------|---------|-----------|------|
| Shared dev server | 0s | None | Low |
| On-demand EC2 | 2-5 min | Full | Pay-per-use |
| **Pre-warmed pool** | **< 1s** | **Full** | **Fixed pool** |

## How It Works

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Devbox Service                                │
│                                                                     │
│  ┌──────────────┐     ┌──────────────┐     ┌───────────────────┐   │
│  │  devbox-cli  │────>│ devbox-server│────>│  EC2 Pool (warm)  │   │
│  │  (claim/     │     │  (Axum API)  │     │  ┌───┐ ┌───┐ ┌───┐│   │
│  │   release)   │     │              │     │  │ R │ │ R │ │ R ││   │
│  └──────────────┘     └──────┬───────┘     │  └───┘ └───┘ └───┘│   │
│                              │             └───────────────────┘   │
│                              │                                     │
│                    ┌─────────v──────────┐                          │
│                    │  Pool Reconciler   │                          │
│                    │  (background loop) │                          │
│                    │                    │                          │
│                    │  Desired: 5 Ready  │                          │
│                    │  Actual:  3 Ready  │                          │
│                    │  Action: Launch 2  │                          │
│                    └────────────────────┘                          │
└─────────────────────────────────────────────────────────────────────┘

R = Ready instance (warm, waiting for claim)
```

1. **Pool reconciler** maintains a fixed number of warm instances
2. **Agent claims** a devbox -- instantly gets a ready-to-use instance
3. **Agent works** via SSH (the universal adapter every remote IDE speaks)
4. **Agent releases** -- instance terminates, reconciler launches a replacement

## Key Features

### Pre-Warmed Pool
Configurable number of instances always ready. Claim latency under 1 second.

### Ephemeral Instances
Each devbox is used once and terminated on release. No state leakage between users.

### SSH Access (Vouch CA)
Humans and agents connect over SSH — the universal adapter every remote IDE (VS Code Remote-SSH, JetBrains Gateway, Cursor) requires. Authentication is certificate-based via [Vouch](https://www.vouch.io/)'s SSH CA: short-lived user certificates, no `authorized_keys` to manage. Per-claim authorization is dynamic — claiming tags the instance `devbox:owner=<principal>`, which the host reads via IMDSv2 and enforces through sshd's `AuthorizedPrincipalsCommand` (`devbox-agent principals`). The login user is the certificate principal itself. See the "Access model" section of [`CLAUDE.md`](CLAUDE.md).

### Snapshot-Seeded EBS
Development tools, language runtimes, and package caches baked into EBS snapshots. New instances start with a fully-configured workspace.

### Dual-Backend Database
SQLite for local development, Aurora DSQL for production. Same code, same queries, runtime dispatch.

## Quick Start

### Build

```bash
# Prerequisites: Rust 1.96+, clang, pkg-config
make build
```

### Run Locally (SQLite)

```bash
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server
```

The server starts on `http://localhost:3000` with a dashboard at the root and API at `/api/`.

### Docker

```bash
# Build image
make docker-build

# Run with persistent SQLite volume
make docker-run
```

### CLI

```bash
# List all devboxes
devbox list --server-url http://localhost:3000

# Claim a devbox (owner derived from token)
devbox claim --server-url http://localhost:3000

# Check status (ID defaults to your active claim)
devbox status --server-url http://localhost:3000

# Release when done (ID defaults to your active claim)
devbox release --server-url http://localhost:3000
```

## Crates

| Crate | Description |
|-------|-------------|
| [`devbox-common`](crates/devbox-common/) | Shared types: `DevboxId`, `DevboxState` enum, API request/response types, configuration |
| [`devbox-server`](crates/devbox-server/) | Axum HTTP server with database layer (SQLite/DSQL), pool reconciliation, EC2 orchestration, HTML dashboard |
| [`devbox-cli`](crates/devbox-cli/) | CLI binary with `claim`, `release`, `list`, `status`, `ssh` subcommands via reqwest |

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Database connection string (`sqlite:...` or `postgres://...`) | `sqlite::memory:` |
| `RUST_LOG` | Tracing filter directive | `info` |
| `PORT` | Server listen port | `3000` |
| `DEVBOX_TOKEN` | Bearer token for CLI authentication (Vouch OIDC) | (none) |

### Pool Configuration (planned)

| Variable | Description | Default |
|----------|-------------|---------|
| `DEVBOX_POOL_SIZE` | Target number of Ready instances | `5` |
| `DEVBOX_INSTANCE_TYPE` | EC2 instance type | `m5.large` |
| `DEVBOX_AMI_ID` | AMI to launch instances from | (required) |
| `DEVBOX_SUBNET_IDS` | Comma-separated subnet IDs | (required) |
| `DEVBOX_RECONCILE_INTERVAL` | Seconds between reconciliation ticks | `30` |

## Development

```bash
make fmt      # Format code
make lint     # Run clippy
make test     # Run tests
make check    # Cargo check
```

See [`.kiro/steering/build-and-test.md`](.kiro/steering/build-and-test.md) for full development guide.

## Architecture Decisions

- **Document-oriented DB** - Minimizes schema migrations; devbox metadata stored as JSON documents with indexed fields
- **No encryption at rest** - Devbox metadata (instance IDs, states) is not sensitive enough to warrant client-side encryption
- **sea-query for SQL** - Type-safe query building that works across SQLite and Postgres without raw SQL
- **Trait-based EC2 client** - Enables unit testing of reconciliation logic without AWS access
- **CancellationToken shutdown** - Graceful shutdown of background loops without orphaned instances

## License

All crates are dual-licensed under:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

Choose whichever license works best for your use case.
