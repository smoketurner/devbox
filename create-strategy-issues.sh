#!/usr/bin/env bash
# Files the 2026 strategy backlog as GitHub issues on smoketurner/devbox.
# Review before running. Requires: gh auth login. Safe to re-run labels;
# re-running the whole script will create duplicate issues.
set -euo pipefail

REPO="smoketurner/devbox"

# --- labels (idempotent) ----------------------------------------------------
gh label create roadmap    -R "$REPO" --color 1D76DB --description "Strategic roadmap item"            2>/dev/null || true
gh label create adoption   -R "$REPO" --color 0E8A16 --description "Self-hosted adopter experience"    2>/dev/null || true
gh label create governance -R "$REPO" --color 5319E7 --description "Identity, policy, audit, attestation" 2>/dev/null || true

issue() { # title, labels, body
  gh issue create -R "$REPO" --title "$1" --label "$2" --body "$3"
}

# --- now: orchestrators as first-class claimants ----------------------------

issue "Expose the control plane as an MCP server (claim/status/release as tools)" "roadmap" "$(cat <<'EOF'
**Why:** The claimant is becoming software - orchestrators, ticket webhooks, and agents themselves will outnumber humans as claim sources. MCP is the interface they already speak.

**Proposal:** Serve MCP alongside the existing HTTP API in devbox-server: tools for `claim` (profile, name, ttl), `status`, `list`, `release`. Authn via the same token model as the CLI; service principals land in a separate issue.

**Provenance:** `.kiro/references.md` - "AWS - Bedrock AgentCore Runtime" and "The hosted agent-sandbox cohort" entries; `.kiro/steering/product.md` roadmap arc "now".

**Done when:** an agent with the MCP endpoint configured can claim a box, read its SSH target, and release it without the CLI.
EOF
)"

issue "Service principals for non-human claimants (quotas, budgets, audit)" "roadmap,governance" "$(cat <<'EOF'
**Why:** Orchestrators and agents need their own identities - claims attributed to a shared human token destroy the audit trail and make quota enforcement impossible.

**Proposal:** First-class service principals with per-principal claim quotas, optional spend budgets, and TTL defaults. Every claim records principal, spawning identity (if delegated), and originating task reference. Audit answers: which run, spawned by whom, touched what.

**Provenance:** `.kiro/references.md` - "AWS - Bedrock AgentCore Runtime" entry (Identity/Policy split); `.kiro/steering/product.md` "non-human identity" bullet.

**Done when:** a CI job claims under its own principal, hits its quota, and the claim ledger shows the full attribution chain.
EOF
)"

issue "Named, concurrent claims per owner + per-profile pools" "roadmap" "$(cat <<'EOF'
**Why:** The primary agent workflow is box-per-task, many at once. Today's one-active-claim model and generic pool block it.

**Proposal:** `devbox claim --name auth-fix --profile backend` with multiple simultaneous claims per owner; pools keyed by profile (instance type + snapshot lineage). CLI defaults resolve by name when ambiguous.

**Provenance:** `.kiro/references.md` - "exe.dev" entry (box-per-task parallelism) and "Ramp" entry (per-profile hot pools).

**Done when:** one owner holds three named claims from two profiles and `devbox list` disambiguates them.
EOF
)"

issue "SSH-native control plane: claim/list/release as forced commands" "roadmap" "$(cat <<'EOF'
**Why:** Zero-install is most of exe.dev's felt quality. Every target user already has an SSH client and (with Vouch or any CA) a certificate.

**Proposal:** A control-plane SSH endpoint where the forced command dispatches `claim`/`list`/`status`/`release`, authenticated by the same certificates used for box access. The CLI becomes optional sugar over the same verbs.

**Provenance:** `.kiro/references.md` - "exe.dev" entry (SSH as the entire API).

**Done when:** `ssh devbox.example.com claim` works from a machine that has never installed devbox-cli.
EOF
)"

# --- next: identity and lifecycle -------------------------------------------

issue "Generalize per-claim credential minting at the egress proxy" "roadmap,governance" "$(cat <<'EOF'
**Why:** GitHub tokens are one instance of a general pattern: credentials belong at the proxy, injected per claim, never on the box.

**Proposal:** Extend the planned allowlisting egress proxy (smoketurner/devbox-infra#16) with header-injection integrations scoped to claim identity: any bearer-token API, rotated centrally, inherited by profile. The box sees plaintext hosts; the proxy sees the world.

**Provenance:** `.kiro/references.md` - "WorkOS" entry (token injection), "exe.dev" entry (secrets live at the proxy), "Stripe" entry (smokescreen).

**Done when:** an agent on a claimed box calls an allowlisted API successfully with no credential present anywhere on the instance.
EOF
)"

