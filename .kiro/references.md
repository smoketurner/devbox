# References

Curated external work that informs devbox's design. Each entry notes **what
devbox borrows** so the lesson is actionable, not just a bookmark.

---

## WorkOS — Project Horizon

<https://workos.com/blog/project-horizon>

An internal "autonomous code factory": an event-driven control plane that runs
coding agents in cloud sandboxes (they moved from GitHub Codespaces to Cloudflare
Containers + a Sandbox SDK), with a custom MCP context server and a
self-improvement loop.

**What devbox borrows**

- **Controlled egress with token injection.** Horizon proxies *all* outbound
  traffic so it can enforce allowlists, log, and *inject credentials without
  exposing them to the agent*. Validates devbox's "no arbitrary egress" stance and
  points the `devbox-infra` egress story (currently NAT + VPC peering, slated for
  Transit Gateway / Network Firewall) toward an **allowlisting egress proxy that
  injects per-claim tokens** rather than baking secrets onto the box.
- **Orchestration separate from execution.** Their control plane lives outside the
  sandbox and owns lifecycle/state; devbox already splits control plane (server +
  reconciler) from execution (instances) — keep that boundary crisp.
- **Pause/resume + deterministic destroy.** Devbox destroys on release (cattle).
  A **stop/resume** option (stop instance, persist EBS) is a cost lever worth
  considering for long-lived claims.
- **Prebuilt sandboxes + warm dependency caches.** Reinforces the golden-AMI +
  snapshot-seeded workspace direction.
- **Scoped, short-lived VCS tokens; co-authored commits.** Mint per-identity
  GitHub tokens at use time instead of a shared secret.

---

## Ramp — Why we built our background agent ("Inspect")

<https://builders.ramp.com/post/why-we-built-our-background-agent>

A remote background coding agent (~30% of merged frontend/backend PRs) built on
Modal Sandboxes with per-repo images and filesystem snapshots, Cloudflare Durable
Objects for per-session state, and GitHub App tokens for VCS.

**What devbox borrows**

- **Snapshot-seeded workspace on a short refresh cadence.** Ramp rebuilds per-repo
  images **every ~30 min** with repos cloned and deps installed, then spins
  sessions up from a **filesystem snapshot** (≤30 min stale) instead of a cold
  build. Directly shapes devbox's **snapshot-seeded EBS** item: maintain a
  periodically-refreshed EBS snapshot (pre-cloned repos + warm caches) and attach
  it at launch.
- **Lazy write-gating.** Sessions can **read files immediately** while a background
  `git` sync runs; **writes are gated** until sync completes. A good readiness
  model for warm-up / health-gating — claim feels instant, correctness preserved.
- **Pre-warming on intent + per-profile hot pools.** They warm a sandbox when the
  user *starts typing*, and keep hotter pools for high-traffic repos. Suggests
  **predictive / pre-claim warming** and **multiple pools keyed by profile/repo**
  (devbox pools are generic today).
