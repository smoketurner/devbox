# Requirements Document: SSH Access with Vouch CA

**Status:** Active

## Introduction

Devbox instances are reached over **SSH**, because SSH is the access path every
remote IDE (VS Code Remote-SSH, JetBrains Gateway, Cursor) and terminal already
speaks. Authentication is **SSH certificate–based**: Vouch operates the SSH
Certificate Authority and issues short-lived user certificates; devbox hosts
trust that CA. This removes per-host key management (`authorized_keys`) entirely.

The open problem this spec solves is **per-claim authorization**: a CA-signed
certificate is only useful if the target host considers the certificate's
*principal* authorized for the login account. Devboxes are pre-warmed and
generic until claimed, so the authorized principal must be injected dynamically
at claim time — without the instance contacting the management plane (which the
isolation rules in `../../steering/security.md` forbid).

## Glossary

- **Vouch_CA**: The SSH Certificate Authority operated by Vouch; signs user certs.
- **User_Certificate**: A short-lived SSH certificate issued by Vouch carrying one
  or more **principals**.
- **Principal**: The identity name embedded in a certificate (e.g. `agent-42` or a
  human's username). Must match the devbox `owner`.
- **Owner_Tag**: The EC2 instance tag `devbox:owner=<principal>` applied to a
  claimed instance by the reconciler (`apply_pending_owner_tags`).
- **AuthorizedPrincipalsCommand**: An `sshd` directive naming a command that
  returns the principals allowed to log into a given account.
- **IMDS**: EC2 Instance Metadata Service (IMDSv2, token-required).

## Requirements

### Requirement 1: SSH certificate trust

**User Story:** As a platform operator, I want devbox hosts to trust Vouch-issued
certificates, so that no per-host key distribution is needed.

#### Acceptance Criteria

1. THE devbox AMI SHALL configure `sshd` with `TrustedUserCAKeys` pointing to the
   Vouch CA public key baked into the image at a fixed path (e.g.
   `/etc/ssh/vouch_ca.pub`).
2. THE devbox AMI SHALL disable password authentication and permit SSH protocol 2
   only.
3. THE devbox host SHALL accept a connection only when the presented certificate
   is signed by the Vouch CA AND carries a principal authorized for the login
   account (see Requirement 2).
4. THE devbox host SHALL NOT rely on any `authorized_keys` file for normal user
   access.

### Requirement 2: Dynamic per-claim principal authorization

**User Story:** As a coding agent or engineer, I want my certificate to grant
access only to the devbox I have claimed, so that access tracks claim ownership.

#### Acceptance Criteria

1. WHEN a devbox is claimed, THE reconciler SHALL apply the instance tag
   `devbox:owner=<principal>` (existing behavior: `apply_pending_owner_tags`).
2. THE Launch Template SHALL enable instance metadata tags
   (`InstanceMetadataTags=enabled`) so the `devbox:owner` tag is readable on the
   instance via IMDSv2.
3. THE devbox AMI SHALL configure `sshd` with `AuthorizedPrincipalsCommand` and a
   dedicated low-privilege `AuthorizedPrincipalsCommandUser` (e.g. `nobody`).
4. THE AuthorizedPrincipalsCommand SHALL read the `devbox:owner` tag from IMDSv2
   (acquiring a token first) and print exactly that principal, or print nothing if
   the tag is absent.
5. THE AuthorizedPrincipalsCommand SHALL NOT make any network call to the
   devbox-server management plane.
6. WHEN no `devbox:owner` tag is present (instance not yet claimed), THE host SHALL
   authorize no principals and therefore reject certificate logins.

### Requirement 3: Identity contract

**User Story:** As an operator integrating Vouch and devbox, I want the claim
identity and the certificate identity to be the same namespace, so authorization
is unambiguous.

#### Acceptance Criteria

1. THE `owner` field of a claim request SHALL equal the certificate principal that
   Vouch issues for that human or agent identity.
2. THE system SHALL treat the principal as non-secret; all access control derives
   from the Vouch CA signature plus the per-claim principal check.

### Requirement 4: Lifecycle

#### Acceptance Criteria

1. WHILE a devbox is in the `Claimed` state with the `devbox:owner` tag applied,
   THE host SHALL authorize the claimant's principal.
2. WHEN a devbox is released, THE instance SHALL be terminated (existing behavior);
   stale principal authorization is therefore discarded with the instance.
3. THE design SHALL NOT require recycling a claimed instance back to `Ready`.

## Out of Scope

- Issuing or rotating Vouch certificates (owned by Vouch).
- Network reachability of port 22 (governed by security groups / VPC design).
- Authenticating the **claim API** itself (tracked separately; claim currently
  validates ownership but not caller identity).
