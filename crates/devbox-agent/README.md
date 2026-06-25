# devbox-agent

The on-host agent for [devbox](../../README.md). A small, statically-linked
(musl) binary baked into the golden AMI that owns the **host side** of devbox's
SSH access model and warm-pool lifecycle. It accepts no inbound connections and
never calls the devbox control plane — it only reads its own instance metadata
(IMDS) and calls the AWS Auto Scaling API for its own instance, using the host
instance profile.

One binary, three subcommands, each wired to a different host trigger:

| Subcommand | Triggered by | Job |
|------------|--------------|-----|
| `principals <login-user>` | sshd `AuthorizedPrincipalsCommand`, per auth (as `nobody`) | Print the authorized principal, or nothing |
| `owner-sync` | `devbox-owner-sync.service` (systemd) | Provision the claimant's Unix account + git identity, then exit |
| `warmup` | `devbox-warmup.service` (systemd, at boot) | Freshen `/workspace` repos, then self-tag `devbox:ready=true` |

## `principals` — per-claim SSH authorization

sshd runs `devbox-agent principals %u` on every authentication. A devbox is
generic until claimed, so authorization is bound to the `devbox:owner` instance
tag (the claimant's Vouch principal, applied by the reconciler on claim). The
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

It then reads the `devbox:owner-email` tag (set by the reconciler from the
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

**Credential — minted in-agent, nothing baked.** An installation token lives only
an hour, so it can't be an env var. At warm-up the agent reads the GitHub App
private key from an **SSM SecureString** (via the host instance profile), signs a
short JWT, and exchanges it for a fresh `contents:read` installation token used
only for the fetch (`src/github_token.rs`). The host instance profile therefore
needs `ssm:GetParameter` + `kms:Decrypt` on that parameter, and egress to the SSM
and `api.github.com` endpoints. Config is non-secret, via the environment:

- `DEVBOX_GITHUB_APP_ID` — App ID or Client ID (the JWT issuer).
- `DEVBOX_GITHUB_INSTALLATION_ID` — installation to mint against.
- `DEVBOX_GITHUB_KEY_PARAM` — SSM SecureString parameter holding the RSA PEM.
- `DEVBOX_GITHUB_API_BASE` — optional; defaults to `https://api.github.com` (set
  for GitHub Enterprise).

If these are unset the fetch runs unauthenticated (private repos won't freshen).
Other knobs:

- `WARMUP_FETCH_TIMEOUT_SECS` — overall fetch budget (default 120 s; keep it well
  under `POOL_READY_TIMEOUT_SECS`). If the delta can't land in time the box still
  becomes Ready on the snapshot-age checkout (**degrade, not reap**); a git child
  that overruns is killed so nothing mutates `/workspace` after readiness.

An absent or empty `/workspace` (e.g. the EBS volume didn't mount, so the directory
falls back to the root disk) simply skips freshening — the box still becomes Ready.

The minted token is read-only; the claimant's per-claim write credential is a
separate concern. Warming build/dependency caches into the snapshot is on the
roadmap.

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
