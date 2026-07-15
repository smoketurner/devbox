# Continuous Improvement

Project-specific instructions for the continuous improvement cycle.
This file is read by the `rust-ci-analyst` agent and the `/rust-agents:continuous-improvement` skill.

## Test Configuration

Unit tests run against in-memory/SQLite with a mock compute layer — no AWS required:

```bash
make test                                  # cargo test --all-features
cargo test -p devbox-server <substring>    # single crate / test
```

Run the server locally for live testing (serves http://localhost:3000):

```bash
make run-server                            # loads .env if present
# or explicit SQLite:
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server
```

For debug output:

```bash
RUST_LOG=debug cargo run --bin devbox-server 2>.local/testing/debug/session.log
```

CLI and on-host agent:

```bash
make run        ARGS="list"               # devbox CLI
make run-agent  ARGS="principals dev"     # on-host agent
```

## Project Subsystems

Workspace crates: `devbox-common`, `devbox-server`, `devbox-cli`, `devbox-agent`.
Logical subsystems to track in coverage-status.md:

- **reconciler** — adopt-only ASG loop, leader lock, owner tagging, readiness gate, reaper (`crates/devbox-server/src/reconcile/`)
- **compute** — AWS compute trait + impl + mock (`crates/devbox-server/src/compute/`)
- **document-store** — CRUD, pool queries, optimistic concurrency, DSQL, migrations (`crates/devbox-server/src/db/`)
- **auth** — JWT/JWKS verification, OIDC discovery, RP-initiated logout (`crates/devbox-server/src/auth/`)
- **routes + dashboard** — Axum API and HTML UI (`routes.rs`, `ui.rs`)
- **ssm-data-channel** — native SSM wire codec + reliable transport (`crates/devbox-cli/src/ssm/`)
- **cli-auth** — device-code OAuth + DCR, per-server session cache (`crates/devbox-cli/src/auth.rs`)
- **agent** — principals resolver, owner-sync, warmup, checkout, doctor (`crates/devbox-agent/src/`)

## Interfaces

- **HTTP API**: `/api/v1/devboxes/*`, `/api/v1/pool/metrics` (bearer-token auth); `/health` (unauthenticated)
- **HTML dashboard**: `GET /` (app-side OIDC session cookie)
- **CLI**: `devbox login/claim/release/list/status/ssh`
- **On-host agent**: `devbox-agent principals|owner-sync|warmup|checkout|doctor`

## Critical Paths

Prone to silent breakage not caught by unit tests — live-test before any PR that touches them:

- SSM data-channel wire codec + reliable transport (`crates/devbox-cli/src/ssm/message.rs`, `channel.rs`)
- Database migrations across both backends (`migrations/{sqlite,postgres}/`) and sea-query queries that must work on SQLite **and** Aurora DSQL
- Auth: Vouch JWKS verification, OIDC endpoint discovery, owner derivation via `username_from_email`
- Reconciler tick: desired-capacity math, scale-in protection, owner tagging, ready-timeout reaping
- `devbox ssh` end-to-end: profile auto-selection, ProxyCommand construction, login as the cert principal

## Environment Setup

- **No AWS needed for tests** — in-memory SQLite + mock compute layer.
- **Local server**: SQLite (`DATABASE_URL=sqlite:...`); production uses Aurora DSQL (IAM auth).
- **AWS access** is via IAM roles / instance profiles only — never static keys.
- **Crypto backend**: aws-lc-rs everywhere (avoid OpenSSL / ring / RustCrypto).
- **Auth**: Vouch OIDC at `https://us.vouch.sh` (override via `AUTH_OIDC_ISSUER`).

## Reference Projects

- **WorkOS Project Horizon** — allowlisting egress proxy, per-claim VCS token injection, stop/resume long-lived claims
- **Ramp Inspect** — snapshot-seeded EBS workspaces, predictive/multi-pool warming
- **edjgeek Claude Code Sandbox for iPad** — suspend-to-memory-snapshot idle state, proxy-measured idleness, hard-cap termination
- **`../vouch`** — production-proven source for shared DSQL/migration/serve/auth patterns; diff against it before "fixing" a shared pattern
- See `.kiro/references.md` for annotations on what devbox borrows.

## Testing Notes

- The workspace denies panic-prone patterns at the lint level (`unwrap`/`expect`/`panic`/indexing/unchecked arithmetic/`as` casts). Tests opt out per-item with `#[expect(clippy::unwrap_used, reason = "...")]`.
- No raw SQL — queries are built with `sea-query` so they run on both SQLite and DSQL.
- Platform target is arm64 + AL2023; do not introduce x86-only assumptions.
- `proptest` is available for parsers/serialization (e.g. the SSM wire codec).
