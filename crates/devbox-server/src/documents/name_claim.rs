//! Name-claim document: the database-enforced record of who holds a devbox name.
//!
//! Aurora DSQL runs REPEATABLE READ (snapshot isolation), so a
//! read-then-write uniqueness check inside a transaction is not race-safe: two
//! concurrent writers can both see a name as free and both commit. Instead, a
//! name is held by inserting a claim document whose **id is derived from the
//! name** ([`claim_doc_id`]) in the same transaction as the devbox-doc write —
//! the `documents` table's primary key then rejects the second claimant no
//! matter how the writes interleave. This is the same PK-conflict mechanism as
//! the reconciler leader lock (`reconcile/lock.rs`).
//!
//! The loser of a concurrent race sees either a unique violation (SQLSTATE
//! 23505 — not retryable, propagates to the caller, classify with
//! [`is_unique_violation`](crate::db::pool::is_unique_violation)) or a DSQL
//! same-key write conflict, which
//! [`with_dsql_retry!`](crate::with_dsql_retry) retries — the rerun then hits
//! the committed winner's row as a unique violation.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::StoreTransaction;
use crate::db::document_type::{DocumentType, IndexEntry};

/// Document recording which devbox holds a name. Its id is [`claim_doc_id`] of
/// the name, so the `documents` primary key enforces global name uniqueness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameClaimDoc {
    /// Document id of the devbox holding the name.
    pub devbox_id: String,
}

impl DocumentType for NameClaimDoc {
    const DOC_TYPE: &'static str = "name_claim";

    fn index_entries(&self) -> Vec<IndexEntry> {
        vec![IndexEntry {
            field: "devbox_id",
            value: self.devbox_id.clone(),
        }]
    }
}

/// Document id of the claim on `name`. The `name:` prefix keeps claim ids
/// disjoint from the UUID ids of other documents.
pub fn claim_doc_id(name: &str) -> String {
    format!("name:{name}")
}

/// Reconcile the name-claim document with a devbox name change, inside the
/// caller's open transaction: release `old_name`'s claim and acquire
/// `new_name`'s. Either name may be empty (nothing to release / acquire).
/// Releasing a claim that does not exist is a no-op, so documents created
/// before claims existed rename cleanly.
///
/// An unchanged non-empty name *ensures* the claim exists rather than
/// acquiring it: documents that predate claims carry a name with no claim
/// doc, and this backfills one the next time the doc is written (e.g. a plain
/// claim), closing the window where another box could take the name. A claim
/// already held — by this box, or transitionally by a legacy duplicate — is
/// left untouched.
///
/// # Errors
///
/// Returns an error if a database write fails — in particular a unique
/// violation when another document already holds `new_name`, which rolls the
/// caller's transaction back once dropped.
pub async fn sync_name_claim(
    tx: &mut StoreTransaction<'_>,
    devbox_id: &str,
    old_name: &str,
    new_name: &str,
) -> Result<()> {
    let claim = NameClaimDoc {
        devbox_id: devbox_id.to_string(),
    };
    if old_name == new_name {
        if !new_name.is_empty() {
            tx.insert_with_id_if_absent(&claim_doc_id(new_name), &claim)
                .await?;
        }
        return Ok(());
    }
    if !old_name.is_empty() {
        tx.delete(&claim_doc_id(old_name)).await?;
    }
    if !new_name.is_empty() {
        tx.insert_with_id(&claim_doc_id(new_name), &claim).await?;
    }
    Ok(())
}
