# CLAUDE.md

Guidance for Claude Code and other agents working in this repository.

## What this is

**Devbox provisions remote development machines** — pre-warmed, isolated EC2
instances that a human engineer or a coding agent can claim in seconds, work on
over **SSH**, and discard when done.

The thesis: **the remote dev machine is the right substrate for modern software
development**, and especially for agent-driven development. A standardized,
disposable, network-isolated instance gives you three things a laptop or a
container can't:

1. **Full-fidelity hardware and OS** — real Linux, real resources (up to many
   cores / hundreds of GB RAM), no "works on my machine."
2. **SSH access — the universal adapter every IDE already speaks.** VS Code
   Remote-SSH, JetBrains Gateway, Cursor, and plain terminals all attach over
   SSH with zero bespoke integration. This is the deciding constraint: the
   access path must be SSH.
3. **A real isolation boundary** — instances run with no production access and
   no arbitrary network egress, strong enough to run autonomous agents
   unattended (containers are not a security boundary; full instances are).

Pre-warming a pool hides the minutes of boot + cache-warming behind a
sub-second *claim* (the warm pool is the UX; the isolation is the value).
Machines are **cattle, not pets**: each is used once and terminated on release.

## Access model: SSH with a Vouch-operated CA

- Humans and agents connect over **SSH** — the universal adapter every remote
  IDE (VS Code Remote-SSH, JetBrains Gateway, Cursor) requires.
- **Authentication is SSH certificate–based, via Vouch's SSH CA.** Vouch issues
  short-lived user certificates; devbox hosts trust the CA through
  `TrustedUserCAKeys`. There are **no `authorized_keys` files to manage.**
- **Per-claim authorization is dynamic.** Claiming a devbox tags the instance
  `devbox:owner=<principal>` (already done by the reconciler's
  `apply_pending_owner_tags`). The host exposes that tag via IMDSv2
  (`InstanceMetadataTags=enabled`) and an `sshd` `AuthorizedPrincipalsCommand`
  reads it, so a CA-signed cert is accepted only for the current claimant — the
  box never calls back to the management plane.
- **Integration contract:** the `owner` in a claim request MUST equal the
  certificate principal Vouch issues (same identity namespace for humans and
  agents). The principal is not secret; security lives in the CA signature.
- Isolation per instance: dedicated security group, IMDSv2 required, no
  production IAM, EBS encrypted at rest.

See [`.kiro/specs/ssh-access/`](.kiro/specs/ssh-access/) for the full design.

## Architecture (30-second version)

Rust workspace, three crates:

| Crate | Role |
|-------|------|
| `devbox-common` | Shared types: `DevboxId`, `DevboxState`, API request/response |
| `devbox-server` | Axum API (`/api/v1/devboxes/*`) + HTML dashboard, document store (SQLite dev / Aurora DSQL prod), ASG-based pool reconciler, AWS compute layer |
| `devbox-cli`    | `claim` / `release` / `list` / `status` |

**Pool management is ASG-based.** The reconciler manages a Launch Template + Auto
Scaling Group; `DesiredCapacity = claimed_count + target_warm_pool_size`. It runs
as a leader-locked background loop (only one replica acts at a time) and syncs
`DevboxDoc` records against ASG membership each tick.

> Authoritative design: [`.kiro/specs/asg-pool-management/`](.kiro/specs/asg-pool-management/).
> The older `devbox-pool-manager.md`, `pool-reconciliation/`, and
> `ec2-integration.md` specs describe a **superseded** direct-`RunInstances`
> approach and are retained only as history (see their headers).

## Commands

```bash
make build      # release build (includes CSS)
make fmt        # cargo fmt --all
make lint       # cargo clippy --all-targets --all-features -- -D warnings
make test       # unit tests (in-memory SQLite; no AWS needed)
make check      # cargo check

# Run the server locally against SQLite:
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server          # serves http://localhost:3000
```

## Conventions (enforced — see `.kiro/steering/code-conventions.md`)

- **No panics in production code**: `unwrap`/`expect`/`panic`/`unreachable`/
  `todo`/indexing/unchecked arithmetic are *denied* at the lint level. Use
  `.get()`, `checked_*`/`saturating_*`, and `try_into`. Tests opt out with
  `#[expect(clippy::unwrap_used, reason = "...")]`.
- **No `unsafe`.**
- **No raw SQL** — build queries with `sea-query` (works across SQLite + DSQL).
- **No secrets in code**; AWS via IAM roles / instance profiles, never static keys.
- **Conventional commits** (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`).
- Run `make fmt && make lint && make test` before committing.

## Where things live

| Need | Location |
|------|----------|
| Shared types | `crates/devbox-common/src/lib.rs` |
| CLI | `crates/devbox-cli/src/main.rs` |
| Server entry / config / shutdown | `crates/devbox-server/src/main.rs` |
| HTTP routes | `crates/devbox-server/src/routes.rs` |
| Dashboard UI | `crates/devbox-server/src/ui.rs` |
| Reconciler (loop, tick, config, lock) | `crates/devbox-server/src/reconcile/` |
| AWS compute trait + impl + mock | `crates/devbox-server/src/compute/` |
| Document store (CRUD, pool, DSQL, migrations) | `crates/devbox-server/src/db/` |
| Devbox document type | `crates/devbox-server/src/documents/devbox.rs` |
| Migrations | `crates/devbox-server/migrations/{sqlite,postgres}/` |

## API surface

| Method + path | Purpose |
|---------------|---------|
| `GET /health` | Server + database health |
| `GET /api/v1/devboxes` | List all devboxes |
| `GET /api/v1/devboxes/{id}` | Get one devbox |
| `POST /api/v1/devboxes/claim` | Claim a Ready devbox (body: `owner`, optional `instance_type`) |
| `POST /api/v1/devboxes/{id}/release` | Release a Claimed devbox (body: `owner`) |
| `GET /api/v1/pool/metrics` | Pool counts vs target |
| `GET /` | HTML dashboard |

## Status: implemented vs planned

**Implemented:** API + CLI (claim/release/list/status), document store over
SQLite/DSQL with optimistic concurrency, ASG-based reconciler (Launch Template +
ASG + lifecycle hooks + scale-in protection + `devbox:owner` tagging on claim via
`apply_pending_owner_tags`), graceful shutdown, dashboard scaffolding, unit tests.
Release enforces **ownership** (owner must match) but there is no caller
authentication yet.

**Planned / not yet built:**
- **API authentication** — claim/release are currently unauthenticated (ownership
  is checked, identity is not).
- **SSH/Vouch-CA host config** — `InstanceMetadataTags=enabled` on the Launch
  Template (see `compute/ec2.rs` metadata options) plus the AMI-baked CA key,
  sshd drop-in, and `devbox-principals` script (see `.kiro/specs/ssh-access/`).
- **Snapshot-seeded EBS** workspace, SSH/SSM **health-check gating** of "warming",
  **idle-claim reclaim**, pool config via **env vars**.
- **Durable agent sessions** (reconnect while the agent keeps working).
- **Dashboard styling** — `static/css` is a placeholder.

## Source of truth

`.kiro/steering/*` for conventions and `.kiro/specs/asg-pool-management/` +
`.kiro/specs/ssh-access/` for the active designs. **When a doc disagrees with the
code, trust the code and fix the doc.**
