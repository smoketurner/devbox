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
| `owner-sync` | `devbox-owner-sync.service` (systemd) | Provision the claimant's Unix account, then exit |
| `warmup` | `devbox-warmup.service` (systemd, at boot) | Self-tag `devbox:ready=true` once the host is warmed |

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
3. Calls `ec2:CreateTags` to set `devbox:ready=true` on its own instance.

The reconciler reads that tag on each tick and flips the `DevboxDoc` from
`Warming` to `Ready`; only Ready boxes can be claimed. If `warmup` fails the
agent exits with a non-zero status and the reconciler's reaper terminates the
box after `ready_timeout` — the ASG then relaunches a replacement. No
`ABANDON` signal is needed. The local warming is light today; richer
pre-warming (e.g. snapshot-seeded caches) is on the roadmap.

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
