# devbox-agent

The on-host agent for [devbox](../../README.md). A small, statically-linked
(musl) binary baked into the golden AMI that owns the **host side** of devbox's
SSH access model and warm-pool lifecycle. It accepts no inbound connections and
never calls the devbox control plane — it only reads its own instance metadata
(IMDS) and calls the AWS Auto Scaling API for its own instance, using the host
instance profile.

One binary, five subcommands, each wired to a different host trigger:

| Subcommand | Triggered by | Job |
|------------|--------------|-----|
| `principals <login-user>` | sshd `AuthorizedPrincipalsCommand`, per auth (as `nobody`) | Print the authorized principal, or nothing |
| `owner-sync` | `devbox-owner-sync.service` (systemd) | Provision the claimant's Unix account + git identity, then exit |
| `warmup` | `devbox-warmup.service` (systemd, at boot) | Freshen `/workspace` repos, then self-tag `devbox:ready=true` |
| `checkout <urls>` | the snapshot-builder, or a developer/agent on a claimed box | Clone repos into `/workspace`, minting a read-only token per repo |
| `doctor` | operator on a claimed box, via `devbox ssh -- devbox-agent doctor` | Print a read-only diagnostic of warm-cache delivery |

## `principals` — per-claim SSH authorization

sshd runs `devbox-agent principals %u` on every authentication. A devbox is
generic until claimed, so authorization is bound to the `devbox:owner` instance
tag (the claimant's Vouch principal, applied inline at claim time). The
command reads that tag from IMDS and prints it **only if it equals the requested
login user `%u`** — which both authorizes the certificate principal and pins the
login account to it (so `ssh root@box` with an `alice` certificate is rejected).

Fail-closed: any error, an absent tag, or a mismatch prints nothing, so sshd
authorizes no principals and rejects the login.

## `owner-sync` — provision the login account

sshd resolves the target Unix account *before* running
`AuthorizedPrincipalsCommand`, so the account named after the principal must
already exist. `owner-sync` polls IMDS until the `devbox:owner` tag appears, then
creates that account (`useradd -m -G docker`, passwordless sudo, ownership of
`/workspace`) and **exits**. A devbox is claimed once and terminated on release,
so there is nothing to do afterwards; the unit uses `Restart=on-failure` so a
clean exit stays stopped.

It then reads the `devbox:owner-email` tag (set inline at claim time from the
claimant's token email, alongside `devbox:owner`) and writes their git identity —
`user.email` and `user.name` — into the new account's `~/.gitconfig`, so the first
commit is attributed correctly with no manual setup. Best-effort: an absent tag or
a `git` failure is logged, not fatal.

Polling is the only on-host option: there is no event for an instance-tag change,
and the isolation rules forbid the control plane from pushing to the box.

## `warmup` — self-tag the instance ready

The warm-pool handshake. The pool ASG has no launch lifecycle hook; instances
go `Pending → InService` on their own. `warmup` (a oneshot service at boot)
performs host-side preparation and then records readiness in the control plane
via an EC2 tag. **`devbox:ready=true` is set only when all steps succeed**:

1. `systemctl start docker` — fail-closed: if Docker fails, `warmup` exits
   with a non-zero status and the tag is never set.
2. Reads the instance id and region from IMDS.
3. Freshen the snapshot-seeded repos under `/workspace` to near-HEAD.
4. Calls `ec2:CreateTags` to set `devbox:ready=true` on its own instance.

The reconciler reads that tag on each tick and flips the `DevboxDoc` from
`Warming` to `Ready`; only Ready boxes can be claimed. If `warmup` fails the
agent exits with a non-zero status and the reconciler's reaper terminates the
box after `ready_timeout` — the ASG then relaunches a replacement. No
`ABANDON` signal is needed.

### Freshening `/workspace`

A warm box launches with `/workspace` seeded from a periodically-refreshed EBS
snapshot (provisioned by Terraform in `devbox-infra`), so the repos are present
but a little stale. Before tagging ready, `warmup` `git fetch`es the small delta
since the snapshot was cut and hard-resets each repo to its upstream HEAD, so a
claimant gets a near-HEAD checkout without paying a full clone at launch.

**Credential — server-backed, nothing baked.** An installation token lives only an
hour, so it can't be an env var. At warm-up the agent authenticates to the
control plane with an **AWS web-identity token** (STS `GetWebIdentityToken`, IAM
Outbound Identity Federation — a short-lived, AWS-signed OIDC JWT asserting this
instance's identity, with no static secret to steal) and asks the server to mint
a short-lived, repo-scoped, read-only GitHub App installation token (see
`src/control_plane.rs`). The GitHub App private key lives only on the control
plane, read from an SSM SecureString by the server; the host needs only
`sts:GetWebIdentityToken` and egress to `api.github.com`.

Configuration is non-secret and supplied via the environment:

- `DEVBOX_SERVER_URL` — the control-plane base URL (also the audience for the
  web-identity token). When unset the agent is not configured for server-backed
  minting and fetches unauthenticated.

If this is unset the fetch runs unauthenticated (private repos won't freshen).
Other knobs:

- `WARMUP_FETCH_TIMEOUT_SECS` — overall fetch budget (default 120 s; keep it well
  under `POOL_READY_TIMEOUT_SECS`). If the delta can't land in time the box still
  becomes Ready on the snapshot-age checkout (**degrade, not reap**); a git child
  that overruns is killed so nothing mutates `/workspace` after readiness.

An absent or empty `/workspace` (e.g. the EBS volume didn't mount, so the directory
falls back to the root disk) simply skips freshening — the box still becomes Ready.

The minted token is read-only; the claimant's per-claim write credential is a
separate concern. Warming build/dependency caches into the snapshot is the job of
`checkout`'s per-repo `.devbox/warm.sh` hook (see below), not warmup; warmup only
freshens and preserves the warmed `target/`. What is **still in `devbox-infra`** is
running the snapshot-builder on a schedule, the Launch Template block-device-mapping
that seeds `/workspace`, and `RUSTUP_HOME`/`CARGO_HOME=/workspace` set system-wide so
the toolchain and caches survive into the claimant's fresh per-principal home.

## `checkout` — clone and warm repositories

Seeds `/workspace`. Run by the **snapshot-builder** before a new AMI is cut (so the
warmed result rides the EBS snapshot), and on demand by a developer or agent to add
a repo to a claimed box. For each repo URL it:

1. Mints a per-repo read-only GitHub App installation token (same server-backed
   flow as warmup), then `git clone --filter=blob:none` into `/workspace/<name>`
   (10-min budget). A clone failure is **fatal** — no broken snapshot is published.
2. Runs the repo's `.devbox/warm.sh` hook if present (30-min budget) to pre-build
   dependency/build caches; a hook failure is logged, not fatal.
3. `git gc --quiet` to compact the object store (5-min budget, non-fatal).

This is the warming step that produces a warm snapshot. For the caches to survive
into the claimant's session, `RUSTUP_HOME`/`CARGO_HOME`/`target/` must live on the
`/workspace` volume — configured system-wide in `devbox-infra`, not here.

## `doctor` — diagnose warm-cache delivery

A one-shot diagnostic for debugging cold builds on a claimed box. Run via
`devbox ssh <box> -- devbox-agent doctor`. It checks:

- Whether `/workspace` is a separate mount (root-disk fallback = no caches)
- Where `RUSTUP_HOME`/`CARGO_HOME` resolve (on-volume vs. bypassed)
- Whether the cargo registry cache is populated
- For each seeded repo: presence of `target/` and the pinned toolchain
- The EBS volumes attached, with snapshot ids to compare against
  `/devbox/workspace-snapshot/latest`

Read-only and best-effort: every probe degrades to a printed note rather than
aborting.

## Design

- **Fully async** `#[tokio::main(flavor = "current_thread")]` app — IMDS and the
  AWS SDK are async, and the agent has no parallelism to exploit. `warmup` and
  `principals` run once and exit; `owner-sync` runs until it provisions, then
  exits.
- **IMDS** access goes through `aws_config::imds::Client` (manages the IMDSv2
  token + retries); a `404` (absent path/tag) maps to `None`.
- **No inbound, no control-plane callback** — the agent only reads its own IMDS
  and writes to its own EC2 instance tags (`devbox:ready` on warmup,
  `devbox:owner` read by `principals`).

## Build & install

Built as an `aarch64-unknown-linux-musl` static binary (the `agent` target in
`docker-bake.hcl` / the workspace `Dockerfile.build`) and published to a GitHub
release. The `devbox-infra` `image-builder` `04-devbox` component downloads it to
`/usr/local/sbin/devbox-agent` and installs the sshd config + systemd units.

```bash
make bake-agent            # musl build via Docker Bake
cargo build -p devbox-agent
```

See the [project CLAUDE.md](../../CLAUDE.md) "Access model" section for the full
SSH / Vouch-CA design.
