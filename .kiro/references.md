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
  later follow-up can restore even if the sandbox was reclaimed — the concrete
  mechanism behind devbox's planned **durable agent sessions**.
- **Per-clone GitHub App tokens; user opens the PR (no self-approval).** Tokens are
  scoped per clone and PRs are attributed to the human, avoiding self-approval.
- **Observability wired in.** Sandboxes ship connected to Datadog/Sentry/etc.;
  devbox bakes the CloudWatch agent — extend toward the same "feels local"
  telemetry.

---

## How to use this file

When adopting an idea here, link back to the entry from the relevant spec or PR so
the provenance stays attached. Add new references with the same **what we borrow**
framing — a reference without a takeaway is just a bookmark.
