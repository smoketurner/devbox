//! Reconciliation tick logic.
//!
//! The reconciler is adopt-only: Terraform provisions the Launch Template and
//! ASG (see CLAUDE.md). Each tick looks the ASG up by name, syncs `DevboxDoc`
//! records with its membership, observes the host-set `devbox:ready` tag to
//! transition Warming→Ready, reaps boxes that never become ready, and writes
//! only runtime state — desired capacity (clamped to the ASG's max), scale-in
//! protection, owner tags, and terminations.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use crate::compute::{Compute, InstanceInfo};
use crate::db::DocumentStore;
use crate::documents::devbox::DevboxDoc;
use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};

use super::config::ReconcilerConfig;

/// Compute the desired ASG capacity.
///
/// Formula: `min(claimed_count + target_warm_pool_size, max_pool_size)`
///
/// Uses `saturating_add` to avoid arithmetic overflow and `.min()` to clamp
/// the result to the configured maximum pool size.
///
/// # Returns
///
/// The desired capacity value, always in the range `[0, max_pool_size]`.
pub(crate) fn compute_desired_capacity(
    claimed_count: u32,
    target_warm_pool_size: u32,
    max_pool_size: u32,
) -> u32 {
    claimed_count
        .saturating_add(target_warm_pool_size)
        .min(max_pool_size)
}

