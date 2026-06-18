//! Reconciliation tick logic.
//!
//! Contains the per-tick reconciliation steps implementing the ASG-based
//! pool management flow. Each tick ensures infrastructure (Launch Template,
//! ASG, lifecycle hook), syncs document state with ASG membership, handles
//! lifecycle transitions, and maintains desired capacity.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use jiff::Timestamp;

use crate::compute::{AsgConfig, Compute, LifecycleHookConfig};
use crate::db::DocumentStore;
use crate::documents::devbox::DevboxDoc;
use devbox_common::{DevboxState, SubnetId};

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
/// This implements the full ASG-based pool management flow:
/// 1. Ensure Launch Template
/// 2. Compute initial desired capacity from DB state
/// 3. Ensure ASG exists
/// 4. Ensure lifecycle hook
/// 5. Describe ASG to get current instances
/// 6. Sync DevboxDoc records with ASG membership
/// 7. Handle Warming instances (complete lifecycle for InService ones)
/// 8. Handle Terminating instances (terminate + delete doc)
/// 9. Recompute desired capacity and update if changed
/// 10. Update scale-in protection
/// 11. Apply pending owner tags
///
/// # Errors
///
/// Returns an error if critical infrastructure steps (1, 3, 4, 5) fail.
/// Non-critical steps (7, 8, 10, 11) log errors and continue.
pub(super) async fn reconciliation_tick(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    config: &ReconcilerConfig,
) -> Result<()> {
    // Step 1: Ensure Launch Template (abort on failure)
    let lt = compute
        .ensure_launch_template(&config.to_launch_template_config())
        .await?;

    // Step 2: Compute initial desired capacity from DB state
    let all_docs = store.list_all::<DevboxDoc>().await?;
    let claimed_count = count_by_state(&all_docs, DevboxState::Claimed);
    let initial_desired = compute_desired_capacity(
        claimed_count,
        config.target_warm_pool_size,
        config.max_pool_size,
    );

    // Step 3: Ensure ASG exists (abort on failure)
    let asg_name = compute
        .ensure_asg(&AsgConfig {
            name: config.asg_name(),
            launch_template_id: lt.id,
            launch_template_version: lt.version,
            subnet_ids: config.subnet_ids.iter().map(|s| s.0.clone()).collect(),
            min_size: 0,
            max_size: config.max_pool_size,
            desired_capacity: initial_desired,
            health_check_grace_period: 300,
            propagate_tags_at_launch: true,
            pool_id: config.pool_id.clone(),
            managed_by: config.server_id.clone(),
        })
        .await?;

    // Step 4: Ensure lifecycle hook (abort on failure)
    compute
        .ensure_lifecycle_hook(&LifecycleHookConfig {
            asg_name: asg_name.clone(),
            hook_name: config.lifecycle_hook_name(),
            heartbeat_timeout_secs: config.lifecycle_hook_timeout_secs(),
        })
        .await?;

    // Step 5: Describe ASG to get current instances (abort on failure)
    let asg_desc = compute.describe_asg(&asg_name).await?;

    // Step 6: Sync DevboxDoc records with ASG membership
    sync_docs_with_asg(store, config, &asg_desc.instances).await;

    // Re-read docs after sync to get fresh state
    let all_docs = store.list_all::<DevboxDoc>().await?;

    // Step 7: Handle Warming instances
    handle_warming_instances(store, compute, config, &asg_name, &all_docs, &asg_desc.instances)
        .await;

    // Step 8: Handle Terminating instances
    handle_terminating_instances(store, compute, &asg_name, &all_docs).await;

    // Step 9: Recompute desired capacity and update if changed
    recompute_desired_capacity(store, compute, config, &asg_name, &asg_desc).await;

    // Step 10: Update scale-in protection
    update_scale_in_protection(store, compute, &asg_name, &asg_desc.instances).await;

    // Step 11: Apply pending owner tags
    apply_pending_owner_tags(store, compute, &all_docs).await;

    Ok(())
}

