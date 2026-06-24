# devbox-server

The devbox control plane: an [Axum](https://github.com/tokio-rs/axum) HTTP API
and HTML dashboard, a document store, and the pool reconciler. It maintains a
warm pool of EC2 dev boxes that callers claim over SSH. See the
[project CLAUDE.md](../../CLAUDE.md) for the full architecture.

## What it does

- **API + dashboard** (`routes.rs`, `ui.rs`) — `claim` / `release` / `list` /
  `status` / pool metrics, plus an HTML dashboard. Claim is exactly-once under
  optimistic concurrency.
- **Auth** (`auth/`) — resolves the caller from a `Bearer` Vouch JWT (CLI/agents
  via device-code OAuth) or the ALB's `x-amzn-oidc-data` (legacy), and binds
  `owner` to the verified principal. The dashboard uses app-side OIDC login with
  a session cookie.
- **Document store** (`db/`) — typed documents over SQLite (dev) or Aurora DSQL
  (prod, IAM-auth), with optimistic concurrency (`compare_and_update`) and
  queries built with `sea-query` (no raw SQL).
- **Reconciler** (`reconcile/`) — an adopt-only, leader-locked loop that adopts
  the Terraform-provisioned ASG by name, syncs `DevboxDoc` records with ASG
  membership, and maintains desired capacity, scale-in protection, owner
  tagging, and terminations. It never creates infrastructure.
- **Compute** (`compute/`) — the AWS EC2 / Auto Scaling trait, its EC2 impl, and
  a mock for tests.

## API

| Method + path | Purpose |
|---|---|
| `GET /health` | Server + database health |
| `GET /api/v1/devboxes` | List devboxes |
| `GET /api/v1/devboxes/{id}` | Get one |
| `POST /api/v1/devboxes/claim` | Claim a Ready devbox |
| `POST /api/v1/devboxes/{id}/release` | Release a Claimed devbox |
| `GET /api/v1/pool/metrics` | Pool counts vs target |
| `GET /` | HTML dashboard |

## Run locally

```bash
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server            # serves http://localhost:3000
```

Key env vars: `DATABASE_URL`, `PORT`, `POOL_ID`, `POOL_TARGET_WARM_SIZE`, and
`AUTH_OIDC_ISSUER` (default Vouch; authentication is always on). The JWKS URI and
the dashboard authorize/token/end-session endpoints are discovered at startup from
`{AUTH_OIDC_ISSUER}/.well-known/openid-configuration`. Token audience is not
validated — under DCR each CLI install has its own `aud`. The reconciler adopts the
ASG named `devbox-pool-<POOL_ID>`.
