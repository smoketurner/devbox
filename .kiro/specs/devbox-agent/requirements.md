# Requirements Document: Devbox Host Agent

**Status:** Active (design now; binary built later)

## Introduction

The **devbox host agent** is a small program that runs **on each devbox instance** (baked into the
AMI), distinct from the control-plane server. It exists because some host-side behavior must key off
the per-claim `devbox:owner` instance tag *locally* — without the box ever calling the management
plane (per the isolation rules in `../../steering/security.md`).

Its first responsibility is **just-in-time Linux user provisioning**: a generic warm box has no login
user; when it is claimed, the agent creates the claimant's UNIX account so `ssh <owner>@box` resolves
normally. It also subsumes the `devbox-principals` resolver (the sshd `AuthorizedPrincipalsCommand`
target), consolidating all host-side IMDS/tag logic in one audited binary.

Planned as a new workspace crate **`crates/devbox-agent`**, built to the same no-panic conventions as
the rest of the workspace. **This spec is the design of record; the binary is implemented later.**

## Glossary

- **Agent**: the `devbox-agent` host binary.
- **Owner_Tag**: the EC2 instance tag `devbox:owner=<principal>` applied to a claimed instance by the
  reconciler (`apply_pending_owner_tags`).
- **Principal**: the Vouch certificate principal == the claim `owner`, used as the Linux username.
- **IMDS**: EC2 Instance Metadata Service (IMDSv2, token required).

## Requirements

### Requirement 1: Claim-driven user provisioning

**User Story:** As a claimant (human or agent), I want my UNIX account to exist when I SSH in, so that
login to a generic box succeeds without any per-host setup.

#### Acceptance Criteria

1. THE Agent SHALL read the `devbox:owner` tag from IMDSv2 (acquiring a token first).
2. WHEN the `devbox:owner` tag is present and the corresponding UNIX account does not exist, THE Agent
   SHALL create it idempotently: `useradd -m -s /bin/bash <owner>`, a home directory, and membership
   in a passwordless-sudo group.
3. WHEN the tag is absent (unclaimed box), THE Agent SHALL create no login user, so the box has no
   interactive account until claimed.
4. THE Agent SHALL be idempotent and safe to run repeatedly (no error if the account already exists).
5. THE Agent SHALL run as a systemd unit (oneshot triggered plus a bounded short-interval timer) so the
   account is ready within a small, tunable delay after claim; it SHALL NOT require a push from the
   management plane.
6. THE Agent SHALL validate the principal against a conservative charset/length before using it as a
   username (reject anything not a safe Linux username), failing closed.

### Requirement 2: Principals resolver

**User Story:** As the SSH daemon, I want a fast, fail-closed command that tells me which principal is
authorized for a login, so only the current claimant's certificate is accepted.

#### Acceptance Criteria

1. THE Agent SHALL provide a `principals` subcommand suitable for sshd `AuthorizedPrincipalsCommand`
   (invoked as e.g. `devbox-agent principals %u`), runnable as an unprivileged user (`nobody`).
2. THE subcommand SHALL read the `devbox:owner` tag from IMDSv2 and print exactly that principal when
   the requested login user matches it, and print nothing otherwise (fail-closed).
3. THE subcommand SHALL make no network call to the management plane and SHALL not write to disk.

### Requirement 3: Isolation and safety

#### Acceptance Criteria

1. THE Agent SHALL only ever read the box's own IMDS; it SHALL NOT contact the devbox-server control
   plane.
2. THE Agent SHALL treat the principal as non-secret; all access control derives from the Vouch CA
   signature plus the per-claim principal match (this agent only provisions/identifies, it does not
   authenticate).
3. Provisioned accounts are ephemeral: the instance is terminated on release, discarding them; the
   Agent SHALL NOT implement account garbage collection.

## Future scope (not required now)

- Warm-up readiness signaling (gate `Warming -> Ready` on a real health probe) could live in the agent.
- Replacing the standalone shell `devbox-principals` once the agent ships.

## Out of Scope

- SSH certificate issuance / the Vouch CA (Vouch).
- The AMI bake + systemd unit wiring (see `../ami-image-builder/`).
- Network reachability of port 22 (security groups, in `devbox-infra`).
