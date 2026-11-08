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
the native egress policy (`policy/mod.rs`) owns the **allow/deny decision**. Every
egress request is authorized through the single `EgressPolicy::decide_http` path
(the same policy instance as the CONNECT proxy), so there is no parallel ad-hoc
check and the policy's host can never drift from the host the proxy forwards to.

### CONNECT forward proxy and allowed ports

Alongside the credential-injecting reverse proxy there is a policy-driven
**CONNECT forward proxy** (`egress.rs`), enabled by `EGRESS_PROXY_ADDR`. A box
points `HTTPS_PROXY`/`HTTP_PROXY` at it; for each `CONNECT host:port` the proxy
asks the same shared `EgressPolicy` (default-deny) whether the host is permitted,
resolves it, refuses any non-public IP (SSRF / IMDS / RFC1918 protection), then
relays raw bytes. The tunnel is opaque, so this hop never injects a credential
(that is the reverse proxy's job).

The proxy relays a CONNECT only on **ports `{80, 443}`** and nothing else. The
set is a fixed constant in source (`ALLOWED_PORTS`); it is **not**
operator-configurable, and there is no env var to widen it. A CONNECT to any
other port is refused with `403` before any policy call or DNS resolution. Port
443 is HTTPS; **port 80** exists for the plain-HTTP `apt`/`yum` package mirrors
whose repositories are served over HTTP. The SSRF / IMDS / RFC1918 guard
(`resolve_public` + `is_forbidden_ip`, connecting to the exact resolved address
so there is no DNS-rebind window) and the default-deny host policy apply
**identically on both ports** - widening the port gate to 80 does not relax any
other guard.

### Default egress bundles (curated, default-on)

The server ships a small, versioned, **default-on** catalog of outbound egress
bundles compiled into the binary (`egress_bundles.rs`), so a freshly deployed
devbox can reach the package registries every common toolchain needs without the
operator hand-writing an `EGRESS_ALLOWLIST`. The catalog honors every existing
egress invariant: default-deny, forbid-overrides-permit, fail-closed, a single
`normalize_host` on both sides of every comparison, and one authorization path.

The catalog carries a `CATALOG_VERSION` (currently `1`), surfaced in startup
logging so an operator can tell exactly which built-in bundle set a running
server shipped. Bump it on any catalog change (host added or removed, template
changed).

There are **two bundle kinds**:

1. **Package-repository bundles** - a name plus a list of HTTPS hosts. Each host
   becomes a `permit`-`Connect` allowlist entry in the shared policy, exactly
   like an `EGRESS_ALLOWLIST` entry (so dot-boundary subdomain folding still
   applies: `static.crates.io` is covered by `crates.io`). These are **on by
   default** so `cargo`, `pip`, `npm`, `go`, container pulls, `git`/GitHub, and
   `apt`/`yum` all work with an **empty operator allowlist**.
2. **API integration templates** - a name, an upstream host (or host *pattern*),
   and method/path scaffolding, carrying **no** credential. A template stays
   **inert / fail-closed** until the operator supplies a credential (see
   [API integration templates](#api-integration-templates-fail-closed-until-credentialed)
   below). It contributes no Connect host and no `permit`-`HttpRequest` rule on
   its own.

#### Package-repository bundles and their hosts

Every host below is reached over HTTPS (port 443). The bundle name is the token
you pass to `EGRESS_DEFAULT_BUNDLES`.

| Bundle | Hosts |
|---|---|
| `rust` | `crates.io`, `static.crates.io`, `index.crates.io` |
| `python` | `pypi.org`, `files.pythonhosted.org` |
| `node` | `registry.npmjs.org` |
| `go` | `proxy.golang.org`, `sum.golang.org` |
| `containers` | `registry-1.docker.io`, `production.cloudflare.docker.com`, `ghcr.io`, `pkg-containers.githubusercontent.com`, `quay.io`, `cdn.quay.io`, `cdn01.quay.io`, `cdn02.quay.io`, `cdn03.quay.io`, `storage.googleapis.com` |
| `github` | `github.com`, `codeload.github.com`, `objects.githubusercontent.com`, `raw.githubusercontent.com` |
| `apt` | `deb.debian.org`, `security.debian.org`, `esm.ubuntu.com`, `archive.ubuntu.com`, `security.ubuntu.com` |
| `yum` | `cdn.amazonlinux.com`, `mirrors.fedoraproject.org`, `dl.fedoraproject.org` |

The `apt` and `yum` mirrors may be fetched over plain HTTP, which is why the
CONNECT proxy admits port 80 (see above).

#### Selecting bundles (`EGRESS_DEFAULT_BUNDLES`)

`EGRESS_DEFAULT_BUNDLES` selects which default bundles are active. Its semantics
(parsed as a pure function of the raw value):

- **unset** => all default bundles on (default-on).
- a **comma-separated list** of bundle names => **only** those bundles are active
  (both package bundles and API template names are valid tokens). Unknown names
  are logged with a warning and ignored (fail-safe, never fail-open); if every
  listed name is unknown, nothing is enabled (not "all").
- the token **`none`** (case-insensitive), or an **empty / whitespace-only**
  value => all default bundles off (the opt-out switch).

Examples:

```bash
# Default: unset => all bundles on; cargo/pip/npm/go/containers/github/apt/yum
# work with no operator allowlist.

# Enable only a subset (Rust + Python package repos):
EGRESS_DEFAULT_BUNDLES="rust,python"

# Turn the built-in catalog off entirely (operator supplies its own allowlist):
EGRESS_DEFAULT_BUNDLES=none
```

#### Composition with operator config

Bundles **compose with** the operator egress config rather than replacing it:

- `EGRESS_ALLOWLIST` hosts and the active package-bundle hosts are merged into
  the **same** shared policy as `permit`-`Connect` rules (identical dot-boundary
  folding). This holds whether the policy is seeded from the allowlist CSV or
  from a custom `EGRESS_POLICY_PATH` source (the bundle hosts are added as
  generated `permit`-`Connect` rules there too).
- `EGRESS_UPSTREAMS` method/path rules compile into that same policy. A catalog
  API template documents a supported integration (its host and method/path
  scaffold) but does not auto-wire anything; the operator makes an integration
  live by configuring an `EGRESS_UPSTREAMS` entry themselves, supplying that host,
  the allow scaffold, and a credential (see below).
- **forbid-overrides-permit** still holds: an operator `forbid` rule in
  `EGRESS_POLICY_PATH` (or disabling a bundle via `EGRESS_DEFAULT_BUNDLES`) wins
  over any bundled `permit`. Disabling or forbidding is always decisive.

At startup the server logs the active catalog: the `CATALOG_VERSION`, each active
package bundle with its host count, the total number of Connect hosts
contributed, and the eligible API template names (noting they stay inert until a
credential is supplied). A distinct line is logged when
`EGRESS_DEFAULT_BUNDLES=none` so a deny-all deployment is obvious in the logs.

#### API integration templates (fail-closed until credentialed)

The catalog names three API integration templates. Each declares a host (or host
pattern) and method/path scaffolding and carries **no** credential:

| Template | Host (or pattern) | Scaffolding |
|---|---|---|
| `anthropic` | `api.anthropic.com` | `POST /v1/messages`, `GET /v1/models` |
| `openai` | `api.openai.com` | `POST /v1/chat/completions`, `POST /v1/responses` |
| `aws-bedrock` | `bedrock-runtime.{region}.amazonaws.com` | `POST` any path (the model id is part of the path) |

A catalog template documents and names a supported integration (its host and
method/path scaffold) **by name only**; the catalog does not consult it at
request time and does not auto-wire a credential. It contributes **no**
`permit`-`HttpRequest` rule and **no** live reverse-proxy upstream on its own. To
make an integration live the operator configures an `EGRESS_UPSTREAMS` entry
themselves, supplying the host, the allow scaffold (which may be copied from the
template above), and a credential (the `static_header` or `aws_sigv4` mechanism
documented below). The reverse proxy reads only that `EGRESS_UPSTREAMS` entry; it
never looks the template up by name. Until the operator adds such an entry the
integration is inert: no Connect host is emitted for it, no `permit`-`HttpRequest`
rule exists, and `decide_http` default-denies it.

For `aws-bedrock` the template declares only the `{region}` host **pattern** and
the SigV4 `POST` scaffold; the concrete host and region come from the operator's
`EGRESS_UPSTREAMS` entry.

This keeps the credential on the control plane: it is **never hardcoded in the
catalog and never reaches the box or the agent**, and an un-credentialed template
is fully denied rather than silently credential-less or wide-open.

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
Vouch; authentication is always on).

Egress env vars:

| Var | Purpose |
|---|---|
| `EGRESS_PROXY_ADDR` | Enables the CONNECT forward proxy listener on this socket address (ports `{80, 443}`); unset disables the listener. |
| `EGRESS_DEFAULT_BUNDLES` | Selects the built-in default-on bundle catalog: unset => all on; comma-separated list => only those; `none`/empty => all off. Unknown names warned and ignored. |
| `EGRESS_ALLOWLIST` | Comma-separated CONNECT host allowlist, merged with the active bundle hosts into the shared policy (dot-boundary subdomain folding). |
| `EGRESS_POLICY_PATH` | Path to a Dogwood `.dw` policy source that replaces the allowlist-derived default policy; bundle hosts are still merged in and operator `forbid` rules still win. |
| `EGRESS_UPSTREAMS` | JSON registry of credential-injecting reverse-proxy integrations; the operator makes an integration live by adding an entry here (host, allow scaffold, and a credential), optionally using a catalog API template's host/scaffold as the reference. |
| `EGRESS_DECISION_LOG` | Optional path to append JSONL egress decision records to, in addition to the `tracing` sink. |

The JWKS URI and the dashboard
authorize/token/end-session endpoints are discovered at startup from
`{AUTH_OIDC_ISSUER}/.well-known/openid-configuration`. Token audience is not
validated — under DCR each CLI install has its own `aud`. The reconciler adopts the
ASG named `devbox-pool-<POOL_ID>` and reads the warm-pool target from its `min_size`.