/// Count documents matching a given state.
fn count_by_state(
    docs: &[crate::db::document_type::Document<DevboxDoc>],
    state: DevboxState,
) -> u32 {
    let count = docs.iter().filter(|d| d.data.state == state).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Execute a single reconciliation tick.
///
/// Adopt-only flow:
/// 1. Look up the ASG by name; skip the tick if it does not exist yet.
/// 2. Describe all ASG instances to build an id→InstanceInfo map (includes the
///    `devbox:ready` tag).
/// 3. Sync DevboxDoc records with ASG membership.
/// 4. Mark Warming instances Ready when `InstanceInfo.ready` is true.
/// 5. Reap Warming instances that have exceeded `ready_timeout`.
/// 6. Terminate Terminating instances and delete their docs.
/// 7. Recompute desired capacity (clamped to the ASG's max) and update.
/// 8. Update scale-in protection.
/// 9. Apply pending owner tags.
///
/// # Errors
///
/// Returns an error only if the document store is unreadable. A missing ASG is
/// logged and skipped (no crash-loop while Terraform is being applied); other
/// per-instance AWS failures are logged and the tick continues.
pub(super) async fn reconciliation_tick(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    config: &ReconcilerConfig,
) -> Result<()> {
    // Step 1: Adopt the Terraform-provisioned ASG by name; skip if absent.
    let asg_name = config.asg_name();
    let asg_desc = match compute.describe_asg(&asg_name).await {
        Ok(desc) => desc,
        Err(e) => {
            tracing::warn!(
                asg = %asg_name,
                error = %e,
                "ASG not found; skipping tick (is the pool Terraform applied?)"
            );
            return Ok(());
        }
    };

    // Step 2: Describe all instances in the ASG upfront to get the ready tag.
    //
    // This single call covers both new instances (for sync_docs_with_asg) and
    // already-warming instances (for handle_warming_instances and the reaper),
    // ensuring we read the `devbox:ready` tag for every instance regardless of
    // when it was first adopted.
    let all_ids: Vec<&str> = asg_desc
        .instances
        .iter()
        .map(|inst| inst.instance_id.as_str())
        .collect();
    let instance_infos = match compute.describe_instances(&all_ids).await {
        Ok(infos) => infos,
        Err(e) => {
            tracing::error!(error = %e, "failed to describe ASG instances; skipping tick");
            return Ok(());
        }
    };
    let info_by_id: HashMap<&str, &InstanceInfo> = instance_infos
        .iter()
        .map(|info| (info.instance_id.as_str(), info))
        .collect();

    // Step 3: Sync DevboxDoc records with ASG membership.
    sync_docs_with_asg(store, &asg_desc.instances, &info_by_id).await;

    // Re-read docs after sync to get fresh state.
    let all_docs = store.list_all::<DevboxDoc>().await?;

    // Step 4: Mark Warming instances Ready when the `devbox:ready` tag is present.
    handle_warming_instances(store, &all_docs, &info_by_id).await;

    // Step 5: Reap Warming instances that never became ready within ready_timeout.
    reap_unready_instances(store, config, &all_docs, &info_by_id).await;

    // Step 6: Handle Terminating instances.
    handle_terminating_instances(store, compute, &asg_name, &all_docs).await;

    // Step 7: Recompute desired capacity and update if changed.
    recompute_desired_capacity(store, compute, config, &asg_name, &asg_desc).await;

    // Step 8: Update scale-in protection.
    update_scale_in_protection(store, compute, &asg_name, &asg_desc.instances).await;

    // Step 9: Apply pending owner tags.
    apply_pending_owner_tags(store, compute, &all_docs).await;

    Ok(())
}

/// Step 3: Sync DevboxDoc records with ASG membership.
///
/// - Delete docs whose instance_id is NOT in the ASG (stale cleanup).
/// - Create new Warming docs for instances in a live lifecycle state with no doc.
///
/// All new docs are created in `Warming` state regardless of lifecycle state —
/// readiness is gated on the `devbox:ready` tag, not on `InService`. Creating
/// a doc Ready here would bypass the tag gate.
///
/// The creation filter accepts `Pending*` and `InService` states because, with
/// no lifecycle hook, instances go directly `Pending → InService`; the reconciler
/// may first observe them in either state.
async fn sync_docs_with_asg(
    store: &DocumentStore,
    asg_instances: &[crate::compute::AsgInstance],
    info_by_id: &HashMap<&str, &InstanceInfo>,
) {
    // Build set of instance IDs currently in the ASG
    let asg_instance_ids: HashSet<&str> = asg_instances
        .iter()
        .map(|inst| inst.instance_id.as_str())
        .collect();

    // Get current docs
    let docs = match store.list_all::<DevboxDoc>().await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to list docs for ASG sync");
            return;
        }
    };

    // Delete docs whose instance_id is not in ASG (stale cleanup)
    for doc in &docs {
        if let Some(ref instance_id) = doc.data.instance_id
            && !asg_instance_ids.contains(instance_id.as_str())
            && let Err(e) = store.delete(&doc.id).await
        {
            tracing::error!(
                error = %e,
                doc_id = %doc.id,
                instance_id = %instance_id,
                "failed to delete stale doc"
            );
        }
    }

    // Build set of instance_ids that already have docs
    let doc_instance_ids: HashSet<String> = docs
        .iter()
        .filter_map(|d| d.data.instance_id.clone())
        .collect();

    // ASG instances that need a new doc (only adoptable lifecycle states).
    // With no lifecycle hook, instances go Pending → InService directly; we
    // accept any Pending* variant and InService to catch either observation.
    let new_instances: Vec<&crate::compute::AsgInstance> = asg_instances
        .iter()
        .filter(|inst| !doc_instance_ids.contains(&inst.instance_id))
        .filter(|inst| {
            inst.lifecycle_state.starts_with("Pending") || inst.lifecycle_state == "InService"
        })
        .collect();

    if new_instances.is_empty() {
        return;
    }

    for inst in new_instances {
        let Some(info) = info_by_id.get(inst.instance_id.as_str()) else {
            tracing::warn!(
                instance_id = %inst.instance_id,
                "no DescribeInstances result; will retry next tick"
            );
            continue;
        };

        // Always create Warming: readiness is gated on devbox:ready tag, never
        // on lifecycle state alone. handle_warming_instances will flip to Ready
        // on the next tick once the tag is seen.
        let new_doc = DevboxDoc {
            instance_id: Some(inst.instance_id.clone()),
            state: DevboxState::Warming,
            instance_type: InstanceType(info.instance_type.clone()),
            ami_id: AmiId(info.ami_id.clone()),
            subnet_id: SubnetId(info.subnet_id.clone()),
            ebs_volume_id: None,
            owner: None,
            claimed_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
        };

        let doc_id = uuid::Uuid::now_v7().to_string();
        if let Err(e) = store.insert_with_id(&doc_id, &new_doc).await {
            tracing::error!(
                error = %e,
                instance_id = %inst.instance_id,
                "failed to create doc for ASG instance"
            );
        }
    }
}

