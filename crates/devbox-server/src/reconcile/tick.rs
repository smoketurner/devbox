//! Reconciliation tick logic.
//!
//! The reconciler is adopt-only: Terraform provisions the Launch Template, ASG,
//! and lifecycle hook (see CLAUDE.md). Each tick looks the
//! ASG up by name, syncs `DevboxDoc` records with its membership, observes
//! host-driven lifecycle transitions, and writes only runtime state — desired
//! capacity (clamped to the ASG's max), scale-in protection, owner tags, and
//! terminations.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use jiff::Timestamp;

use crate::compute::Compute;
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
/// 2. Sync DevboxDoc records with ASG membership.
/// 3. Mark Warming instances Ready once the host drives them InService.
/// 4. Terminate Terminating instances and delete their docs.
/// 5. Recompute desired capacity (clamped to the ASG's max) and update.
/// 6. Update scale-in protection for Claimed instances.
/// 7. Apply pending owner tags.
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

    // Step 2: Sync DevboxDoc records with ASG membership.
    sync_docs_with_asg(store, compute, &asg_desc.instances).await;

    // Re-read docs after sync to get fresh state.
    let all_docs = store.list_all::<DevboxDoc>().await?;

    // Step 3: Mark Warming instances Ready once the host drives them InService.
    handle_warming_instances(store, &all_docs, &asg_desc.instances).await;

    // Step 4: Handle Terminating instances.
    handle_terminating_instances(store, compute, &asg_name, &all_docs).await;

    // Step 5: Recompute desired capacity and update if changed.
    recompute_desired_capacity(store, compute, config, &asg_name, &asg_desc).await;

    // Step 6: Update scale-in protection.
    update_scale_in_protection(store, compute, &asg_name, &asg_desc.instances).await;

    // Step 7: Apply pending owner tags.
    apply_pending_owner_tags(store, compute, &all_docs).await;

    Ok(())
}

/// Step 2: Sync DevboxDoc records with ASG membership.
///
/// - Delete docs whose instance_id is NOT in ASG instance set (stale cleanup)
/// - Create new Warming docs for instances in "Pending:Wait" with no doc
/// - Create new Ready docs for instances in "InService" with no doc
///
/// Instance metadata (type, AMI, subnet) is read from `DescribeInstances` rather
/// than carried in config — the running instance is the source of truth.
async fn sync_docs_with_asg(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    asg_instances: &[crate::compute::AsgInstance],
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
    let new_instances: Vec<&crate::compute::AsgInstance> = asg_instances
        .iter()
        .filter(|inst| !doc_instance_ids.contains(&inst.instance_id))
        .filter(|inst| {
            inst.lifecycle_state == "Pending:Wait" || inst.lifecycle_state == "InService"
        })
        .collect();

    if new_instances.is_empty() {
        return;
    }

    // Enrich with EC2 metadata; without it we cannot populate the doc, so retry
    // on the next tick rather than inventing values.
    let ids: Vec<&str> = new_instances
        .iter()
        .map(|inst| inst.instance_id.as_str())
        .collect();
    let infos = match compute.describe_instances(&ids).await {
        Ok(infos) => infos,
        Err(e) => {
            tracing::error!(error = %e, "failed to describe new instances; will retry");
            return;
        }
    };
    let info_by_id: HashMap<&str, &crate::compute::InstanceInfo> = infos
        .iter()
        .map(|info| (info.instance_id.as_str(), info))
        .collect();

    for inst in new_instances {
        let state = if inst.lifecycle_state == "Pending:Wait" {
            DevboxState::Warming
        } else {
            DevboxState::Ready
        };

        let Some(info) = info_by_id.get(inst.instance_id.as_str()) else {
            tracing::warn!(
                instance_id = %inst.instance_id,
                "no DescribeInstances result; will retry next tick"
            );
            continue;
        };

        let new_doc = DevboxDoc {
            instance_id: Some(inst.instance_id.clone()),
            state,
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

/// Step 7: Handle Warming instances.
///
/// The host's `devbox-agent warmup` completes the ASG launch lifecycle hook once
/// the box is ready, moving it from `Pending:Wait` to `InService`. The reconciler
/// observes that transition and flips the corresponding `DevboxDoc` to Ready. It
/// does not complete the hook itself — only the host knows when warm-up is done.
async fn handle_warming_instances(
    store: &DocumentStore,
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
    asg_instances: &[crate::compute::AsgInstance],
) {
    // Build map of instance_id -> lifecycle_state
    let instance_states: HashMap<&str, &str> = asg_instances
        .iter()
        .map(|inst| (inst.instance_id.as_str(), inst.lifecycle_state.as_str()))
        .collect();

    for doc in all_docs {
        if doc.data.state != DevboxState::Warming {
            continue;
        }

        let instance_id = match doc.data.instance_id {
            Some(ref id) => id.as_str(),
            None => continue,
        };

        // Wait for the host to complete the hook (Pending:Wait -> InService).
        let is_in_service = instance_states
            .get(instance_id)
            .is_some_and(|s| *s == "InService");

        if !is_in_service {
            continue;
        }

        // Update doc state to Ready
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
                    "warming instance transitioned to ready"
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

/// Step 8: Handle Terminating instances.
///
/// For docs in Terminating state:
/// - If instance_id is Some: terminate in ASG, then delete doc
/// - If instance_id is None: just delete doc
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
            // Terminate instance in ASG (don't decrement — let ASG manage replacement)
            if let Err(e) = compute.terminate_instance_in_asg(instance_id, false).await {
                tracing::error!(
                    error = %e,
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    "failed to terminate instance in ASG"
                );
                continue;
            }
        }

        // Delete the doc
        if let Err(e) = store.delete(&doc.id).await {
            tracing::error!(
                error = %e,
                doc_id = %doc.id,
                "failed to delete terminating doc"
            );
        }
    }
}

/// Step 5: Recompute desired capacity and update if changed.
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

/// Step 10: Update scale-in protection.
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

/// Step 11: Apply pending owner tags.
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
