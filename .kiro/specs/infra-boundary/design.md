# Design Document: Infra / Control-Plane Boundary

**Status:** Active

## Overview

The Launch Template, ASG, and warm-up lifecycle hook move from reconciler-managed
(create/update on every tick) to **Terraform-provisioned, reconciler-adopted**. Terraform
owns *what the fleet is*; the control plane owns *how big it is right now and which
instances are in use*. The two never write the same field.

```
 devbox-infra (Terraform)                    devbox-server (reconciler)
 ────────────────────────                    ──────────────────────────
 Launch Template                             describe_asg (adopt by name)
   ImageId = resolve:ssm:/devbox/ami/latest  set_desired_capacity = min(claimed+warm, max)
   IMDSv2 + InstanceMetadataTags=enabled     set_scale_in_protection (Claimed = protected)
   encrypted EBS, SGs, base tags             tag_instance devbox:owner=<principal>
 ASG                                          terminate released instances
   Min/Max, subnets/AZs, health check        complete_lifecycle_action (warming -> ready)
   propagate_tags_at_launch = true           sync DevboxDoc <-> ASG membership
   ignore_changes = [desired_capacity] ◄──── (control plane owns desired_capacity)
 Warm-up lifecycle hook (name + heartbeat)
 EventBridge AMI-refresh rule + executor
 IAM (host role + scoped control-plane role)
```

## Division of fields (who writes what)

| Resource / field | Terraform | Control plane |
|---|---|---|
| Launch Template (all fields) | ✅ create/own | — (never touches) |
| ASG existence, Min/Max, subnets, health check, hook | ✅ | — |
| ASG `DesiredCapacity` | `ignore_changes` | ✅ `set_desired_capacity` |
| Per-instance scale-in protection | — | ✅ `set_scale_in_protection` |
| `devbox:owner` instance tag | — | ✅ `tag_instance` |
| Instance termination on release | — | ✅ `terminate_instance_in_asg` |
| Lifecycle action completion | — | ✅ `complete_lifecycle_action` |

The only shared resource is the ASG, and the only field the control plane writes on it is
`DesiredCapacity` (plus per-instance protection/tags), all covered by Terraform's
`ignore_changes = [desired_capacity]`.

## Adoption and ordering

- The reconciler looks up `devbox-pool-<pool_id>` via `describe_asg` each tick.
- **ASG absent** (Terraform not applied yet, or being recreated): log a warning, skip the
  tick, retry next interval. No crash-loop, no partial creation. This replaces today's
  "create if missing" behavior.
- `MaxSize` is read from the adopted ASG; the reconciler clamps desired capacity to it
  instead of carrying its own `max_pool_size`.

## AMI rotation (no control-plane involvement)

Because the Launch Template's `ImageId` resolves from `/devbox/ami/latest`:

1. **New launches are already current.** Any instance the ASG launches (warm-pool growth or
   refresh replacement) uses the latest AMI with no template change.
2. **Proactive roll of idle hosts** is driven by an EventBridge rule that starts an ASG
   **instance refresh** with `ScaleInProtectedInstances = Ignore`. Claimed hosts are
   scale-in-protected by the reconciler, so the refresh **skips them**; only unclaimed warm
   hosts are replaced. Claimed hosts adopt the new AMI naturally when released and replaced.

This reuses the existing protection signal as the "in use" marker. It is conservative:
protection tracks `Claimed` (not literal TCP sessions), but given the `ssh-access` design an
*unclaimed* host authorizes no SSH principals, so "unprotected" reliably means "no
connections." Full design lives in `../ami-image-builder/`.

## IAM split

| Role | Permissions |
|---|---|
| Control plane | `autoscaling:DescribeAutoScalingGroups`, `SetDesiredCapacity`, `TerminateInstanceInAutoScalingGroup`, `SetInstanceProtection`, `CompleteLifecycleAction`, `ec2:CreateTags` |
| Terraform (apply-time) | full LT/ASG/hook/EventBridge/IAM management (operator-run, not the running server) |
| AMI-refresh executor (Lambda/SSM Automation) | `autoscaling:StartInstanceRefresh`, `DescribeInstanceRefreshes`, `DescribeAutoScalingGroups` |

The control plane loses every `Create*`/`Update*` infra permission it has today.

## Migration note (control-plane code, separate effort)

The reconciler currently calls `ensure_launch_template` / `ensure_asg` /
`ensure_lifecycle_hook` (tick steps 1/3/4) and builds `LaunchTemplateConfig`/`AsgConfig`
from `ReconcilerConfig`. Realizing this design in code means: drop those three `Compute`
methods and their config builders, start the tick at `describe_asg` with adopt-or-skip
handling, read `max_size` from the ASG, and trim `ReconcilerConfig` to `pool_id`,
`server_id`, `target_warm_pool_size`, `lifecycle_hook_name`, and timing. This is tracked as
a follow-up against `asg-pool-management/`; this document is the design of record.
