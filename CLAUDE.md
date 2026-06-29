# CLAUDE.md

Guidance for Claude Code and other agents working in this repository.

## What this is

**Devbox is tooling to help any company adopt and operate remote development
machines.** It is the control plane — an HTTP API, a CLI, and a pool reconciler —
that maintains a warm pool of isolated EC2 instances that anyone on a team
(engineers and the coding agents working alongside them) can claim in seconds,
work on over **SSH**, and discard when done.

The underlying AWS foundation (VPC, networking, IAM, the AMI pipeline) is
provisioned separately by Terraform in
[`smoketurner/devbox-infra`](https://github.com/smoketurner/devbox-infra). This
repo is the server + CLI + tooling that runs on top of that substrate.

The thesis: **the remote dev machine is the right substrate for modern software
development** — for people and agents alike. A standardized, disposable,
network-isolated instance gives you three things a laptop or a container can't:

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
  `devbox:owner=<principal>` (applied inline by the claim handler so the box is
  loginable without waiting for a reconciler tick; the reconciler's
  `apply_pending_owner_tags` re-applies it as an idempotent fallback if the inline
  call fails).
  The host exposes that tag via IMDSv2 (`InstanceMetadataTags=enabled`) and an
  `sshd` `AuthorizedPrincipalsCommand` — `devbox-agent principals %u` — reads it,
  so a CA-signed cert is accepted only when the login user equals the current
  claimant. The box never calls back to the management plane.
  - *Why a command, not an `AuthorizedPrincipalsFile`:* warm-pool timing. A box
    boots and reaches Ready **before** anyone claims it, so at user-data time there
    is no `devbox:owner` tag to write into a file. The owner arrives later (on
    claim), so `sshd` must read the *current* tag lazily on each auth — a command
    pulls it; a file would need a writer/daemon kept in sync. Fail-closed: no tag →
    no principals authorized.
- **The login user is the certificate principal.** There is no shared `dev`
  account: `devbox-agent owner-sync` provisions a Unix account named after the
  `devbox:owner` principal (passwordless sudo, owns `/workspace`). It also reads the
  `devbox:owner-email` tag — set by `apply_pending_owner_tags` from the claimant's
  token email, alongside `devbox:owner` — and writes the account's `~/.gitconfig`
  (`user.email`/`user.name`) so commits are attributed without manual setup.
  `devbox ssh` logs in as that principal over an SSM Session Manager tunnel (no public IP).
  The SSM data-channel protocol is implemented **natively in-process** (a hidden
  `devbox ssm-proxy` subcommand used as the ssh `ProxyCommand`): it calls
  `ssm:StartSession` for `AWS-StartSSHSession`, opens the WebSocket data channel
  (rustls + aws-lc-rs), and speaks the binary AgentMessage framing + reliable
  transport itself, so no `session-manager-plugin` binary and no `aws` CLI are
  required — only the system `ssh` client. See `crates/devbox-cli/src/ssm/`
  (`message.rs` = wire codec, `channel.rs` = handshake + reliable transport).
  - *AWS profile auto-selection:* the SSM tunnel's `StartSession` call needs
    AWS credentials for the control-plane account. The server advertises that
    account as an `aws_account_id` extension on the RFC 9728 discovery document
    (`/.well-known/oauth-protected-resource`, set from the `AWS_ACCOUNT_ID` env
    var). When `--profile` is omitted and neither `AWS_PROFILE` nor
    `AWS_ACCESS_KEY_ID` is set, `devbox ssh` reads that account and picks the
    local `~/.aws/config` profile whose `role_arn` / `credential_process --role`
    targets it (`crates/devbox-cli/src/aws_profile.rs`), so the user never has to
    remember which profile is the devbox account. No match / no account / old
    server falls back to the caller's default credentials.
- **Integration contract:** the `owner` derived from the authenticated token's
  `email` claim MUST equal the certificate principal Vouch issues (same identity
  namespace for humans and agents). The principal is not secret; security lives
  in the CA signature.
- Isolation per instance: dedicated security group, IMDSv2 required, no
  production IAM, EBS encrypted at rest.

The host side is baked into the AMI by the `devbox-infra` `04-devbox` Image Builder
component (sshd drop-in + Vouch CA key + `devbox-agent`); the SSH login itself is
`crates/devbox-cli/src/ssh.rs` and `crates/devbox-agent/`.

## Architecture (30-second version)

**Lifecycle:** `Launching → Warming → Ready → Claimed → Terminating` (`DevboxState`
in `crates/devbox-common/src/lib.rs`). The ASG launches a box (`Launching`);
`devbox-agent warmup` self-tags `devbox:ready=true` and the reconciler flips
`Warming → Ready`; `claim` moves `Ready → Claimed` and applies the `devbox:owner`
tag; `release` or the ready-timeout reaper drives `Terminating`.

Rust workspace, four crates:

| Crate | Role |
|-------|------|
| `devbox-common` | Shared types: `DevboxId`, `DevboxState`, API request/response |
| `devbox-server` | Axum API (`/api/v1/devboxes/*`) + HTML dashboard, document store (SQLite dev / Aurora DSQL prod), ASG-adopting pool reconciler, AWS compute layer |
| `devbox-cli`    | `claim` / `release` / `rename` / `list` / `status` / `ssh` |
| `devbox-agent`  | On-host binary baked into the AMI: `principals` (sshd resolver), `owner-sync` (provision the claimant's account), `warmup` (self-tags `devbox:ready=true` once warmed), `checkout` (clone repos into `/workspace`). musl static; built/released by CI, downloaded into the golden AMI |

**Pool management is ASG-based and the reconciler is adopt-only.** The Launch
Template and ASG are **provisioned by Terraform** in `devbox-infra`; there is no
launch lifecycle hook. The reconciler **adopts** the ASG by name (skipping the
tick if it is absent) and owns only runtime state — `DesiredCapacity =
min(claimed_count + ASG min_size, ASG max)`, per-instance scale-in
protection, owner tagging, and termination. The warm-pool target is the ASG's
`min_size` (read live each tick), so Terraform is the single source of truth for
pool sizing. Instance metadata (type/AMI/subnet) is read from `DescribeInstances`,
not config. It runs as a leader-locked
background loop (only one replica acts at a time) and syncs `DevboxDoc` records
against ASG membership each tick. The host's `devbox-agent warmup` sets the
instance tag `devbox:ready=true` once the host is ready; the reconciler then
marks the `DevboxDoc` `Ready`. Boxes that never tag ready within `ready_timeout`
(default 300 s, env `POOL_READY_TIMEOUT_SECS`) are terminated by the reconciler
and the ASG relaunches a replacement. Every box the reconciler creates is given
a unique, memorable `adjective-noun` **name** (e.g. `calm-quilt`) — generated in
`crates/devbox-server/src/naming.rs` from `aws_lc_rs::rand`. The name is shown in
the dashboard/CLI and is a global selector: `devbox ssh|release|status|rename <name>`
resolves a box by name (or id). A claimant may override it via the claim body's
optional `name` (validated by `is_valid_devbox_name`); uniqueness across
non-terminated boxes is enforced atomically by
`DocumentStore::compare_and_update_unique`. There is no backfill — docs written
before the field existed keep an empty name and age out as the pool churns.

> There are no active `.kiro/specs/` left — this file plus the Terraform in
> `devbox-infra` (the `image-builder`, `pool`, and `control-plane` modules) are the
> source of truth. The golden-AMI pipeline, the adopt-only reconciler, the Vouch-CA
> access model, and the Terraform/control-plane boundary are described above and
> realized in those modules.

## Commands

```bash
make build      # release build (includes CSS)
make fmt        # cargo fmt --all
make lint       # cargo clippy --all-targets --all-features -- -D warnings
make test       # unit tests, --all-features (in-memory SQLite; no AWS needed)
make check      # cargo check
make help       # list every target with its description

# Run a single test (substring-matches the test path):
cargo test -p devbox-cli ssh::tests::profile_is_forwarded
cargo test -p devbox-server claim_marks_box_claimed   # any module/test substring

# Run components locally (ARGS is passed through; defaults to --help):
make run-server                        # devbox-server, loads .env, serves :3000
make run        ARGS="list"            # the `devbox` CLI
make run-agent  ARGS="principals dev"  # the on-host agent

# Server against an explicit SQLite file instead of .env:
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server          # serves http://localhost:3000
```

The dashboard CSS is compiled by the **TailwindCSS v4 standalone CLI (not npm)**
and embedded into the binary at compile time via `rust-embed`; `make build`
includes that step, so after changing CSS you must rebuild the binary. `make
test` needs neither Tailwind nor AWS — tests use a placeholder CSS asset and
in-memory SQLite.

## Conventions (enforced — see `.kiro/steering/code-conventions.md`)

- **Toolchain**: Rust `1.96.0`, edition 2024 (`rust-toolchain.toml`). Release
  profile is `lto`/`codegen-units = 1`/`opt-level = "z"`/`strip`/**`panic =
  "abort"`** — abort-on-panic is *why* the no-panic policy below is a hard lint,
  not a preference (a panic is a process abort in release).
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
| CLI (incl. `ssh` over SSM) | `crates/devbox-cli/src/main.rs` (Clap definitions), `crates/devbox-cli/src/command.rs` (handlers), `crates/devbox-cli/src/ssh.rs`, `crates/devbox-cli/src/ssm.rs` (native data channel) |
| On-host agent (principals / owner-sync / warmup / checkout) | `crates/devbox-agent/src/` |
| Server entry / config / shutdown | `crates/devbox-server/src/main.rs` |
| HTTP routes | `crates/devbox-server/src/routes.rs`, `crates/devbox-server/src/service.rs` (domain logic) |
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
| `POST /api/v1/devboxes/claim` | Claim a Ready devbox (body: optional `name` override; `owner` from token) |
| `POST /api/v1/devboxes/{id}/release` | Release a Claimed devbox (no body; `owner` from token) |
| `POST /api/v1/devboxes/{id}/rename` | Rename a Claimed devbox (body: `name`; `owner` from token) |
| `GET /api/v1/pool/metrics` | Pool counts vs target |
| `GET /` | HTML dashboard |

## Status: implemented vs planned

**Implemented:** API + CLI (claim/release/rename/list/status/**ssh**), document store over
SQLite/DSQL with optimistic concurrency, **adopt-only** ASG reconciler (adopts the
Terraform ASG by name, syncs membership, maintains desired capacity, scale-in
protection, `devbox:owner` tagging via `apply_pending_owner_tags`), graceful
shutdown, Tailwind-styled HTML dashboard, unit tests. **Tag-based readiness gate:** instances
auto-join the ASG (no launch lifecycle hook); `devbox-agent warmup` starts Docker,
freshens the snapshot-seeded repos under `/workspace`
(`crates/devbox-agent/src/freshen.rs`; see the "Workspace freshening" planned item
below for the infra half), then self-sets `devbox:ready=true` via `ec2:CreateTags`; the reconciler flips
`DevboxDoc` `Warming → Ready` on that tag; boxes that never tag ready within
`ready_timeout` (`POOL_READY_TIMEOUT_SECS`, default 300 s, validated 60–3600 s) are
terminated and the ASG relaunches them. **SSH/Vouch-CA path:** `devbox-agent` (principals resolver
+ per-principal account provisioning + warmup) baked into the AMI; Terraform `pool`
module provides the host instance profile (SSM core + `ec2:CreateTags` for
`devbox:ready`), `InstanceMetadataTags=enabled`, and sshd `AuthorizedPrincipalsCommand`
config. The CLI auto-selects the AWS profile for the SSM tunnel by matching the
control-plane account it reads from the discovery document's `aws_account_id`
extension (server env `AWS_ACCOUNT_ID`); see "AWS profile auto-selection" under
the access model. The companion `control-plane` Terraform sets `AWS_ACCOUNT_ID`
on the ECS task. **AMI rotation:** the Launch Template resolves
`resolve:ssm:/devbox/ami/latest`, and the `pool` module's EventBridge → SSM
Automation rolls unclaimed warm hosts onto a new AMI via an ASG instance refresh
(`ScaleInProtectedInstances = Ignore`, so Claimed hosts are skipped). **Deployment:**
the `control-plane` module provisions Aurora DSQL (IAM-auth, no static password),
an ECR repo, and the server on ECS/Fargate (arm64) behind an internal NLB. The
dashboard is gated by app-side OIDC login (session cookie; see `auth/jwt.rs`
`OidcConfig`), while the API uses bearer-token auth. **CI/CD is keyless and immutable:**
`.github/workflows/deploy.yml` assumes a GitHub-OIDC-federated role (from the
`control-plane` module) to push a commit-SHA-tagged image to ECR, register a new
ECS task-definition revision pinned to it, and roll the service — with the ECS
deployment circuit breaker auto-rolling-back a failed deploy. No static AWS keys.
**API auth is mandatory.** There is no unauthenticated path and no `owner` in
the request body — claim/release **always** bind `owner` to the authenticated
principal (the Unix login derived from the token's `email` claim), so every
mutating call maps to an identity. The CLI authenticates via **device-code OAuth
(RFC 8628) + anonymous Dynamic Client Registration (RFC 7591)**: `devbox login`
discovers the authorization server from `GET
/.well-known/oauth-protected-resource` (RFC 9728), self-registers a public
client with Vouch, and caches the resulting **`access_token`** in
`~/.config/devbox/config.json`, **scoped per server** (keyed by hostname, like
the Vouch CLI) so several servers stay logged in at once; that login also
records the server as `current_server`, so subsequent commands default to it and
`--server` only needs to be passed to target a different one (precedence:
`--server`/`$DEVBOX_SERVER` → remembered `current_server` →
`http://localhost:3000`). The device-code grant returns a standard OAuth
2.0 token response (RFC 8628 §3.5), whose token is an `access_token` — not an
OIDC `id_token`; Vouch issues a JWKS-verifiable RFC 9068 access token carrying
the `email` claim (the same token type the Vouch CLI's FIDO2 grant uses).
Subsequent `claim`/`release` send it as a
`Bearer` token (no token → the CLI errors "run `devbox login`"). The server also
accepts an ALB's `x-amzn-oidc-data` header (legacy path when fronted by an ALB).
Both paths are verified against the Vouch JWKS (issuer + signature + `email`
claim). **Security boundary:** any valid, unexpired Vouch token with an
`email` claim is accepted; **audience is intentionally not validated** because
each DCR-registered CLI install gets its own `aud` value (= its own `client_id`),
so there is no single audience to pin — there is no audience config knob (a
future tightening would use RFC 8707 resource indicators, pinning to the
server's own `resource`). Authorization is per-claim ownership, not
per-audience. The owner is derived through `username_from_email`, which gates on
`is_valid_unix_username` (`^[a-z_][a-z0-9_.-]*$`, ≤32 chars — the same rule the
host's `owner-sync` applies); a token whose `email` local part is not a valid
Unix login is rejected with a 401, so a misconfigured principal fails at claim
time rather than as a broken SSH login. The dashboard is a separate path:
optional app-side OIDC login (`AUTH_OIDC_CLIENT_ID`, `AUTH_OIDC_CLIENT_SECRET`,
`AUTH_OIDC_REDIRECT_URI`) with a session cookie, deriving the same email-based
owner. **OIDC endpoints are discovered, not configured:** the only OIDC knob is
`AUTH_OIDC_ISSUER` (default `https://us.vouch.sh`); the JWKS URI and the dashboard
authorize / token / end-session endpoints are resolved once at startup from
`{AUTH_OIDC_ISSUER}/.well-known/openid-configuration` (bounded retry, fail-fast if
unreachable; the document's `issuer` is checked against the configured one). This
mirrors the CLI's discovery (`crates/devbox-cli/src/auth.rs`) — see
`crates/devbox-server/src/auth/discovery.rs`. Logout uses **OIDC RP-Initiated
Logout**: `/logout` clears the session cookie and redirects to the discovered
`end_session_endpoint` (`https://us.vouch.sh/oauth/logout` for Vouch) with the
cached id_token as `id_token_hint`, so the SSO session is terminated too (not just
the local cookie). Vouch redirects back to `/signed-out` — derived from
`AUTH_OIDC_REDIRECT_URI`'s origin, no separate env var — which must be registered
in the Vouch client's `post_logout_redirect_uris` (an unregistered URI falls back
to Vouch's own done page). **Every `/api/v1` endpoint requires authentication**,
reads included (list/get/pool metrics) — an unauthenticated API call is a 401,
never data. Only `/health` (infrastructure health checks present no credential)
and the RFC 9728 discovery document (fetched pre-login to bootstrap auth) are
open; the CLI's `list`/`status`/`ssh` therefore require `devbox login` too.

**Planned / not yet built** (ideas borrowed from [`.kiro/references.md`](.kiro/references.md)
are tagged inline):
- **Principal ↔ Unix-username alignment (operational)** — Server-side validation
  now rejects a non-Unix-safe `owner` at claim time (see "Owner validation"
  above), but the Vouch config must still be set so `AUTH_PRINCIPAL_CLAIM` emits a
  Unix-safe username (not the default UUID `sub`) that equals the SSH cert
  principal. Verify end-to-end (OIDC claim == cert principal == `owner-sync` account).
- **Workspace freshening (snapshot-seeded EBS workspace)** — *Agent half
  implemented:* `devbox-agent warmup` discovers git repos under `/workspace` and
  `git fetch` + hard-resets each to upstream HEAD before tagging ready
  (`crates/devbox-agent/src/freshen.rs`). The read-only credential is **minted
  in-agent** (`crates/devbox-agent/src/github_token.rs`): the agent reads the GitHub
  App private key from an **SSM SecureString** via the host instance profile, signs
  an RS256 JWT, and exchanges it for a fresh 1 h `contents:read` installation token
  (installation discovered per repo; config via `DEVBOX_GITHUB_APP_ID` /
  `DEVBOX_GITHUB_KEY_PARAM` / optional `DEVBOX_GITHUB_API_BASE`, read from the
  environment). Nothing baked, no
  off-box broker, no control-plane callback — the only stored secret is the App key
  in SSM (IAM + KMS gated). The fetch is time-budgeted (`WARMUP_FETCH_TIMEOUT_SECS`,
  default 120 s) and **degrades, does not reap** — a too-large delta, a mint/fetch
  failure, or an absent/empty `/workspace` (e.g. the EBS volume didn't mount, so the
  directory falls back to the root disk) still becomes Ready on whatever checkout is
  present.
  Freshness is **warming-time only** (no claim-time fetch / lazy write-gating — the
  claimant fetches HEAD themselves post-claim). *Still in `devbox-infra`:* the
  periodic snapshot-builder pipeline + `/devbox/workspace-snapshot/latest` SSM param
  + Launch Template block-device-mapping (per-instance volume, encrypted,
  `DeleteOnTermination=true`) that seeds the volume, the App-key SSM SecureString +
  `ssm:GetParameter`/`kms:Decrypt` on the host instance profile, and the GitHub
  egress allowlist (`api.github.com` + the git host). _(cf. Ramp Inspect)_
- **Warm dependency/build caches** — warm language caches (Rust `target/` +
  `CARGO_HOME`, Go/Node/Python equivalents) into the snapshot via a per-repo
  `.devbox/warm.sh` hook run by the snapshot-builder, with shared caches on the data
  volume and `RUSTUP_HOME`/`CARGO_HOME` etc. set system-wide (all on the
  `/workspace` volume) so the pinned toolchain and caches survive into the
  claimant's fresh home. Optional remote cache (sccache / Bazel) through the
  allowlist. _(cf. Ramp Inspect)_
- **Health-check gating of "warming"** — `devbox-agent warmup` already gates Ready on
  Docker + repo freshen; extend to network and richer health, plus **idle-claim
  reclaim**.
- **Durable agent sessions** — snapshot-on-release so a later follow-up restores
  even after the box is reclaimed. _(cf. Ramp Inspect)_
- **Allowlisting egress proxy** — route outbound through a controlled proxy that
  enforces allowlists and **injects per-claim VCS tokens**, instead of baking a
  shared `devbox/git-token` secret onto the box (today's `03-repos` credential
  helper). _(cf. WorkOS Horizon)_
- **Predictive / multi-pool warming** — pre-claim warming and pools keyed by
  profile/repo rather than one generic pool. _(cf. Ramp Inspect)_
- **Stop/resume long-lived claims** (persist EBS) as a cost lever. _(cf. WorkOS Horizon)_

## Related repositories

- **[`smoketurner/devbox-infra`](https://github.com/smoketurner/devbox-infra)** —
  Terraform for the AWS foundation (VPC, subnets, security groups, IAM, and the
  AMI pipeline) that this control plane runs on. Networking and IAM live there;
  pool/claim/lifecycle logic lives here.

## Related reading

External systems that inform devbox's roadmap — annotated with what we borrow — in
[`.kiro/references.md`](.kiro/references.md) (WorkOS Project Horizon, Ramp Inspect).

## Source of truth

`.kiro/steering/*` for conventions; this file plus the `devbox-infra` Terraform are
the source of truth (no active `.kiro/specs/` remain). The access model lives in
"Access model" above.
**When a doc disagrees with the code, trust the code and fix the doc.**
