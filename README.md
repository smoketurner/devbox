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
# Log in via device-code OAuth (Vouch + FIDO2/YubiKey)
devbox login --server http://localhost:3000

# List all devboxes
devbox list --server http://localhost:3000

# Claim a devbox (owner derived from your login session)
devbox claim --server http://localhost:3000

# Check status (ID defaults to your active claim)
devbox status --server http://localhost:3000

# Release when done (ID defaults to your active claim)
devbox release --server http://localhost:3000
```

## Crates

| Crate | Description |
|-------|-------------|
| [`devbox-common`](crates/devbox-common/) | Shared types: `DevboxId`, `DevboxState` enum, API request/response types, configuration |
| [`devbox-server`](crates/devbox-server/) | Axum HTTP server with database layer (SQLite/DSQL), pool reconciliation, EC2 orchestration, HTML dashboard |
| [`devbox-cli`](crates/devbox-cli/) | CLI binary with `claim`, `release`, `rename`, `list`, `status`, `ssh` subcommands via reqwest |
| [`devbox-agent`](crates/devbox-agent/) | On-host binary baked into the golden AMI: `principals` (sshd resolver), `owner-sync` (provision the claimant's account), `warmup` (freshen `/workspace`, self-tag `devbox:ready=true`), `checkout` (clone repos into `/workspace`) |

## Components and the AMI pipeline

There is a hard ownership line between this repo and the Terraform substrate in
[`devbox-infra`](https://github.com/smoketurner/devbox-infra). This repo owns
*behavior* and ships two artifacts; `devbox-infra` owns *infrastructure* and
consumes them. Neither side's code provisions the other's resources — the
reconciler is **adopt-only**.

| Repo | Owns | Produces |
|------|------|----------|
| **`devbox`** (this repo) | Control plane + on-host tooling | A **server container image** (→ ECR/ECS) and the **`devbox-agent` binary** (→ GitHub release) |
| **`devbox-infra`** | AWS substrate: VPC, IAM, the golden-AMI pipeline, the pool ASG, the workspace snapshot, the ECS control plane | The **golden AMI** + the running ASG/ECS that consume the two artifacts |

Where each crate runs determines how it reaches the host:

- `devbox-server` is **deployed** to ECS/Fargate. It never logs into a box; it
  adopts the Terraform-provisioned ASG and writes runtime state (desired
  capacity, `devbox:owner` tags, terminations).
- `devbox-cli` runs **off-box** on a laptop and reaches instances over an SSM
  tunnel — no bastion, VPN, or public IP.
- `devbox-agent` is the **only** crate baked into the AMI. It is the single seam
  where this repo's code crosses into the image.

### The `04-devbox` seam

`devbox-infra` builds the AMI with EC2 Image Builder from an ordered component
chain (`modules/image-builder/components/`): `01-base` (OS hardening) → `02-toolchain`
(language runtimes) → `03-repos` (git access) → **`04-devbox`** → `05-docker-images`
(pre-pulled images) → `99-validation`. The fourth component,
`04-devbox.yml.tftpl`, is what binds `devbox-agent` into the image:

1. **Downloads the agent** from a Terraform-injected `agent_url` + `agent_sha256`
   to `/usr/local/sbin/devbox-agent`. CI in this repo builds the
   `aarch64-unknown-linux-musl` binary (`make bake-agent`), publishes it to a
   GitHub release, and `devbox-infra` pins that URL + SHA. A new agent version is
   a new release + a recipe bump.
2. **Installs the systemd units** that invoke `devbox-agent warmup` and
   `devbox-agent owner-sync`, plus `/etc/devbox/warmup.env` baked from non-secret
   Terraform vars (GitHub App id, the SSM parameter name holding the App key,
   fetch timeout). The App private key itself is never baked — `warmup` reads it
   from SSM at boot.
3. **Configures sshd + the Vouch CA** — drops `TrustedUserCAKeys` and
   `AuthorizedPrincipalsCommand /usr/local/sbin/devbox-agent principals %u`, and
   fetches the Vouch CA public key. This is the host half of the access model in
   [`CLAUDE.md`](CLAUDE.md).

The pool launch template (`modules/pool`) then resolves the AMI lazily
(`image_id = resolve:ssm:/devbox/ami/latest`), enables IMDSv2 with instance tags
(read by `principals`/`owner-sync`), and clones the workspace snapshot
(`modules/snapshot-builder`) as a per-instance volume that `warmup` freshens.

### Runtime handshake

```
devbox-infra (Image Builder)                 devbox repo CI
  01→05 components build the AMI                builds devbox-agent (musl)
  04-devbox curls the agent from ───────────►  GitHub release (url + sha256)
  publishes AMI id → /devbox/ami/latest

devbox-infra (pool ASG + LT)                 devbox-server (ECS, adopt-only)
  LT resolves AMI, clones workspace snap        reconciler keeps N warm
  instance boots:
    warmup.service ──► devbox-agent warmup
        freshen /workspace, tag devbox:ready=true
                                          reconciler sees tag → Ready
  devbox claim (CLI) ──► server tags devbox:owner / devbox:owner-email
    owner-sync.service ──► devbox-agent owner-sync (provisions account)
  devbox ssh (CLI, native SSM tunnel)
    sshd ──► devbox-agent principals %u ──► authorize iff login == owner
```

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Database connection string (`sqlite:...` or `postgres://...`) | `sqlite::memory:` |
| `RUST_LOG` | Tracing filter directive | `info` |
| `PORT` | Server listen port | `3000` |
| `DEVBOX_SERVER` | Default server URL for the CLI | `http://localhost:3000` |

### Pool Configuration

Pool sizing is managed by Terraform in `devbox-infra` via the ASG's `min_size` and `max_size`.
The reconciler adopts the ASG by name and computes `DesiredCapacity = min(claimed_count + ASG min_size, ASG max_size)`.
Instance type, AMI, and subnets are read from the Launch Template and ASG at runtime.

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