issue "Idle sleep/wake for claimed boxes (stop-to-EBS, resume on demand)" "roadmap" "$(cat <<'EOF'
**Why:** Long-lived agent sessions wait - on CI, on rate limits, on human review. Terminating loses context; running idle burns money.

**Proposal:** Idle detection stops a *claimed* box (EBS persists); resume on SSH connect or webhook (CI completion, PR comment). Released boxes still terminate - cattle semantics unchanged. Evaluate EC2 Hibernate vs plain stop for resume latency.

**Provenance:** `.kiro/references.md` - "exe.dev" entry (auto-stop as default lifecycle), "CodeSandbox" entry (Hibernate evaluation), "WorkOS" entry (pause/resume).

**Done when:** a box idle 30 min stops automatically and an SSH attempt brings it back with workspace intact.
EOF
)"

issue "Spot-backed pool tier for agent claims" "roadmap" "$(cat <<'EOF'
**Why:** Fan-out economics. Fifty agent boxes at on-demand prices lose to CoW-fork vendors; cattle + git-owned durability makes agent claims uniquely spot-tolerant.

**Proposal:** A pool tier on spot capacity for agent-profile claims: interruption notice triggers `devbox-agent` to push WIP branch and release; reconciler replaces. Human-profile pools stay on-demand.

**Provenance:** `.kiro/steering/product.md` thesis pillar 3; `.kiro/references.md` - "Joe Magerramov" entry (durability is git's job).

**Done when:** a simulated spot interruption on a claimed agent box results in a pushed WIP branch and a clean replacement, with the event in the claim ledger.
EOF
)"

# --- then: verification and governance --------------------------------------

issue "Per-claim attestation receipts (signed provenance for agent-authored change)" "governance" "$(cat <<'EOF'
**Why:** As agents author more merged code, security teams will need to prove what produced a change. Nobody ships this; regulated buyers will be forced to want it, and it is only buildable inside the customer's trust boundary.

**Proposal:** On release, emit a signed receipt: AMI digest, instance id, claim principal chain, repo SHAs in/out, egress log digest, session-capture pointer. Store in the claim ledger; optionally attach to the PR (SLSA-style).

**Provenance:** `.kiro/steering/product.md` "verification" bullet; `.kiro/references.md` - "AWS - Bedrock AgentCore Runtime" entry.

**Done when:** a PR produced on a devbox carries a verifiable receipt an auditor can check against the ledger.
EOF
)"

issue "Identity-gated preview URLs per claim" "roadmap" "$(cat <<'EOF'
**Why:** Verification is the bottleneck: reviewers need to click the running app, not just read the diff. Must not violate the no-public-ingress stance.

**Proposal:** Per-claim internal DNS name routed through an identity-aware proxy tied to the same identity plane as SSH access. Private by default, shareable within the org. Infra half lands in devbox-infra.

**Provenance:** `.kiro/references.md` - "exe.dev" entry (per-box TLS hostnames); `.kiro/steering/product.md` "verification" bullet.

**Done when:** `https://<claim-name>.<internal-zone>` reaches port 3000 on the claimed box for an authenticated colleague and nobody else.
EOF
)"

issue "Session capture as a claim artifact" "governance" "$(cat <<'EOF'
**Why:** Replayable sessions are both an audit requirement and a verification aid - what did the agent actually do on the box?

**Proposal:** Surface SSM session logging (already available in the substrate) as a first-class claim artifact: enable per profile, store to the adopter's bucket, reference from the claim record and attestation receipt.

**Provenance:** `.kiro/steering/product.md` "verification" bullet; pairs with the attestation receipts issue.

**Done when:** a released claim's record links to a replayable capture of its sessions.
EOF
)"

issue "Autonomy policy profiles (attended / unattended / self-directed)" "governance" "$(cat <<'EOF'
**Why:** Orgs will ratchet agents from attended to autonomous via policy, not trust. The infrastructure should encode the ratchet.

**Proposal:** Named policy profiles binding: egress allowlist tier, credential scopes the proxy will mint, approval gates (e.g. push-to-main blocked), TTL/budget caps. A claim selects a profile; a security team promotes an agent by editing config.

**Provenance:** `.kiro/steering/product.md` "autonomy will be graduated" bullet; `.kiro/references.md` - "AWS - Bedrock AgentCore Runtime" entry (policy outside the agent).

