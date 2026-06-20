//! Leader lock for distributed reconciler coordination.
//!
//! Uses a document in the DocumentStore as an advisory lock to ensure
//! only one reconciler instance performs actions at a time.

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use crate::db::DocumentStore;
use crate::documents::leader_lock::LeaderLockDoc;

use super::config::ReconcilerConfig;

/// Well-known document ID for the leader lock singleton.
const LOCK_ID: &str = "reconciler-leader-lock";

/// Build a new lock document with the current server's identity.
fn build_lock_doc(config: &ReconcilerConfig, expires_at: Timestamp) -> LeaderLockDoc {
    LeaderLockDoc {
        holder_id: config.server_id.clone(),
        expires_at,
    }
}

/// Attempt to acquire or renew the leader lock.
///
/// Returns `true` if this server holds the lock after this call.
/// Returns `false` if another server holds a valid (non-expired) lock.
///
/// # Algorithm
///
/// 1. Get the lock document by its well-known ID.
/// 2. If absent → insert a new lock with our server_id and TTL expiry.
/// 3. If present and expired → claim it via compare_and_update.
/// 4. If present and we hold it → renew by extending expires_at.
/// 5. If present and held by another → return false.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub(super) async fn try_acquire_lock(
    store: &DocumentStore,
    config: &ReconcilerConfig,
) -> Result<bool> {
    let now = Timestamp::now();
    let ttl_secs = config.lock_ttl.as_secs();
    let ttl_nanos = config.lock_ttl.subsec_nanos();
    let signed_duration = SignedDuration::new(i64::try_from(ttl_secs)?, i32::try_from(ttl_nanos)?);
    let new_expiry = now.checked_add(signed_duration)?;

    match store.get::<LeaderLockDoc>(LOCK_ID).await? {
        None => {
            // No lock exists — create it with our server_id.
            let lock_doc = build_lock_doc(config, new_expiry);
            match store.insert_with_id(LOCK_ID, &lock_doc).await {
                Ok(_) => Ok(true),
                Err(e) if crate::db::pool::is_unique_violation(&e) => Ok(false),
                Err(e) => Err(e),
            }
        }
        Some(doc) => {
            if doc.data.expires_at < now {
                // Lock expired — claim it via compare_and_update.
                let updated = build_lock_doc(config, new_expiry);
                let success = store
                    .compare_and_update(LOCK_ID, doc.version, &updated)
                    .await?;
                Ok(success)
            } else if doc.data.holder_id == config.server_id {
                // We hold the lock — renew it by extending expires_at.
                let updated = build_lock_doc(config, new_expiry);
                let success = store
                    .compare_and_update(LOCK_ID, doc.version, &updated)
                    .await?;
                Ok(success)
            } else {
                // Another server holds a valid (non-expired) lock.
                Ok(false)
            }
        }
    }
}
