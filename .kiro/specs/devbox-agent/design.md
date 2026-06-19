# Design Document: Devbox Host Agent

**Status:** Active (design now; binary built later)

## Overview

`devbox-agent` is a small Rust binary that runs **on the devbox instance** (baked into the AMI), not
in the control plane. It turns the per-claim `devbox:owner` IMDS tag into local host state without the
box ever calling the management plane. Two responsibilities today:

1. **Just-in-time user provisioning** — create the claimant's UNIX account when the box is claimed.
2. **Principals resolver** — the sshd `AuthorizedPrincipalsCommand` target.

Planned crate: `crates/devbox-agent` (workspace member, no-panic conventions). Built and tested in this
repo; installed onto the AMI by the Image Builder pipeline in `devbox-infra`.

## Why an on-box agent (and why pre-create, not NSS)

A generic warm box has no login user until it is claimed, and the claim arrives *after* boot (the
reconciler tags an already-running instance). So the account must be created in reaction to the tag.

The subtle constraint is that **sshd resolves the login user via `getpwnam` pre-authentication** and
rejects unresolved users (`authctxt->valid = 0`) to prevent user enumeration — so a PAM `useradd` hook
alone is too late on the first connection. Two ways to make the name resolve:

- **NSS module** that synthesizes the entry in-process — but it loads into *every* `getpwnam` caller
  (sshd, sudo, cron) and would need a network (IMDS) read inside `getpwnam`; that is a real hazard and
  a C-ABI `.so` to maintain. **Rejected.**
- **Pre-create the account** before anyone logs in, so `getpwnam` hits plain `/etc/passwd`. **Chosen.**

The agent pre-creates, driven by a systemd timer reading the owner tag. Trade-off: a bounded delay
between claim and account readiness (the poll interval), which is acceptable because users connect
seconds after claiming and the interval is tunable. No NSS module, no network call inside sshd.

## Provisioning flow

```
ASG launches box ──> Warming ──> Ready (unclaimed: no devbox:owner tag, NO login user)
                                          │
              claim ──> reconciler tags box devbox:owner=<owner>
                                          │  (systemd timer, bounded interval)
                       devbox-agent provision:
                         - IMDSv2 token -> read tag /latest/meta-data/tags/instance/devbox:owner
                         - validate <owner> as a safe Linux username
                         - if absent: useradd -m -s /bin/bash <owner> + passwordless-sudo group
                                          │
                ssh <owner>@box ──> sshd getpwnam(<owner>) resolves from /etc/passwd
                                ──> cert signed by Vouch CA?  AuthorizedPrincipalsCommand(<owner>)
                                    returns <owner>?  ──> session opens as <owner>
```

Both the **account** and the **principal authorization** key off the same `devbox:owner` tag, so an
unclaimed box has neither a login user nor an authorized principal.

## Components (CLI subcommands)

- `devbox-agent provision` — run by a systemd oneshot + short-interval timer (as root). Idempotent;
  reads the tag, validates the principal, `useradd`s if missing. No-ops when unclaimed or already
  provisioned.
- `devbox-agent principals <login-user>` — the sshd `AuthorizedPrincipalsCommand` target
  (`AuthorizedPrincipalsCommandUser nobody`). Reads the tag; prints `<owner>` iff `<login-user>` ==
  owner; else prints nothing (fail-closed). Replaces the standalone `devbox-principals` shell script
  described in `../ssh-access/`.

Shared internals: one IMDSv2 client (token fetch + tag read), one principal-validation routine, used by
both subcommands.

## Security considerations

- **Isolation preserved:** the agent only reads the box's own IMDS; it never calls the control plane.
- **Fail-closed:** any IMDS error, missing tag, or invalid principal ⇒ no user / no principal output.
- **Username validation:** the principal is validated against a strict charset/length before being
  passed to `useradd`, so a malformed tag cannot inject shell/`useradd` arguments.
- **Least privilege:** `provision` runs as root (it must `useradd`); `principals` runs as `nobody`.
- **Ephemeral:** accounts vanish with the instance on release; no GC path to get wrong.
- The agent **provisions and identifies**; it does not authenticate — trust still derives from the
  Vouch CA signature on the certificate.

## Relationship to other specs

- `../ssh-access/` — defines the cert trust + principals model; this agent implements the resolver and
  the Linux user model it references.
- `../ami-image-builder/` — bakes the agent binary, its systemd unit, and the sudoers template into the
  AMI; enables `InstanceMetadataTags` (also required by the Launch Template, owned by Terraform).
- `../infra-boundary/` — the reconciler that applies the `devbox:owner` tag the agent consumes.

## Deferred build note

This document is the design of record. Implementation (the `crates/devbox-agent` binary + systemd unit
+ sudoers, and the AMI wiring in `devbox-infra`) is a tracked follow-up; nothing in the control-plane
server depends on it.
