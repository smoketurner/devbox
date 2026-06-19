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
| `warmup` | `devbox-warmup.service` (systemd, at boot) | Release the ASG launch lifecycle hook |

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

## `warmup` — release the ASG launch lifecycle hook

The warm-pool handshake. The pool ASG holds each newly launched instance in
`Pending:Wait` behind an `EC2_INSTANCE_LAUNCHING` lifecycle hook whose default
result is `ABANDON` — the box does **not** join the pool until it is signalled
ready. `warmup` (a oneshot service at boot):

1. Best-effort `systemctl start docker`.
2. Reads the instance id, region, and ASG name (`aws:autoscaling:groupName` tag)
   from IMDS.
3. Finds the launching hook via `DescribeLifecycleHooks`.
4. Calls `CompleteLifecycleAction` with `CONTINUE` — the instance moves to
   `InService`, and the reconciler then marks its `DevboxDoc` Ready.

On any failure it signals `ABANDON`, so the ASG terminates and replaces the
half-baked box. The local warming is light today (mainly the hook handshake);
richer pre-warming (e.g. snapshot-seeded caches) is on the roadmap.

## Design

- **Fully async** `#[tokio::main(flavor = "current_thread")]` app — IMDS and the
  AWS SDK are async, and the agent has no parallelism to exploit. `warmup` and
  `principals` run once and exit; `owner-sync` runs until it provisions, then
  exits.
- **IMDS** access goes through `aws_config::imds::Client` (manages the IMDSv2
  token + retries); a `404` (absent path/tag) maps to `None`.
- **No inbound, no control-plane callback** — the agent only reads its own IMDS
  and acts on its own instance.

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