/// Step 4: Transition Warming instances to Ready when `devbox:ready=true` is set.
///
/// The host's `devbox-agent warmup` sets the `devbox:ready=true` tag on its own
/// instance once warm-up is complete. The reconciler observes that tag (surfaced
/// in `InstanceInfo.ready`) and flips the corresponding `DevboxDoc` to Ready.
async fn handle_warming_instances(
    store: &DocumentStore,
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
    info_by_id: &HashMap<&str, &InstanceInfo>,
) {
    for doc in all_docs {
        if doc.data.state != DevboxState::Warming {
            continue;
        }

        let instance_id = match doc.data.instance_id {
            Some(ref id) => id.as_str(),
            None => continue,
        };

        let is_ready = info_by_id.get(instance_id).is_some_and(|info| info.ready);

        if !is_ready {
            continue;
        }

        let mut updated_doc = doc.data.clone();
        updated_doc.state = DevboxState::Ready;

        match store
            .compare_and_update(&doc.id, doc.version, &updated_doc)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    "warming instance transitioned to ready (devbox:ready tag seen)"
                );
            }
            Ok(false) => {
                tracing::warn!(
                    doc_id = %doc.id,
                    "version conflict updating warming doc to ready"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    doc_id = %doc.id,
                    "failed to update warming doc to ready"
                );
            }
        }
    }
}

/// Step 5: Reap Warming instances that exceed `ready_timeout` without becoming ready.
///
/// For each Warming doc whose instance is NOT ready and whose `created_at` is
/// older than `config.ready_timeout`, the doc is flipped to `Terminating` FIRST
/// (via a version-guarded write), then `handle_terminating_instances` (step 6)
/// issues the AWS terminate call on the next iteration over the doc list.
///
/// Ordering matters: setting the doc state before the AWS call means that on a
/// version conflict (`Ok(false)`) the AWS call is skipped entirely — the doc
/// remains in its actual state and we retry cleanly next tick. Calling AWS first
/// would terminate an instance whose doc might still read `Warming`, leading to a
/// double-terminate on the next tick and a spurious "failed to terminate" error log.
///
/// The doc is set to `Terminating`, NOT deleted: deleting lets `sync_docs_with_asg`
/// recreate a fresh Warming doc on the next tick (while the box is still shutting
/// down), resetting the timer. Leaving a Terminating doc lets step 6 and the
/// "instance gone from ASG" stale-cleanup path handle final removal.
async fn reap_unready_instances(
    store: &DocumentStore,
    config: &ReconcilerConfig,
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
    info_by_id: &HashMap<&str, &InstanceInfo>,
) {
    let now = Timestamp::now();
    let timeout_secs = config.ready_timeout.as_secs();
    let timeout_nanos = config.ready_timeout.subsec_nanos();
    let timeout_signed = match (i64::try_from(timeout_secs), i32::try_from(timeout_nanos)) {
        (Ok(secs), Ok(nanos)) => SignedDuration::new(secs, nanos),
        _ => {
            tracing::error!("ready_timeout duration overflow; skipping reap");
            return;
        }
    };

    for doc in all_docs {
        if doc.data.state != DevboxState::Warming {
            continue;
        }

        let instance_id = match doc.data.instance_id {
            Some(ref id) => id.as_str(),
            None => continue,
        };

        // Skip instances that have already set the ready tag.
        let is_ready = info_by_id.get(instance_id).is_some_and(|info| info.ready);
        if is_ready {
            continue;
        }

        // Check if the doc's created_at + ready_timeout has elapsed.
        // deadline = created_at + timeout; if deadline < now → timed out.
        let deadline = match doc.data.created_at.checked_add(timeout_signed) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    doc_id = %doc.id,
                    "timestamp overflow computing reap deadline; skipping"
                );
                continue;
            }
        };

        if deadline >= now {
            // Not yet timed out.
            continue;
        }

        // Flip doc to Terminating FIRST. handle_terminating_instances (step 6)
        // issues the AWS terminate call on the same tick. This ordering prevents a
        // double-terminate: if the version-guarded write fails (Ok(false)), we skip
        // the AWS call and retry next tick without having touched AWS.
        let mut updated_doc = doc.data.clone();
        updated_doc.state = DevboxState::Terminating;

        match store
            .compare_and_update(&doc.id, doc.version, &updated_doc)
            .await
        {
            Ok(true) => {
                tracing::warn!(
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    created_at = %doc.data.created_at,
                    "reaping warming instance that exceeded ready_timeout; set to Terminating"
                );
                // handle_terminating_instances (step 6) will issue the AWS call.
            }
            Ok(false) => {
                tracing::warn!(
                    doc_id = %doc.id,
                    "version conflict marking warming doc Terminating for reap; will retry next tick"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    doc_id = %doc.id,
                    "failed to set reaped warming doc to Terminating"
                );
            }
        }
    }
}

