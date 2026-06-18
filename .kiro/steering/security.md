# Security Rules

## Non-Negotiable

These rules apply to all code in this repository without exception:

1. **No secrets in code** -- Use environment variables or AWS Secrets Manager
2. **No unsafe code** -- Denied at the lint level
3. **No panics in production code** -- Denied at the lint level
4. **No raw SQL strings** -- Use sea-query builder for all queries
5. **No hardcoded AWS credentials** -- Use IAM roles and instance profiles
6. **No long-lived credentials** -- DSQL tokens are short-lived and auto-refreshed

## Instance Isolation

- Each devbox runs in its own EC2 instance with a dedicated security group
- Devboxes must not be able to reach the devbox-server management plane directly
- Network isolation via VPC subnets and security group rules
- No sharing of EBS volumes between active instances

## IAM and Authentication

- The devbox-server uses an IAM role with least-privilege permissions
- DSQL access uses IAM-generated auth tokens (short-lived, auto-refreshed)
- EC2 operations use the server's instance role (no static keys)
- API authentication will use IAM Signature V4 or bearer tokens (not yet implemented)

## SSH Access (Vouch CA)

Users and agents reach devboxes over **SSH** — the access path every remote IDE
(VS Code Remote-SSH, JetBrains Gateway, Cursor) requires.

- **Certificate-based auth via Vouch's SSH CA.** Vouch issues short-lived user
  certificates; devbox hosts trust the CA via `TrustedUserCAKeys`. There are **no
  `authorized_keys` files to manage** and no static keys baked into AMIs.
- **Dynamic per-claim authorization.** Claiming a devbox tags the instance
  `devbox:owner=<principal>` (applied by the reconciler). The host reads that tag
  from IMDSv2 (`InstanceMetadataTags=enabled`) via an `sshd`
  `AuthorizedPrincipalsCommand`, so a CA-signed cert is accepted only for the
  current claimant. The instance never calls back to the management plane,
  preserving the isolation rule above.
- **Identity contract:** the claim `owner` MUST equal the certificate principal
  Vouch mints. The principal is not secret; all trust derives from the CA
  signature. See [`../specs/ssh-access/`](../specs/ssh-access/).
- Disable password auth; SSH protocol 2 only; passwordless connections still
  flow through the CA + principals check.

## Data Handling

- Document data is stored as plain JSON (no client-side encryption needed for devbox metadata)
- No PII is stored in the document store beyond owner identifiers
- Database connections use TLS (rustls with aws-lc-rs backend)
- SQLite databases must not be accessible from the network

## Input Validation

- Validate all input from network or CLI
- Use typed wrappers (`DevboxId`, `DevboxState`) for validated data
- Never trust client-supplied instance IDs without verification against EC2
- Validate AMI IDs and subnet IDs against allowlists

## EC2 Security

- Only launch instances from approved AMI IDs
- Enforce IMDSv2 (require token for metadata access)
- Tag all instances for cost tracking and identification
- Terminate instances promptly when released (do not leave idle claimed instances)
- EBS volumes should be encrypted at rest

## No Production Access from Devboxes

Devbox instances are for development/testing only:
- Security groups must block access to production databases
- IAM roles on devbox instances must not have production write access
- Network ACLs must prevent lateral movement to production subnets
