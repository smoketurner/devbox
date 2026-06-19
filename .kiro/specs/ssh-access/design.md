# Design Document: SSH Access with Vouch CA

**Status:** Active

## Overview

SSH is the access substrate for devboxes — it is what remote IDEs require and
what humans and agents already use. Authentication is certificate-based: **Vouch
runs the SSH CA** and issues short-lived user certificates; devbox hosts trust
the CA. The only devbox-specific problem is binding a generic, pre-warmed host to
the identity that just claimed it. We solve that with information the system
already produces — the `devbox:owner` instance tag — surfaced to `sshd` locally,
so the host never contacts the management plane.

## Authentication flow

```
Vouch CA ──signs──> User_Certificate (principal = "agent-42", short TTL)
                                  │
   user/agent: ssh dev@<devbox>   │ presents cert
                                  v
                         ┌──────────────────┐
                         │   devbox sshd    │
                         │ TrustedUserCAKeys│  1. cert signed by Vouch CA?  ──no──> reject
                         │                  │  2. principal authorized for "dev"?
                         │ AuthorizedPrincipals
                         │   Command         │        │
                         └─────────┬─────────┘        │ runs as `nobody`
                                   │                  v
                                   │      /usr/local/sbin/devbox-principals
                                   │        - GET IMDSv2 token (PUT, TTL header)
                                   │        - GET .../meta-data/tags/instance/devbox:owner
                                   │        - print the principal (or nothing)
                                   v
                    principal "agent-42" in output?  ──yes──> allow   ──no──> reject
```

## Components

### Host configuration (baked into the AMI)

`/etc/ssh/sshd_config.d/10-devbox.conf`:

```
TrustedUserCAKeys /etc/ssh/vouch_ca.pub
AuthorizedPrincipalsCommand /usr/local/sbin/devbox-principals %u
AuthorizedPrincipalsCommandUser nobody
PasswordAuthentication no
PubkeyAuthentication yes
Protocol 2
```

- `/etc/ssh/vouch_ca.pub` — the Vouch CA public key, baked at image-build time.
- A login account (e.g. `dev`) with a login shell and passwordless sudo, per the
  AMI spec.

### `devbox-principals` resolver (baked into the AMI)

A small, dependency-free script (shell or static binary). Pseudocode:

```sh
#!/bin/sh
# args: $1 = target login user (unused for now; single shared account)
set -eu
TOKEN=$(curl -sf -X PUT "http://169.254.169.254/latest/api/token" \
  -H "X-aws-ec2-metadata-token-ttl-seconds: 60") || exit 0
OWNER=$(curl -sf -H "X-aws-ec2-metadata-token: $TOKEN" \
  "http://169.254.169.254/latest/meta-data/tags/instance/devbox:owner") || exit 0
# Print the authorized principal. Empty output => no access.
printf '%s\n' "$OWNER"
```