/// Step 6: Handle Terminating instances.
///
/// For docs in Terminating state:
/// - If instance_id is Some: terminate in ASG (idempotent — logs warn if already
///   gone), then delete doc regardless of the terminate result.
/// - If instance_id is None: just delete doc.
///
/// The terminate call is treated as best-effort: EC2 returns an error if the
/// instance is already terminated or not found. We log a warning and proceed to
/// delete the doc so it does not linger. The reaper (`reap_unready_instances`)
/// flips docs to Terminating first and then relies on this function to make the
/// AWS call, so terminate errors here are expected in the "already gone" case and
/// must not block doc cleanup.
async fn handle_terminating_instances(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    _asg_name: &str,
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
) {
    for doc in all_docs {
        if doc.data.state != DevboxState::Terminating {
            continue;
        }

        if let Some(ref instance_id) = doc.data.instance_id {
            // Terminate in ASG (don't decrement — let ASG manage replacement).
            // Log a warning on error but always proceed to delete the doc: the
            // instance may already be gone (terminated by the reaper on a prior
            // tick), and leaving the doc would cause repeated spurious errors.
            if let Err(e) = compute.terminate_instance_in_asg(instance_id, false).await {
                tracing::warn!(
                    error = %e,
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    "terminate instance in ASG returned error (may be already gone); proceeding to delete doc"
                );
            }
        }

        // Delete the doc — always, even if the terminate call above failed.
        if let Err(e) = store.delete(&doc.id).await {
            tracing::error!(
                error = %e,
                doc_id = %doc.id,
                "failed to delete terminating doc"
            );
        }
    }
}

/// Step 7: Recompute desired capacity and update if changed.
///
/// The maximum is read from the adopted ASG (Terraform owns `MaxSize`), not from
/// config.
async fn recompute_desired_capacity(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    config: &ReconcilerConfig,
    asg_name: &str,
    asg_desc: &crate::compute::AsgDescription,
) {
    // Re-read docs to get current state after the warming/terminating steps.
    let docs = match store.list_all::<DevboxDoc>().await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to list docs for capacity recompute");
            return;
        }
    };

    let claimed_count = count_by_state(&docs, DevboxState::Claimed);
    let desired = compute_desired_capacity(
        claimed_count,
        config.target_warm_pool_size,
        asg_desc.max_size,
    );

    // Log warning if the unclamped value exceeds the ASG's max.
    let unclamped = claimed_count.saturating_add(config.target_warm_pool_size);
    if unclamped > asg_desc.max_size {
        tracing::warn!(
            unclamped_desired = unclamped,
            max_size = asg_desc.max_size,
            claimed_count = claimed_count,
            "computed desired capacity exceeds ASG max_size"
        );
    }

    if desired != asg_desc.desired_capacity
        && let Err(e) = compute.set_desired_capacity(asg_name, desired).await
    {
        tracing::error!(
            error = %e,
            desired = desired,
            "failed to set desired capacity"
        );
    }
}