- **Session durability via snapshot-on-completion.** After a run they snapshot so a
  later follow-up can restore even if the sandbox was reclaimed. Devbox tried this
  (**durable agent sessions**, #87) and **removed it**: it re-implemented `git
  push` with bespoke machinery and cut against the cattle-not-pets thesis. Not
  borrowed; do not re-propose.
- **Per-clone GitHub App tokens; user opens the PR (no self-approval).** Tokens are
  scoped per clone and PRs are attributed to the human, avoiding self-approval.
- **Observability wired in.** Sandboxes ship connected to Datadog/Sentry/etc.;
  devbox bakes the CloudWatch agent — extend toward the same "feels local"
  telemetry.

---

## Stripe — Minions (one-shot coding agents on devboxes)

<https://stripe.dev/blog/minions-stripes-one-shot-end-to-end-coding-agents>
([part 2](https://stripe.dev/blog/minions-stripes-one-shot-end-to-end-coding-agents-part-2))

Stripe's homegrown coding agents (1,000+ merged PRs a week) run on "devboxes" —
pre-warmed, isolated EC2 instances originally built for human engineers, claimable
in ~10 seconds, isolated from production and the open internet.

**What devbox borrows**

- **Same substrate for humans and agents.** Minions run on the exact environment
  human engineers use, not a separate agent-only sandbox — direct validation of
  devbox's core thesis.
- **No-arbitrary-egress is what unlocks unattended agents.** Stripe enforces it in
  production with [smokescreen](https://github.com/stripe/smokescreen), their
  allowlisting egress proxy — the enforcement layer the devbox-infra egress plan
  adopts ([smoketurner/devbox-infra#16](https://github.com/smoketurner/devbox-infra/issues/16)).
- **Caches, not boot, make a box hot — and warmth is measured.** A pre-warmed box
  is only fast if code, deps, and build artifacts are already present. Shaped the
  warm/cold probe ([#79](https://github.com/smoketurner/devbox/issues/79)); full
  claim-to-first-build measurement was considered and declined
  ([#86](https://github.com/smoketurner/devbox/issues/86)) in favor of the probe.

---

## Joe Magerramov — "Disposable Environments, Durable Sessions"

<https://blog.joemag.dev/2026/01/disposable-environments-durable.html>

An ideal agentic workflow: environments are declarative, reproducible, and
disposable; the *session* — the ongoing developer↔agent collaboration — is the
durable thing. The environment earns deletion only after its work is pushed.

**What devbox borrows**

- **The session, not the disk, is the durable thing.** Sharpens cattle-not-pets:
  durability belongs to git (push a WIP branch before releasing), not to instance
  state. Devbox built S3-archived durable sessions on this framing (#87) and
  removed them (#91) — the archive machinery re-implemented `git push`; the
  insight survives as "WIP durability is git's job."
- **Declarative, versioned environment templates.** The golden AMI pipeline plus
  the per-repo `.devbox/warm.sh` hook are devbox's version of the reproducible,
  never-think-about-it template.

---

## exe.dev - Devbox and Sandbox (hosted computers for developers and agents)

<https://exe.dev/devbox>

Hosted Cloud Hypervisor VMs sold as a product: copy-on-write `cp` forks a 40 GB
base box in ~0.4s, idle boxes auto-stop to disk and resume in seconds, a GitHub
App plus in-VM proxy keeps credentials off the box entirely, every box gets a
private TLS hostname, and a web coding agent (Shelley) is bundled. The closest
thing to devbox's felt experience, delivered as someone else's cloud.

**What devbox borrows**

- **SSH as the entire API.** `ssh exe.dev <command>` is the whole control plane -
  zero client install. Devbox should expose `claim`/`list`/`status`/`release` as
  forced-commands on the CA-trusted SSH endpoint so the CLI becomes optional
  sugar. This one move accounts for most of exe.dev's "just works" feel.
- **Secrets live at the proxy, generalized.** Their integration model is header
  injection for *any* bearer-token API (Stripe, OpenAI, ...), scoped so clones
  inherit it, with keys never on disk or in env
  (<https://blog.exe.dev/http-proxy-secrets>). Extends the WorkOS entry and
  devbox-infra#16 beyond GitHub into a general per-claim credential plane.
- **Auto-stop as the default lifecycle, not a feature.** Claimed-but-idle boxes
  should stop to EBS and resume on SSH; released boxes still terminate. Refines
  the Horizon pause/resume note into concrete semantics.
- **Box-per-task parallelism as the primary workflow.** One fork per agent, many
  at once. Argues for named, concurrent claims per owner and per-profile pools
  (pairs with the Ramp entry).

**Not borrowed:** CoW instant forks (no EC2 primitive exists; the warm pool is
the EC2-shaped substitute - see the CodeSandbox entry), hosted multi-tenancy,
and bundling an agent.

---

## Ona (Gitpod) - "We're leaving Kubernetes"

<https://ona.com/stories/we-are-leaving-kubernetes>

Six years and 1.5M users running dev environments on Kubernetes, concluding it is
the wrong substrate: dev environments are exceptionally stateful, have
unpredictable resource usage, and need far-reaching permissions that fight k8s's
design at every turn.

**What devbox borrows**

- **Provenance for full-VMs-not-k8s.** This is the standing, receipts-included
  answer to "why isn't devbox a Kubernetes operator?" Link it from any spec or
  discussion where that question resurfaces.
- **State is the hard part.** Their years of PVC/eBPF/shiftfs/FUSE dead-ends are
  the argument for boring primitives: EBS volumes and AMIs over clever
  filesystem indirection.
- **A market signal, not just a design lesson.** Gitpod has since renamed to Ona,
  "mission control for software engineering agents" - the incumbent CDE vendors
  are moving up-stack to orchestration, leaving the substrate layer open.

---

## CodeSandbox - cloning running VMs via memory snapshots

<https://codesandbox.io/blog/how-we-clone-a-running-vm-in-2-seconds>

Firecracker memory snapshot/restore plus copy-on-write storage: clone a *running*
VM, memory and all, in ~2 seconds. The ceiling of the fork-latency design space,
and what exe.dev-class products build on.

**What devbox borrows**

- **Name the ceiling honestly.** EC2 tenants cannot fork a running instance. The
  nearest primitives are the warm pool (chosen), EC2 Hibernate, and EBS Fast
  Snapshot Restore - each with different costs. Documenting this keeps the warm
  pool honest about what it compensates for.
- **Evaluate Hibernate for claimed-box stop/resume.** Pairs with the exe.dev
  auto-stop item: Hibernate is a 1:1 stop/resume primitive, not a clone
  primitive, but it may beat cold re-claim for long-lived sessions.
- **FSR is for the golden snapshot only.** Per-snapshot-per-AZ-hour pricing makes
  FSR-backed per-branch forks uneconomic; use it (if at all) on the one snapshot
  everything launches from.

**Not borrowed:** running our own hypervisor on `.metal` instances to get true
forks. That turns devbox into "a custom cloud inside your cloud" and forfeits the
reviewable-plain-AWS-primitives property that makes self-hosted adoption viable.

---

## Coder - the incumbent self-hosted CDE

<https://coder.com>

The established self-hosted cloud development environment: Terraform-templated
workspaces, enterprise RBAC/SSO/audit, and (recently) agent-oriented features.
The product every enterprise evaluator will compare devbox against.

**What devbox borrows**

- **The enterprise buyer's checklist.** Template governance, RBAC, SSO, audit
  logging, and chargeback are procurement table stakes. Devbox doesn't need all
  of them at once, but the roadmap should acknowledge each deliberately.
- **The contrast, written down.** Coder is pets-with-lifecycle for humans; devbox
  is cattle for humans *and* agents, with durability delegated to git. If this
  difference isn't documented, evaluators will conclude devbox is a worse Coder
  rather than a different thesis.

---

## Dev Container spec + Codespaces prebuilds

<https://containers.dev>

The ecosystem standard for declarative per-repo environment definitions
(`devcontainer.json`), plus GitHub Codespaces' prebuild-on-push model that keeps
environments warm against the default branch.

**What devbox borrows**

- **Make interop a decision, not an accident.** Either adopt `devcontainer.json`
  as the per-repo layer on top of the golden AMI (features, post-create hooks
  mapping to `.devbox/warm.sh`), or write the ADR for why not. Silence here reads
  as ignorance of the standard.
- **Prebuild triggers map to snapshot refresh.** Codespaces rebuilds prebuilds on
  push to tracked branches - the same cadence thinking as Ramp's ~30 min snapshot
  refresh, expressed as an ecosystem norm.

---

## The hosted agent-sandbox cohort - E2B, Daytona, Modal, Fly Sprites

<https://e2b.dev> · <https://daytona.io> · <https://modal.com> · <https://fly.io>
(landscape: <https://northflank.com/blog/self-hosted-ai-sandboxes>)

SDK-first code-execution sandboxes with 90ms-to-sub-second creates. Daytona is
open source and self-hostable (container-shaped, real ops burden); E2B's
self-hosting is not production-ready for most teams and its BYOC is AWS-only and
enterprise-gated. All are session/SDK products, not machines.

**What devbox borrows**

- **The latency vocabulary buyers now expect.** Sub-second claim is the right
  ballpark and devbox already hits it; do not chase 90ms - that race belongs to
  microVM vendors.
- **The gap they leave is the lane.** None of them offers SSH-native machines on
  the customer's own EC2, shared by humans and agents. Daytona is the nearest
  self-hosted option and it is sandbox-shaped, not devbox-shaped.
- **SDK expectations, answered differently.** Orchestrators will expect a
  programmable interface; devbox's answer is MCP + SSH rather than a bespoke SDK.

---

## Anthropic - sandbox-runtime (Claude Code sandboxing)

<https://github.com/anthropic-experimental/sandbox-runtime>

OS-level confinement for agents: filesystem isolation plus per-process network
proxying, as used by Claude Code's sandboxed mode. Defense in depth *inside* the
box, beneath the VPC/egress layer.

**What devbox borrows**

- **A confinement profile on the box.** A `devbox-agent sandbox` mode (or baked
  bubblewrap profiles) so a rogue or prompt-injected agent cannot read the box's
  own credentials, other checkouts, or the agent binary's config.
- **Complements, never replaces, network egress control.** The smokescreen-style
  allowlisting proxy (Stripe entry) remains the outer wall; per-process
  confinement is the inner one. Two layers, different failure modes.

---

## AWS - Bedrock AgentCore Runtime (hosting coding agents)

<https://aws.amazon.com/blogs/machine-learning/its-safe-to-close-your-laptop-now-hosting-coding-agents-on-amazon-bedrock-agentcore/>

AWS's managed answer to this exact space: per-session isolated microVMs hosting
Claude Code / Codex / Kiro / Cursor with persistent workspaces, interactive PTY
shells over WebSocket, AgentCore Identity, Cedar-based Policy, and CloudTrail
integration. "Close the lid, the agent keeps working."

**What devbox borrows**

- **Category validation, and the redrawn uniqueness line.** AWS entering proves
  the market. Devbox's line is now: SSH-native (real IDEs attach), your own AMI
  and toolchain, humans and agents on one fleet, agent- and model-agnostic, open
  source, zero Bedrock coupling.
- **Session-scoped identity + policy outside the agent.** Their Identity/Policy
  split is the governance shape to match with devbox's per-claim principals,
  scoped token minting, and egress policy - implemented on plain AWS primitives
  the customer already audits.
- **The escape hatch as a differentiator.** Delete devbox and you still own
  working EC2, an AMI pipeline, and sshd config. Delete AgentCore and you own
  nothing. Say this explicitly in positioning.
- **VPC-native egress vindicated.** AgentCore's sandbox network mode shipped with
  a DNS exfiltration hole, and AWS's guidance was to use VPC mode for real
  traffic control - devbox's default posture from day one.

---

## How to use this file

When adopting an idea here, link back to the entry from the relevant spec or PR so
the provenance stays attached. Add new references with the same **what we
borrow** framing — a reference without a takeaway is just a bookmark.