**Done when:** the same agent claim behaves differently under two profiles with no agent-side changes.
EOF
)"

issue "On-box confinement profile via sandbox-runtime patterns" "governance" "$(cat <<'EOF'
**Why:** Defense in depth inside the box: a prompt-injected agent should not be able to read the box's own credentials, other checkouts, or agent config. Complements, never replaces, the egress proxy.

**Proposal:** A `devbox-agent sandbox` mode (or baked bubblewrap profiles) applying filesystem isolation and per-process network proxying to agent processes, following Anthropic's sandbox-runtime patterns.

**Provenance:** `.kiro/references.md` - "Anthropic - sandbox-runtime" entry.

**Done when:** an agent process under the profile cannot read `/etc/devbox` or escape the workspace subtree, verified by test.
EOF
)"

# --- throughout: adoption ----------------------------------------------------

issue "Pluggable SSH CA (remove the hard Vouch dependency)" "adoption" "$(cat <<'EOF'
**Why:** The default install path must require nothing but an AWS account. A mandatory third-party CA is a dependency the adopter's security team has not approved.

**Proposal:** Abstract the CA seam: bring-your-own CA public key + principal mapping, plus a minimal built-in issuance option for evaluation. Keep Vouch as the recommended, documented integration.

**Provenance:** `.kiro/steering/product.md` "self-hosted-first requirements".

**Done when:** a fresh install completes and authenticates SSH with no Vouch account.
EOF
)"

issue "One-command install: versioned distribution across devbox + devbox-infra" "adoption" "$(cat <<'EOF'
**Why:** Self-hosted wins or dies on the operator experience of a team that is not the author. The current two-repo handshake (build agent, publish release, pin SHA, bump recipe) is a lost-adopter machine.

**Proposal:** A versioned distribution: one Terraform module consuming pinned, co-tested artifacts (server image + agent binary + AMI recipe) released as a unit. Target: fresh account to first `claim` in under 30 minutes. Include the GovCloud/air-gap variant (mirrored artifacts, in-account ECR, no bootstrap egress) as a supported path.

**Provenance:** `.kiro/steering/product.md` "self-hosted-first requirements".

**Done when:** the quickstart is one `terraform apply` plus one `devbox claim`, timed under 30 minutes by someone who is not us.
EOF
)"

issue "Security-review package as a shipped artifact" "adoption,governance" "$(cat <<'EOF'
**Why:** For the target buyer, the infosec review is the purchase funnel. Handing them the package turns a one-quarter argument into a one-week approval.

**Proposal:** Ship in-repo and per-release: threat model, data-flow diagram, exact IAM policy manifest (the adopt-only reconciler makes this small and enumerable), SBOM, signed releases, and the escape-hatch statement (delete devbox, keep working machines).

**Provenance:** `.kiro/steering/product.md` "self-hosted-first requirements" and "escape hatch".

**Done when:** a security reviewer can complete an assessment from repo artifacts alone, without reading the source.
EOF
)"

issue "Day-2 operations package + README rewritten for the adopter persona" "adoption" "$(cat <<'EOF'
**Why:** Proof beats features for self-hosted. The operator needs a day-2 story, and the README currently addresses the author, not the platform team at a regulated company standing up an agent fleet.

**Proposal:** (1) Shipped CloudWatch dashboards, runbooks for the five likely failures, and a single-versioned upgrade path for control plane + agent + AMI recipe. (2) README rewrite leading with the adopter persona, the thesis pillars, and the AgentCore/exe.dev/Coder contrast from `.kiro/steering/product.md`.

**Provenance:** `.kiro/steering/product.md` positioning lines and self-hosted-first requirements.

**Done when:** an external pilot team operates a deployment for a month using only shipped docs; README opens with who this is for.
EOF
)"

issue "ADR: devcontainer.json interop with the golden AMI" "adoption" "$(cat <<'EOF'
**Why:** `devcontainer.json` is the ecosystem standard for per-repo environments. Adopting or declining should be a documented decision, not an accident - silence reads as ignorance of the standard.

**Proposal:** Spike + ADR: map devcontainer features/post-create hooks onto the golden AMI + `.devbox/warm.sh` layer, or document why not. Consider prebuild-on-push triggers as input to snapshot refresh cadence.

**Provenance:** `.kiro/references.md` - "Dev Container spec + Codespaces prebuilds" entry.

**Done when:** an accepted ADR exists and the README states the compatibility position.
EOF
)"

echo "Done. Review created issues at https://github.com/$REPO/issues"
