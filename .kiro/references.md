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
  (**durable agent sessions**, #87) and **removed it**: it re-implemented `git push`
  with bespoke machinery and cut against the cattle-not-pets thesis. Not borrowed;
  do not re-propose.
- **Per-clone GitHub App tokens; user opens the PR (no self-approval).** Tokens are
  scoped per clone and PRs are attributed to the human, avoiding self-approval.
- **Observability wired in.** Sandboxes ship connected to Datadog/Sentry/etc.;
  devbox bakes the CloudWatch agent — extend toward the same "feels local"
  telemetry.

---

## edjgeek — Claude Code Sandbox for iPad

<https://edjgeek.com/blog/claude-code-sandbox-for-ipad/>

A single-user Claude Code sandbox reachable from an iPad: Lambda MicroVMs (8GB/4vCPU
baseline, burstable) with a per-user home directory mounted from S3, Cognito + API
Gateway minting short-lived (55-minute, port-scoped) tokens, and a browser terminal —
xterm.js over a WebSocket to a Node PTY server (ttyd protocol) inside the VM — with
Claude Code preinstalled against Bedrock. Idle policy: suspend after 2 h of no proxy
traffic, resume from a memory snapshot within 30 min, terminate beyond that, 8 h hard
cap. Deployed as one SAM template.

**What devbox borrows**

- **Suspend-to-memory-snapshot as the idle state.** Their suspend/resume preserves
  full process state, not just disk — a sharper version of devbox's planned
  **stop/resume long-lived claims** item (EC2 hibernation is the rough equivalent;
  persisting EBS alone loses running processes).
- **Idle measured at the proxy.** Inactivity is inferred from ingress-proxy traffic
  rather than an on-box agent. Pairs with devbox's planned **allowlisting egress
  proxy**: the same proxy that injects per-claim tokens could drive **idle-claim
  reclaim** with no extra on-host machinery.
- **Hard-cap termination.** An absolute session lifetime (their 8 h) backstops any
  idle heuristic so forgotten claims can't accumulate cost indefinitely.
- **Not borrowed — browser-terminal access.** Their xterm.js/WebSocket path is what
  makes the iPad case work (iPadOS has no Remote-SSH IDE), but it forfeits the IDE
  ecosystem; devbox's deciding constraint is **SSH as the universal adapter**. An
  iPad against devbox is an SSH terminal app (Blink, Termius), not a browser tab.
- **Not borrowed — persistent per-user home (S3-mounted).** A pet home directory on
  cattle VMs cuts against the cattle-not-pets thesis; devbox built and removed the
  equivalent (durable agent sessions, #87). WIP durability is git's job.

---

## How to use this file

When adopting an idea here, link back to the entry from the relevant spec or PR so
the provenance stays attached. Add new references with the same **what we borrow**
framing — a reference without a takeaway is just a bookmark.
