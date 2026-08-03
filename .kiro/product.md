# Product

Steering context for what devbox is, who it serves, and the deliberate
decisions that bound its design. Specs and PRs that touch positioning,
scope, or roadmap should stay consistent with this file or change it
explicitly.

## What devbox is

Tooling that lets a company run Stripe-style devboxes in its own AWS
account: a warm pool of ephemeral, isolated EC2 machines, claimable in
under a second over SSH, shared by human engineers and the coding agents
working alongside them. The control plane adopts plain AWS primitives
(ASG, EBS, SSM, sshd, IAM) that a security team can review in an
afternoon. It is a kit you install, not a service you trust.

## Who it is for

The platform or infrastructure team at a company that cannot or will not
put source code, credentials, and network egress on someone else's cloud:
regulated industries (defense, finance, healthcare, government),
data-sovereignty regimes, and any org whose infosec review is the real
purchase funnel. Secondary: teams that could use hosted products but want
the escape hatch and cost model of owning the substrate.

## Thesis (three pillars)

1. **Same substrate for humans and agents.** One AMI, one policy plane,
   one audit trail for the engineer's box and the agent's box. Validated
   by Stripe Minions; no competitor combines it.
2. **SSH is the only interface.** Every remote IDE and every CLI agent
   already speaks it. This keeps devbox agent-agnostic and model-agnostic
   where competitors bundle their runtime.
3. **Cattle, with durability delegated to git.** Boxes are disposable;
   the session's work survives as pushed branches. This also makes agent
   claims uniquely spot-tolerant.

## Where the market is going (and what it demands)

- **The claimant becomes software.** Orchestrators (tickets, CI events,
  other agents) will outnumber humans as claim sources. The control plane
  needs machine-first surfaces: MCP tools, service principals, quotas,
  budgets.
- **The bottleneck moves to verification.** As agents produce more
  changes, value accrues to making their work inspectable: preview URLs,
  session capture, and signed per-claim attestation receipts (AMI digest,
  repos in/out, egress log) that travel with the PR.
- **Non-human identity is the governance battleground.** Every claim
  binds a workload identity, scoped short-lived credentials, and an audit
  answer to "which agent run, spawned by whom, touched what."
- **Autonomy will be graduated.** Orgs ratchet from attended to
  unattended via policy, not trust. Autonomy levels are config: egress
  tiers, credential scopes, approval gates, budget caps.
- **Hosted vendors and clouds are converging on this space** (exe.dev,
  AWS Bedrock AgentCore, E2B, Daytona) while incumbent CDE vendors move
  up-stack to orchestration (Gitpod is now Ona). The substrate-in-your-
  own-account layer is the open lane.

## Positioning lines

- vs **exe.dev**: they sell computers on their cloud; devbox turns your
  cloud into their experience. Their moat is the hypervisor; ours is the
  customer's trust boundary.
- vs **AWS AgentCore**: managed microVM sessions, Bedrock-coupled,
  WebSocket shells, agent-only. Devbox is SSH-native, IDE-attachable,
  your own AMI, humans and agents together, open source. Delete devbox
  and you still own working machines; delete AgentCore and you own
  nothing.
- vs **Coder**: pets-with-lifecycle for humans vs cattle for humans and
  agents. Different thesis, not a feature gap.
- vs **E2B / Daytona / Modal**: SDK-shaped code-execution sandboxes vs
  SSH-shaped machines on your own EC2. Do not chase their fork latency.

One-line version: hosted vendors sell trust in themselves; devbox sells
not needing to extend trust at all.

## Deliberate decisions

- **AWS-only, by choice.** Depth over breadth: lean fully into ASG, SSM,
  IMDSv2, EBS, Image Builder, IAM, CloudTrail, GovCloud. Multi-cloud is
  an explicit non-goal; the adopt-only reconciler pattern keeps a future
  port possible without designing for it now.
- **Self-hosted-first requirements** (each is a workstream, not a
  tagline): one boring install (fresh account to first claim in under 30
  minutes); no mandatory third-party dependencies on the default path
  (SSH CA must be pluggable); a shippable security-review package (threat
  model, IAM manifest, SBOM, signed releases); a day-2 story (dashboards,
  runbooks, single-versioned upgrades); air-gap and GovCloud support.
- **Smallness is a guarded property.** A readable amount of Rust over
  plain AWS primitives - no operator, no CRDs, no embedded hypervisor.
  Auditability is a feature; scope creep is its enemy.
- **The escape hatch is a feature.** Everything devbox manages remains
  usable if devbox is deleted. Preserve this in every design.
- **Licensing stays Apache-2.0 OR MIT for the core.** Keep the core
  complete and unencumbered.

## Non-goals

- Being the orchestrator or dispatcher (Agent HQ, AgentCore harnesses,
  Ona fight there; devbox is the substrate they draw machines from).
- Bundling or favoring a specific coding agent or model.
- Hosted multi-tenant service.
- Copy-on-write instant forks or running our own hypervisor.
- Kubernetes as substrate or packaging (see Ona entry in
  `.kiro/references.md`).
- Agent-to-agent protocol integrations beyond exposing MCP.

## Roadmap arcs

- **Now - orchestrators as first-class claimants:** MCP server surface,
  service principals with quotas/budgets, named concurrent claims,
  SSH-forced-command control plane.
- **Next - identity and lifecycle:** generalized per-claim credential
  minting at the egress proxy, idle sleep/wake for claimed boxes,
  spot-backed agent pool tier.
- **Then - verification and governance:** per-claim attestation receipts,
  identity-gated preview URLs, session capture as claim artifact,
  autonomy policy profiles.
- **Throughout - adoption:** the self-hosted-first requirements above,
  plus a README written for the adopter persona.

Provenance for the ideas above lives in `.kiro/references.md`; link
entries from specs and PRs when adopting them.
