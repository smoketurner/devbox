//! Reconciliation tick logic.
//!
//! Contains the per-tick reconciliation steps: stuck recovery,
//! termination, pool size maintenance, and lifecycle advancement.

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use devbox_common::DevboxState;

use crate::db::DocumentStore;
use crate::db::document_type::Document;
use crate::documents::devbox::DevboxDoc;
use crate::ec2::{Ec2Client, InstanceStatus};

use super::config::ReconcilerConfig;

/// Execute a single reconciliation tick.
///
/// Steps (in order):
/// 1. Query all DevboxDocs
/// 2. Stuck recovery: transition stuck Launching/Warming → Terminating
/// 3. Terminate: call terminate_instance + delete doc for Terminating instances
/// 4. Launch: create new instances if below target
/// 5. Advance: transition Launching → Warming → Ready based on EC2 status
pub(super) async fn reconciliation_tick(
    store: &DocumentStore,
    ec2: &(impl Ec2Client + ?Sized),
    config: &ReconcilerConfig,
) -> Result<()> {
    // Step 1: Query all documents
    let all_docs = store.list_all::<DevboxDoc>().await?;

    // Step 2: Stuck recovery
    recover_stuck_instances(store, &all_docs, config).await;

    // Step 3: Terminate
    terminate_instances(store, ec2, &all_docs).await;

    // Step 4: Launch (re-query to get updated counts after termination)
    let current_docs = store.list_all::<DevboxDoc>().await?;
    launch_instances(store, ec2, &current_docs, config).await;

    // Step 5: Advance lifecycle (re-query to get current state)
    let current_docs = store.list_all::<DevboxDoc>().await?;
    advance_lifecycle(store, ec2, &current_docs).await;

    Ok(())
}

/// Step 2: Transition instances stuck in Launching/Warming to Terminating.
async fn recover_stuck_instances(
    store: &DocumentStore,
    docs: &[Document<DevboxDoc>],
    config: &ReconcilerConfig,
) {
    let now = Timestamp::now();
    let threshold_secs = config.stuck_threshold.as_secs();
    let threshold_nanos = config.stuck_threshold.subsec_nanos();

    // Convert std::time::Duration to jiff::SignedDuration for comparison
    let threshold = match i64::try_from(threshold_secs) {
        Ok(secs) => match i32::try_from(threshold_nanos) {
            Ok(nanos) => SignedDuration::new(secs, nanos),
            Err(_) => {
                tracing::error!("stuck_threshold nanos overflow, skipping stuck recovery");
                return;
            }
        },
        Err(_) => {
            tracing::error!("stuck_threshold secs overflow, skipping stuck recovery");
            return;
        }
    };

    for doc in docs {
        let is_stuck_candidate = matches!(
            doc.data.state,
            DevboxState::Launching | DevboxState::Warming
        );
        if !is_stuck_candidate {
            continue;
        }

        // Check if stuck: now - updated_at > stuck_threshold
        let elapsed = now.duration_since(doc.updated_at);
        if elapsed <= threshold {
            continue;
        }

        // Transition to Terminating
        let mut updated = doc.data.clone();
        updated.state = DevboxState::Terminating;
        match store
            .compare_and_update(&doc.id, doc.version, &updated)
            .await
        {
            Ok(true) => {
                tracing::warn!(
                    id = %doc.id,
                    state = %doc.data.state,
                    "stuck instance transitioned to terminating"
                );
            }
            Ok(false) => {
                tracing::warn!(
                    id = %doc.id,
                    "version conflict during stuck recovery, skipping"
                );
            }
            Err(e) => {
                tracing::error!(
                    id = %doc.id,
                    error = %e,
                    "failed to transition stuck instance"
                );
            }
        }
    }
}

