# devbox-server

The devbox control plane: an [Axum](https://github.com/tokio-rs/axum) HTTP API
and HTML dashboard, a document store, and the pool reconciler. It maintains a
warm pool of EC2 dev boxes that callers claim over SSH. See the
[project CLAUDE.md](../../CLAUDE.md) for the full architecture.

## What it does

- **API + dashboard** (`routes.rs`, `ui.rs`, `service.rs`) — `claim` / `release` /
  `rename` / `list` / `status` / pool metrics, plus an HTML dashboard. Claim is
  exactly-once under optimistic concurrency.
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
| `POST /api/v1/devboxes/{id}/rename` | Rename a Claimed devbox |
| `GET /api/v1/pool/metrics` | Pool counts vs target |
| `GET /` | HTML dashboard |

## Egress integrations (exe.dev-style)

The control plane can expose an **operator-configured registry of outbound
integrations** at `/egress/{integration}/{*rest}`. A box routes a flow through
the control plane; the control plane holds the credential, injects it into the
upstream request, and streams the response back, so **the secret never reaches
the box or the agent**. This is the method/path-aware generalization of the git
reverse proxy (`egress_proxy.rs`).

Separation of concerns: the proxy owns **transport + credential injection only**;
the native egress policy (`policy.rs`) owns the **allow/deny decision**. Every
egress request is authorized through the single `EgressPolicy::decide_http` path
(the same policy instance as the CONNECT proxy), so there is no parallel ad-hoc
check and the policy's host can never drift from the host the proxy forwards to.

### Configure an integration (`EGRESS_UPSTREAMS`)

`EGRESS_UPSTREAMS` is a JSON object mapping a stable **integration name** (the
`/egress/{name}/` path segment) to a spec:

| Field | Required | Meaning |
|---|---|---|
| `host` | yes | The single upstream host, e.g. `api.anthropic.com`. The proxy only ever speaks to this host over `https`; it is never attacker-controlled. |
| `allow` | yes (non-empty) | Method/path scoping. Each entry is `{ "method": "POST", "path": "/v1/messages" }`; `path` is optional (omit to permit any path for that method). These compile into the shared policy as `permit` rules — anything not matched is **denied** (default-deny). |
| `credential` | yes | How the server-held credential is injected (see below). |

Credential kinds:

- `static_header` — inject a fixed header whose value is read **once at startup**
  from an SSM SecureString via the task role. Fields: `header` (e.g. `x-api-key`),
  optional `prefix` (e.g. `"Bearer "`), and `ssm_param` (the SSM parameter name).
  The secret is held in a `secrecy::SecretString` and never logged.
- `aws_sigv4` — sign the outbound request with the control plane task role's own
  AWS credentials (no stored secret). Fields: `service` and `region`.

Any client-supplied `Authorization` header is **always stripped** before the
server injects its own credential.

#### Example 1 — Anthropic (static header from an SSM SecureString)

```json
{
  "anthropic": {
    "host": "api.anthropic.com",
    "allow": [
      { "method": "POST", "path": "/v1/messages" },
      { "method": "GET",  "path": "/v1/models" }
    ],
    "credential": {
      "kind": "static_header",
      "header": "x-api-key",
      "ssm_param": "/devbox/egress/anthropic-key"
    }
  }
}
```

A box calls `POST /egress/anthropic/v1/messages`; the control plane forwards it
to `https://api.anthropic.com/v1/messages` with `x-api-key` injected.

#### Example 2 — Bedrock (AWS SigV4)

```json
{
  "bedrock": {
    "host": "bedrock-runtime.us-east-1.amazonaws.com",
    "allow": [
      { "method": "POST", "path": "/model/anthropic.claude-3-sonnet/invoke" }
    ],
    "credential": {
      "kind": "aws_sigv4",
      "service": "bedrock",
      "region": "us-east-1"
    }
  }
}
```

A box calls `POST /egress/bedrock/model/.../invoke`; the control plane signs the
request with SigV4 and forwards it.

### Status codes

Unknown integration name → **404**; policy deny (unmatched method/path) →
**403**; missing/invalid box credential → **401**; proxy unconfigured → **503**;
upstream transport failure → **502**.

### Validation and provisioning

The server rejects a bad config at startup (before binding): malformed JSON, an
empty `host`, an empty `allow` list, an invalid header name, an unknown
credential `kind`, or a configured-but-unreadable SSM secret are all hard startup
errors (fail-closed). The `EGRESS_*` env and any referenced SSM parameters are
provisioned by **devbox-infra separately**; this crate only reads them.

## Run locally

```bash
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server            # serves http://localhost:3000
```

Key env vars: `DATABASE_URL`, `PORT`, `POOL_ID`, and `AUTH_OIDC_ISSUER` (default
Vouch; authentication is always on). The JWKS URI and the dashboard
authorize/token/end-session endpoints are discovered at startup from
`{AUTH_OIDC_ISSUER}/.well-known/openid-configuration`. Token audience is not
validated — under DCR each CLI install has its own `aud`. The reconciler adopts the
ASG named `devbox-pool-<POOL_ID>` and reads the warm-pool target from its `min_size`.
