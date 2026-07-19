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
use crate::documents::name_claim::sync_name_claim;
use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};

use super::config::ReconcilerConfig;

/// Compute the desired ASG capacity.
///
/// Formula: `min(claimed_count + warm_pool_size, max_pool_size)`
///
/// Both `warm_pool_size` and `max_pool_size` come from the adopted ASG
/// (`min_size`/`max_size`); Terraform is the single source of truth. AWS
/// guarantees `min_size <= max_size`, so at zero claims the result equals
/// `warm_pool_size` and the clamp only engages as claims saturate the pool.
///
/// Uses `saturating_add` to avoid arithmetic overflow and `.min()` to clamp
/// the result to the ASG's maximum.
///
/// # Returns
///
/// The desired capacity value, always in the range `[0, max_pool_size]`.
pub(crate) fn compute_desired_capacity(
    claimed_count: u32,
    warm_pool_size: u32,
    max_pool_size: u32,
) -> u32 {
    claimed_count
        .saturating_add(warm_pool_size)
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

    // Step 2: Describe all instances in the ASG upfront to read the `devbox:ready`
    // tag for the warming→ready transition and the reaper.
    //
    // A describe failure must NOT abort the whole tick: owner-tagging, capacity,
    // scale-in protection, and Terminating cleanup do not depend on the tag and
    // still need to run — a just-claimed box must get its `devbox:owner` tag so the
    // claimant can SSH in, even during a transient EC2 describe brownout. On failure
    // we proceed with empty tag data and skip only the tag-dependent steps, crucially
    // the reaper: running it with no tag data would treat every box as unready and
    // reap the entire warm pool.
    let all_ids: Vec<&str> = asg_desc
        .instances
        .iter()
        .map(|inst| inst.instance_id.as_str())
        .collect();
    let (instance_infos, describe_ok) = match compute.describe_instances(&all_ids).await {
        Ok(infos) => (infos, true),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to describe ASG instances; skipping tag-dependent steps this tick"
            );
            (Vec::new(), false)
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

    // Steps 4 & 5 depend on fresh `devbox:ready` tag data; skip them when the
    // describe failed (empty tag data would falsely reap the whole warm pool).
    if describe_ok {
        // Step 4: Mark Warming instances Ready when the `devbox:ready` tag is present.
        handle_warming_instances(store, &all_docs, &info_by_id).await;

        // Step 5: Reap Warming instances that never became ready within ready_timeout.
        reap_unready_instances(store, compute, config, &all_docs, &info_by_id).await;
    }

    // Step 6: Handle Terminating instances.
    handle_terminating_instances(store, compute, &asg_name, &all_docs).await;

    // Step 7: Recompute desired capacity and update if changed.
    recompute_desired_capacity(store, compute, &asg_name, &asg_desc).await;

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

    // Delete docs whose instance_id is not in ASG (stale cleanup), releasing
    // any name claim in the same transaction.
    for doc in &docs {
        if !asg_instance_ids.contains(doc.data.instance_id.as_str()) {
            let deleted = crate::with_dsql_retry!(async {
                let mut tx = store.begin().await?;
                tx.delete(&doc.id).await?;
                sync_name_claim(&mut tx, &doc.id, &doc.data.name, "").await?;
                tx.commit().await?;
                Ok(())
            });
            if let Err(e) = deleted {
                tracing::error!(
                    error = %e,
                    doc_id = %doc.id,
                    instance_id = %doc.data.instance_id,
                    "failed to delete stale doc"
                );
            }
        }
    }

    // Names already in use, so generation never collides within this tick.
    let mut used_names: HashSet<String> = docs
        .iter()
        .map(|d| d.data.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    // Build set of instance_ids that already have docs
    let doc_instance_ids: HashSet<String> =
        docs.iter().map(|d| d.data.instance_id.clone()).collect();

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

        // Give the box a unique friendly name up front. Generation effectively
        // never fails; if it somehow does, fall back to the instance id (itself
        // unique and a valid name) so the box is still created and named.
        let name = crate::naming::generate_unique_name(store, &used_names)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    error = %e,
                    instance_id = %inst.instance_id,
                    "name generation failed; falling back to instance id"
                );
                inst.instance_id.clone()
            });

        // Always create Warming: readiness is gated on devbox:ready tag, never
        // on lifecycle state alone. handle_warming_instances will flip to Ready
        // on the next tick once the tag is seen.
        let new_doc = DevboxDoc {
            instance_id: inst.instance_id.clone(),
            name: name.clone(),
            state: DevboxState::Warming,
            instance_type: InstanceType(info.instance_type.clone()),
            ami_id: AmiId(info.ami_id.clone()),
            subnet_id: SubnetId(info.subnet_id.clone()),
            region: info.region.clone(),
            ebs_volume_id: None,
            owner: None,
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };

        // Insert the doc and acquire its name claim in one transaction; a
        // lost name race fails the insert and retries next tick.
        let doc_id = uuid::Uuid::now_v7().to_string();
        let inserted = crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            tx.insert_with_id(&doc_id, &new_doc).await?;
            sync_name_claim(&mut tx, &doc_id, "", &new_doc.name).await?;
            tx.commit().await?;
            Ok(())
        });
        match inserted {
            Ok(()) => {
                used_names.insert(name);
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    instance_id = %inst.instance_id,
                    "failed to create doc for ASG instance"
                );
            }
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

        let instance_id = doc.data.instance_id.as_str();

        let is_ready = info_by_id.get(instance_id).is_some_and(|info| info.ready);

        if !is_ready {
            continue;
        }

        let mut updated_doc = doc.data.clone();
        updated_doc.state = DevboxState::Ready;
        // The claim-to-ready dwell (and the deferred claim-to-first-build
        // metric) needs the moment the box became claimable; state alone
        // doesn't record it.
        updated_doc.ready_at = Some(Timestamp::now());

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
///
/// Before terminating a timed-out box, the reaper re-describes that single
/// instance: `info_by_id` is a tick-start snapshot, and `warmup` may have set
/// `devbox:ready=true` in the window since. If the fresh read reports ready (or
/// itself fails), the reap is skipped — never terminate a box that just reported
/// ready, and never reap on a transient describe failure.
pub(super) async fn reap_unready_instances(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
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

        let instance_id = doc.data.instance_id.as_str();

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

        // Stale-snapshot guard: `info_by_id` was captured at tick start. `warmup`
        // may have set `devbox:ready=true` in the window since, so re-describe this
        // one instance before terminating it and skip the reap if it now reports
        // ready. Fail-safe: skip on a re-describe error too — never terminate a
        // possibly-healthy box on a transient failure.
        match compute.describe_instances(&[instance_id]).await {
            Ok(infos) => {
                if infos
                    .iter()
                    .any(|i| i.instance_id.as_str() == instance_id && i.ready)
                {
                    tracing::info!(
                        instance_id = %instance_id,
                        doc_id = %doc.id,
                        "reap candidate reported ready since tick start; skipping reap"
                    );
                    continue;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    instance_id = %instance_id,
                    doc_id = %doc.id,
                    "failed to re-describe reap candidate; skipping reap this tick"
                );
                continue;
            }
        }

        // Flip doc to Terminating FIRST. handle_terminating_instances (step 6)
        // issues the AWS terminate call on the NEXT tick: it iterates the
        // `all_docs` snapshot that was read before this reap ran, so it still sees
        // the doc as Warming this tick and skips it. This ordering prevents a
        // double-terminate: if the version-guarded write fails (Ok(false)), we skip
        // the AWS call and retry next tick without having touched AWS.
        let mut updated_doc = doc.data.clone();
        updated_doc.state = DevboxState::Terminating;
        // Free the name immediately so it can be reused on a fresh claim.
        updated_doc.name = String::new();

        let reaped = crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            if !tx
                .compare_and_update(&doc.id, doc.version, &updated_doc)
                .await?
            {
                return Ok(false);
            }
            sync_name_claim(&mut tx, &doc.id, &doc.data.name, "").await?;
            tx.commit().await?;
            Ok(true)
        });

        match reaped {
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
/// - If instance_id is Some: attempt to terminate in ASG. On a transient error
///   (throttle / 5xx), log a warning and leave the Terminating doc in place so
///   the reap retries on the next tick. The stale-cleanup path in
///   `sync_docs_with_asg` deletes the doc once the instance leaves the ASG.
/// - On a successful terminate (or no instance_id): delete the doc.
///
/// Deleting the doc on a terminate error would let `sync_docs_with_asg` recreate
/// a fresh Warming doc on the next tick (while the instance is still InService),
/// resetting the reap timer — the exact failure mode the reaper's doc comment warns
/// against.
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

        if let Err(e) = compute
            .terminate_instance_in_asg(&doc.data.instance_id, false)
            .await
        {
            tracing::warn!(
                error = %e,
                instance_id = %doc.data.instance_id,
                doc_id = %doc.id,
                "failed to terminate instance in ASG; will retry next tick"
            );
            continue; // leave Terminating doc; stale-cleanup deletes once instance leaves ASG
        }

        // Terminate succeeded — safe to delete now, releasing any name claim
        // in the same transaction (Terminating docs normally have their name
        // already cleared; this covers docs that reached Terminating without
        // the clear).
        let deleted = crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            tx.delete(&doc.id).await?;
            sync_name_claim(&mut tx, &doc.id, &doc.data.name, "").await?;
            tx.commit().await?;
            Ok(())
        });
        if let Err(e) = deleted {
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
/// Both the warm-pool target (`min_size`) and the ceiling (`max_size`) are read
/// from the adopted ASG; Terraform is the single source of truth for pool sizing.
async fn recompute_desired_capacity(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
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
    let desired = compute_desired_capacity(claimed_count, asg_desc.min_size, asg_desc.max_size);

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
        .map(|d| (d.data.instance_id.as_str(), d.data.state))
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

/// Step 9: Re-assert owner tags every tick and apply them for the first time.
///
/// For each Claimed doc with an `owner` set, calls `tag_instance` with the
/// doc-dictated owner (and optional owner-email) on **every** tick.
/// `ec2:CreateTags` is idempotent — re-applying an unchanged value is a no-op,
/// and re-applying a tampered value overwrites it within one tick.
/// Keys present on the instance but absent from the doc (e.g. a stale
/// `devbox:owner-email` when `owner_email` is later cleared) are not deleted.
///
/// `owner_tag_applied` gates first-application bookkeeping only:
/// - When `false`: on a successful `tag_instance`, flip it via
///   `compare_and_update` and emit an info log.
/// - When `true`: the idempotent re-write still runs, but no DB update and no
///   info log, to avoid per-tick churn and log spam.
///
/// Step 9 runs regardless of `describe_ok` (owner-tagging does not depend on
/// the tick-start `DescribeInstances`).
async fn apply_pending_owner_tags(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    all_docs: &[crate::db::document_type::Document<DevboxDoc>],
) {
    for doc in all_docs {
        if doc.data.state != DevboxState::Claimed {
            continue;
        }
        apply_owner_tag(store, compute, doc).await;
    }
}

/// Apply a single Claimed box's owner tags to its instance and record it.
///
/// Shared by the claim handler (which calls it inline so the box is loginable
/// without waiting for a reconciler tick) and [`apply_pending_owner_tags`] (which
/// re-asserts every tick as the idempotent fallback). Both the host's `owner-sync`
/// and `principals` resolver read these tags from IMDS, so a box can only be
/// logged into once they are present.
///
/// Re-applies the tag on every call — `ec2:CreateTags` is idempotent on an
/// unchanged value and overwrites a tampered one. `owner_tag_applied` gates only
/// the bookkeeping write: it is flipped (with an info log) on first success and
/// left alone thereafter, so steady-state re-assertion causes no DB churn. A tag
/// failure leaves it `false` so the next tick retries; the box has no owner (empty
/// tag set) is a no-op.
pub(crate) async fn apply_owner_tag(
    store: &DocumentStore,
    compute: &(impl Compute + ?Sized),
    doc: &crate::db::document_type::Document<DevboxDoc>,
) {
    let instance_id = doc.data.instance_id.as_str();

    // The doc-dictated tag set (devbox:owner, plus devbox:owner-email when
    // present). Empty when the box has no owner — nothing to apply yet.
    let tags = doc.data.owner_tags();
    if tags.is_empty() {
        return;
    }

    // Re-assert unconditionally — idempotent on match, self-heals on divergence.
    if let Err(e) = compute.tag_instance(instance_id, &tags).await {
        tracing::warn!(
            error = %e,
            instance_id = %instance_id,
            doc_id = %doc.id,
            "failed to apply owner tag; will retry on the next reconciler tick"
        );
        return;
    }

    // Only flip owner_tag_applied and emit the info log on first application.
    // For subsequent calls (already true), the idempotent re-write above is
    // sufficient — no DB update and no info log to avoid per-tick churn.
    if doc.data.owner_tag_applied {
        return;
    }

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