Properties: runs as `nobody`, no writable paths, IMDSv2-only (token required,
matching the Launch Template's `HttpTokens=required`), no management-plane calls,
fail-closed (any error → empty output → no principals authorized).

### Management-plane side (already implemented)

- **Claim** sets `owner` and `owner_tag_applied=false` on the `DevboxDoc`
  (`routes.rs::claim_devbox`).
- **Reconciler** applies the tag: `apply_pending_owner_tags` calls
  `tag_instance(instance_id, &[("devbox:owner", owner)])` and flips
  `owner_tag_applied=true` (`reconcile/tick.rs`). No change required.

### The one code change required

Enable instance metadata tags on the Launch Template so the host can read the tag
via IMDS. In `compute/ec2.rs`, the metadata options builder currently sets:

```rust
LaunchTemplateInstanceMetadataOptionsRequest::builder()
    .http_tokens(LaunchTemplateHttpTokensState::Required)
    .http_put_response_hop_limit(2)
    // ADD:
    .instance_metadata_tags(LaunchTemplateInstanceMetadataTagsState::Enabled)
    .build()
```

This is additive and does not change pool behavior. (Tracked as implementation
work; this spec is the design of record.)

## Why this approach

- **Pull-based, no callback.** The host reads its own IMDS; it never connects to
  devbox-server, honoring "devboxes must not reach the management plane directly."
- **No open inbound beyond SSH.** No agent, tunnel, or push channel is required.
- **Reuses existing tagging.** The `devbox:owner` tag and its reconciler path
  already exist; only the Launch Template flag and host config are new.
- **Dynamic and fail-closed.** Unclaimed or mis-tagged hosts authorize nobody.
- **IDE-native.** Plain SSH means VS Code Remote-SSH / JetBrains Gateway work with
  no bespoke integration.

## Why a command and not user-data writing a file?

A natural question: why run `AuthorizedPrincipalsCommand` on every auth instead of
having **user-data read the `devbox:owner` tag once and write an
`AuthorizedPrincipalsFile`**? The answer is **lifecycle timing**.

In a warm pool, instances are launched and brought to `Ready` *before* anyone
claims them:

```
ASG launches ──> user-data runs ONCE ──> Warming ──> Ready   (unclaimed, NO devbox:owner tag)
                                                         │
                                  ... sits warm ...      │
                                                         ▼
        agent/human claims ──> reconciler tags the *already-running* box  devbox:owner=<principal>
```

User-data executes at first boot, when the box is still generic and **unowned** —
there is no `devbox:owner` tag to read. The owner is assigned later, by tagging a
box that is already running and warm. A one-shot user-data write would therefore
run minutes too early and capture nothing.

Because the trigger (the claim) happens *after* boot, a "write the file" approach
needs something to notice the tag change after user-data has finished — which means
either:

- a **polling daemon** on the box that watches IMDS and rewrites the file (a
  long-running, stateful service with a staleness window and a stale-file failure
  mode — strictly more moving parts than the hook), or
- a **push from the control plane** via SSM RunCommand (adds an SSM dependency and
  a per-claim management-plane action, and violates the rule that devboxes never
  receive a push from the management plane).

`AuthorizedPrincipalsCommand` avoids all of that: sshd invokes it lazily, only when
someone actually connects, and it reads the *current* tag at that instant — always
correct (even after re-tagging), no writer to keep in sync, no daemon, nothing
written to disk, and fail-closed. Note also that no sshd directive reads an EC2 tag
directly: `AuthorizedPrincipalsFile` only templates its *path* by `%u`, not by a tag
value, so the choice is genuinely command (pull) vs. writer (push). Given warm-pool
timing, pull wins.

This trade-off only flips in a **cold-launch-per-claim** model, where the owner is
known at launch and could be baked in by user-data or ASG tag-propagation-at-launch.
That model sacrifices the sub-second claim the warm pool exists to provide, so it is
out of scope here.

## Linux user model: host agent pre-creates the claimant account

Boxes are generic until claimed, so there is no login user to bake at AMI time. The chosen model: a
small **on-box host agent** (`devbox-agent`, see [`../devbox-agent/`](../devbox-agent/)) reads the
`devbox:owner` tag and **pre-creates the claimant's UNIX account at claim time** (`useradd -m -s
/bin/bash <owner>` + passwordless sudo), so `ssh <owner>@box` resolves from plain `/etc/passwd` and
`whoami` == the identity.

Why pre-create rather than create-at-login: sshd resolves the login user via `getpwnam`
**pre-authentication** and rejects unresolved users, so a PAM `useradd` hook is too late on first
login. The alternative that makes a not-yet-existing user resolve is an in-process **NSS module** — but
it loads into every `getpwnam` caller (sshd, sudo, cron) and would need an IMDS network read inside
`getpwnam`, a real hazard. Pre-creating before anyone logs in avoids NSS entirely. The cost is a
bounded delay between claim and account readiness (the agent's poll interval), which is acceptable
because users connect seconds after claiming.

The account and the principal authorization both key off the same `devbox:owner` tag, so an unclaimed
box has neither a login user nor an authorized principal. The `AuthorizedPrincipalsCommand` here is the
agent's `principals` subcommand (it supersedes the standalone `devbox-principals` script).

## Alternatives considered

- **User-data (or ASG tag-propagation) writing `AuthorizedPrincipalsFile` at
  launch** — runs before the box is claimed, so the owner tag does not yet exist;
  wrong lifecycle moment for a warm pool (see above). Rejected.
- **`AuthorizedPrincipalsFile` kept current by an on-box polling daemon** — more
  moving parts and a staleness window versus an on-demand command. Rejected.
- **`AuthorizedPrincipalsFile` pushed via SSM RunCommand at claim** — push-based,
  adds a management-plane action and SSM dependency per claim, and breaks the
  no-management-plane-callback isolation rule. Rejected.
- **EC2 Instance Connect** — delivers ephemeral keys but is AWS-API-mediated and
  doesn't model the per-claim principal cleanly alongside a CA. Rejected in favor
  of the Vouch CA.

## Security considerations

- Trust is rooted in the Vouch CA signature; the principal name is not a secret.
- Certificate TTLs are short (Vouch-managed); revocation is largely handled by
  expiry plus instance termination on release.
- The resolver must remain non-writable and run as an unprivileged user to avoid
  becoming an escalation path.
- Combine with security-group scoping of port 22; this spec covers authorization,
  not network reachability.

## Open questions

1. ~~Single shared login account (`dev`) vs. per-principal accounts.~~ **Resolved:**
   per-principal accounts, pre-created at claim time by the `devbox-agent` host agent
   (see the "Linux user model" section and [`../devbox-agent/`](../devbox-agent/)).
2. Should the resolver also assert the instance is in `Claimed` state, or is the
   presence of the `devbox:owner` tag sufficient? (Tag presence is sufficient given
   the reconciler only tags claimed instances.)