/// Step 3: Terminate instances in Terminating state.
async fn terminate_instances(
    store: &DocumentStore,
    ec2: &(impl Ec2Client + ?Sized),
    docs: &[Document<DevboxDoc>],
) {
    for doc in docs {
        if doc.data.state != DevboxState::Terminating {
            continue;
        }

        // If has instance_id, terminate the EC2 instance first
        if let Some(ref instance_id) = doc.data.instance_id
            && let Err(e) = ec2.terminate_instance(instance_id).await
        {
            tracing::error!(
                id = %doc.id,
                instance_id = %instance_id,
                error = %e,
                "failed to terminate EC2 instance"
            );
            continue; // retry next tick
        }

        // Delete the document
        if let Err(e) = store.delete(&doc.id).await {
            tracing::error!(
                id = %doc.id,
                error = %e,
                "failed to delete terminating document"
            );
        }
    }
}

/// Step 4: Launch new instances if pool is below target.
async fn launch_instances(
    store: &DocumentStore,
    ec2: &(impl Ec2Client + ?Sized),
    docs: &[Document<DevboxDoc>],
    config: &ReconcilerConfig,
) {
    let active_count = docs
        .iter()
        .filter(|d| {
            matches!(
                d.data.state,
                DevboxState::Launching | DevboxState::Warming | DevboxState::Ready
            )
        })
        .count();

    let target = config.target_pool_size as usize;
    if active_count >= target {
        return;
    }

    let deficit = target.saturating_sub(active_count);
    for _ in 0..deficit {
        // Create DevboxDoc in Launching state (no instance_id yet)
        let new_doc = DevboxDoc {
            instance_id: None,
            state: DevboxState::Launching,
            instance_type: config.instance_type.clone(),
            ami_id: config.ami_id.clone(),
            subnet_id: config.subnet_id.clone(),
            ebs_volume_id: None,
            owner: None,
            claimed_at: None,
            created_at: Timestamp::now(),
        };

        let inserted = match store.insert(&new_doc).await {
            Ok(doc) => doc,
            Err(e) => {
                tracing::error!(error = %e, "failed to create DevboxDoc for launch");
                continue;
            }
        };

        let doc_id = inserted.id;

        // Call EC2 to launch
        match ec2
            .launch_instance(
                config.instance_type.as_ref(),
                config.ami_id.as_ref(),
                config.subnet_id.as_ref(),
            )
            .await
        {
            Ok(instance_id) => {
                // Update doc with the instance_id
                let mut updated = new_doc.clone();
                updated.instance_id = Some(instance_id.clone());
                if let Err(e) = store.update(&doc_id, &updated).await {
                    tracing::error!(
                        id = %doc_id,
                        error = %e,
                        "failed to store instance_id after launch"
                    );
                } else {
                    tracing::info!(
                        id = %doc_id,
                        instance_id = %instance_id,
                        "launched new EC2 instance"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    id = %doc_id,
                    error = %e,
                    "EC2 launch_instance failed"
                );
                // Leave doc in Launching state — stuck recovery will clean up
            }
        }
    }
}

/// Step 5: Advance Launching → Warming and Warming → Ready.
async fn advance_lifecycle(
    store: &DocumentStore,
    ec2: &(impl Ec2Client + ?Sized),
    docs: &[Document<DevboxDoc>],
) {
    for doc in docs {
        let next_state = match doc.data.state {
            DevboxState::Launching => DevboxState::Warming,
            DevboxState::Warming => DevboxState::Ready,
            _ => continue,
        };

        // Need instance_id to check EC2 status
        let instance_id = match &doc.data.instance_id {
            Some(id) => id,
            None => continue, // Can't check status without instance_id
        };

        // Describe the instance
        let status = match ec2.describe_instance(instance_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    id = %doc.id,
                    instance_id = %instance_id,
                    error = %e,
                    "describe_instance failed, skipping"
                );
                continue;
            }
        };

        if status != InstanceStatus::Running {
            continue; // Not ready to advance
        }

        // Transition to next state
        let mut updated = doc.data.clone();
        updated.state = next_state;
        match store
            .compare_and_update(&doc.id, doc.version, &updated)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    id = %doc.id,
                    from = %doc.data.state,
                    to = %updated.state,
                    "advanced lifecycle state"
                );
            }
            Ok(false) => {
                tracing::warn!(
                    id = %doc.id,
                    "version conflict during lifecycle advancement, skipping"
                );
            }
            Err(e) => {
                tracing::error!(
                    id = %doc.id,
                    error = %e,
                    "failed to advance lifecycle state"
                );
            }
        }
    }
}