/// Step 6: Sync DevboxDoc records with ASG membership.
///
/// - Delete docs whose instance_id is NOT in ASG instance set (stale cleanup)
/// - Create new Warming docs for instances in "Pending:Wait" with no doc
/// - Create new Ready docs for instances in "InService" with no doc
async fn sync_docs_with_asg(
    store: &DocumentStore,
    config: &ReconcilerConfig,
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

    // Get the first subnet from config for new docs
    let subnet_id = config
        .subnet_ids
        .first()
        .cloned()
        .unwrap_or_else(|| SubnetId("unknown".to_string()));

    // Create docs for ASG instances that have no doc
    for inst in asg_instances {
        if doc_instance_ids.contains(&inst.instance_id) {
            continue;
        }

        let state = if inst.lifecycle_state == "Pending:Wait" {
            DevboxState::Warming
        } else if inst.lifecycle_state == "InService" {
            DevboxState::Ready
        } else {
            // Skip instances in other states (Terminating, etc.)
            continue;
        };

        let new_doc = DevboxDoc {
            instance_id: Some(inst.instance_id.clone()),
            state,
            instance_type: config.instance_type.clone(),
            ami_id: config.ami_id.clone(),
            subnet_id: subnet_id.clone(),
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
/// For Warming docs whose ASG instance is now InService:
/// call `complete_lifecycle_action` with "CONTINUE", then update state to Ready.
async fn handle_warming_instances(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    config: &ReconcilerConfig,
    asg_name: &str,
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
    asg_instances: &[crate::compute::AsgInstance],
) {
    // Build map of instance_id -> lifecycle_state
    let instance_states: HashMap<&str, &str> = asg_instances
        .iter()
        .map(|inst| (inst.instance_id.as_str(), inst.lifecycle_state.as_str()))
        .collect();

    let hook_name = config.lifecycle_hook_name();

    for doc in all_docs {
        if doc.data.state != DevboxState::Warming {
            continue;
        }

        let instance_id = match doc.data.instance_id {
            Some(ref id) => id.as_str(),
            None => continue,
        };

        // Check if ASG instance is now InService
        let is_in_service = instance_states
            .get(instance_id)
            .is_some_and(|s| *s == "InService");

        if !is_in_service {
            continue;
        }

        // Complete lifecycle action
        if let Err(e) = compute
            .complete_lifecycle_action(asg_name, &hook_name, instance_id, "CONTINUE")
            .await
        {
            tracing::error!(
                error = %e,
                instance_id = %instance_id,
                "failed to complete lifecycle action"
            );
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
            if let Err(e) = compute
                .terminate_instance_in_asg(instance_id, false)
                .await
            {
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

/// Step 9: Recompute desired capacity and update if changed.
async fn recompute_desired_capacity(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    config: &ReconcilerConfig,
    asg_name: &str,
    asg_desc: &crate::compute::AsgDescription,
) {
    // Re-read docs to get current state after steps 7 and 8
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
        config.max_pool_size,
    );

    // Log warning if unclamped value exceeds max
    let unclamped = claimed_count.saturating_add(config.target_warm_pool_size);
    if unclamped > config.max_pool_size {
        tracing::warn!(
            unclamped_desired = unclamped,
            max_pool_size = config.max_pool_size,
            claimed_count = claimed_count,
            "computed desired capacity exceeds max_pool_size"
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
        .filter_map(|d| {
            d.data
                .instance_id
                .as_deref()
                .map(|id| (id, d.data.state))
        })
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
            **state == DevboxState::Claimed
                && instance_protection.get(*id).copied() == Some(false)
        })
        .map(|(id, _)| *id)
        .collect();

    // Collect non-Claimed instances that ARE protected → disable
    let to_unprotect: Vec<&str> = doc_states
        .iter()
        .filter(|(id, state)| {
            **state != DevboxState::Claimed
                && instance_protection.get(*id).copied() == Some(true)
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