/// Step 8: Update scale-in protection.
///
/// - Enable for Claimed instances that are NOT already protected
/// - Disable for non-Claimed instances that ARE protected
async fn update_scale_in_protection(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    asg_name: &str,
    asg_instances: &[crate::compute::AsgInstance],
) {
    // Re-read docs to get the current state
    let docs = match store.list_all::<DevboxDoc>().await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to list docs for scale-in protection");
            return;
        }
    };

    // Build map of instance_id -> doc state
    let doc_states: HashMap<&str, DevboxState> = docs
        .iter()
        .filter_map(|d| d.data.instance_id.as_deref().map(|id| (id, d.data.state)))
        .collect();

    // Build map of instance_id -> currently protected
    let instance_protection: HashMap<&str, bool> = asg_instances
        .iter()
        .map(|inst| (inst.instance_id.as_str(), inst.protected_from_scale_in))
        .collect();

    // Collect Claimed instances that are NOT protected → enable
    let to_protect: Vec<&str> = doc_states
        .iter()
        .filter(|(id, state)| {
            **state == DevboxState::Claimed && instance_protection.get(*id).copied() == Some(false)
        })
        .map(|(id, _)| *id)
        .collect();

    // Collect non-Claimed instances that ARE protected → disable
    let to_unprotect: Vec<&str> = doc_states
        .iter()
        .filter(|(id, state)| {
            **state != DevboxState::Claimed && instance_protection.get(*id).copied() == Some(true)
        })
        .map(|(id, _)| *id)
        .collect();

    if !to_protect.is_empty()
        && let Err(e) = compute
            .set_scale_in_protection(asg_name, &to_protect, true)
            .await
    {
        tracing::error!(
            error = %e,
            count = to_protect.len(),
            "failed to enable scale-in protection"
        );
    }

    if !to_unprotect.is_empty()
        && let Err(e) = compute
            .set_scale_in_protection(asg_name, &to_unprotect, false)
            .await
    {
        tracing::error!(
            error = %e,
            count = to_unprotect.len(),
            "failed to disable scale-in protection"
        );
    }
}

/// Step 9: Apply pending owner tags.
///
/// For Claimed docs with `owner_tag_applied=false`, `instance_id` set, and
/// `owner` set: call `tag_instance` and mark as applied on success.
async fn apply_pending_owner_tags(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
) {
    for doc in all_docs {
        if doc.data.state != DevboxState::Claimed {
            continue;
        }
        if doc.data.owner_tag_applied {
            continue;
        }

        let instance_id = match doc.data.instance_id {
            Some(ref id) => id.as_str(),
            None => continue,
        };

        let owner = match doc.data.owner {
            Some(ref o) => o.as_str(),
            None => continue,
        };

        if let Err(e) = compute
            .tag_instance(instance_id, &[("devbox:owner", owner)])
            .await
        {
            tracing::error!(
                error = %e,
                instance_id = %instance_id,
                doc_id = %doc.id,
                "failed to apply owner tag"
            );
            continue;
        }

        // Update doc with owner_tag_applied = true
        let mut updated_doc = doc.data.clone();
        updated_doc.owner_tag_applied = true;

        match store
            .compare_and_update(&doc.id, doc.version, &updated_doc)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    "applied owner tag"
                );
            }
            Ok(false) => {
                tracing::warn!(
                    doc_id = %doc.id,
                    "version conflict updating owner_tag_applied"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    doc_id = %doc.id,
                    "failed to update owner_tag_applied"
                );
            }
        }
    }
}
