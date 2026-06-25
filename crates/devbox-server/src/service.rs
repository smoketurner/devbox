//! Domain service layer.
//!
//! This module owns the business logic for all devbox operations. Each function
//! here validates inputs, enforces authorization rules, drives the document
//! store, and returns a typed result. The HTTP layer ([`crate::routes`]) and the
//! dashboard ([`crate::ui`]) are thin adapters that extract request data, call
//! into this module, and map the outcome to a response.
//!
//! State access is via [`AppState`] from [`crate::routes`]; no Axum extractors,
//! status codes, or JSON types cross this boundary. Error cases are expressed via
//! [`AppError`] variants so the callers decide how to render them.

use devbox_common::{DEVBOX_NAME_MAX_LEN, DevboxState, PoolMetricsResponse, is_valid_devbox_name};

use crate::auth::Principal;
use crate::db::UpdateOutcome;
use crate::db::document_type::Document;
use crate::documents::devbox::DevboxDoc;
use crate::error::AppError;
use crate::routes::AppState;

// ============================================================================
// Name validation helpers
// ============================================================================

/// Validate an optional name override for a claim.
///
/// A blank or absent value yields `None` (the box keeps its auto name). A
/// non-blank value must satisfy [`is_valid_devbox_name`] (`400` otherwise).
/// Uniqueness is *not* checked here — it is enforced atomically at claim time by
/// [`DocumentStore::compare_and_update_unique`](crate::db::DocumentStore::compare_and_update_unique).
pub(crate) fn validate_name_override(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    if !is_valid_devbox_name(name) {
        return Err(AppError::BadRequest(format!(
            "invalid name '{name}': use 1-{DEVBOX_NAME_MAX_LEN} lowercase letters, \
             digits, '_' or '-', not starting with '-'"
        )));
    }

    Ok(Some(name.to_string()))
}

/// Validate a required name for a rename request.
///
/// Unlike [`validate_name_override`], a blank name is an error here — rename
/// requires a name. Trims whitespace, rejects empty with a 400, then checks
/// [`is_valid_devbox_name`] using the same message text as
/// `validate_name_override` for parity.
pub(crate) fn validate_rename_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest(format!(
            "invalid name '': use 1-{DEVBOX_NAME_MAX_LEN} lowercase letters, \
             digits, '_' or '-', not starting with '-'"
        )));
    }
    if !is_valid_devbox_name(name) {
        return Err(AppError::BadRequest(format!(
            "invalid name '{name}': use 1-{DEVBOX_NAME_MAX_LEN} lowercase letters, \
             digits, '_' or '-', not starting with '-'"
        )));
    }
    Ok(name.to_string())
}

// ============================================================================
// Domain operations
// ============================================================================

/// Claim a Ready box for `owner`, optionally setting its name to `name`.
///
/// Shared by the JSON API and the HTML dashboard. When a name override is given,
/// each candidate is claimed via [`compare_and_update_unique`](crate::db::DocumentStore::compare_and_update_unique),
/// which checks the name and writes the claim in one transaction — so two
/// concurrent claimants of the same name cannot both win (the DB rejects the
/// loser). A `DuplicateValue` means some live box already holds the name; the
/// loop continues, because that box may itself be a later candidate (the
/// uniqueness check excludes the box being claimed, so claiming it succeeds).
/// Only if no candidate can take the name does the claim fail with a `409`.
/// Without an override the box keeps its reconciler-assigned unique name, so a
/// plain version-guarded claim suffices.
pub(crate) async fn claim_devbox(
    state: &AppState,
    claimant: &Principal,
    name: Option<&str>,
) -> Result<Document<DevboxDoc>, AppError> {
    let name_override = validate_name_override(name)?;

    let ready_docs = state.store.find_all::<DevboxDoc>("state", "ready").await?;
    if ready_docs.is_empty() {
        return Err(AppError::Conflict("no devboxes available".into()));
    }

    // Sort candidates by created_at ascending (longest-waiting first).
    let mut candidates = ready_docs;
    candidates.sort_by_key(|a| a.data.created_at);

    // Set once a candidate reports the name as already held, so an exhausted
    // loop reports "name in use" rather than the generic pool message.
    let mut name_in_use = false;

    for candidate in candidates {
        let mut updated = candidate.data.clone();
        updated.state = DevboxState::Claimed;
        updated.owner = Some(claimant.owner.clone());
        updated.owner_email = Some(claimant.email.clone());
        updated.claimed_at = Some(jiff::Timestamp::now());
        updated.owner_tag_applied = false;

        let claimed = match name_override {
            Some(ref name) => {
                updated.name = name.clone();
                match state
                    .store
                    .compare_and_update_unique(
                        &candidate.id,
                        candidate.version,
                        &updated,
                        "name",
                        name,
                    )
                    .await?
                {
                    UpdateOutcome::Updated => true,
                    // Another claimer took this box; try the next candidate.
                    UpdateOutcome::VersionMismatch => continue,
                    // The name is held by another box. If that box is itself a
                    // later candidate we'll reach it and claim it; otherwise the
                    // loop exhausts and we report the name as in use.
                    UpdateOutcome::DuplicateValue => {
                        name_in_use = true;
                        continue;
                    }
                }
            }
            None => {
                state
                    .store
                    .compare_and_update(&candidate.id, candidate.version, &updated)
                    .await?
            }
        };

        if claimed {
            let refreshed = state
                .store
                .get::<DevboxDoc>(&candidate.id)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("devbox vanished after claim"))
                })?;
            return Ok(refreshed);
        }
    }

    match name_override {
        Some(name) if name_in_use => Err(AppError::Conflict(format!(
            "name '{name}' is already in use"
        ))),
        _ => Err(AppError::Conflict(
            "pool exhausted: all candidates failed concurrent claim".into(),
        )),
    }
}

/// Release a Claimed devbox on behalf of `caller`.
///
/// Shared by the JSON API and the HTML dashboard. Enforces:
/// - State must be `Claimed` (409 otherwise).
/// - Caller must be the box's owner (403 otherwise).
/// - Clears `owner` and frees `name` in the store (so both can be reused on a
///   fresh claim) atomically. The returned document still carries the released
///   box's `name` so callers can render a friendly confirmation.
pub(crate) async fn release_devbox(
    state: &AppState,
    caller: &str,
    id: &str,
) -> Result<Document<DevboxDoc>, AppError> {
    let doc = state
        .store
        .get::<DevboxDoc>(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("devbox '{id}' not found")))?;

    if doc.data.state != DevboxState::Claimed {
        return Err(AppError::Conflict(format!(
            "cannot release devbox in '{}' state",
            doc.data.state
        )));
    }

    let current_owner = doc.data.owner.as_deref().unwrap_or("");
    if current_owner != caller {
        return Err(AppError::Forbidden("ownership mismatch".into()));
    }

    let released_name = doc.data.name.clone();

    let mut updated = doc.data.clone();
    updated.state = DevboxState::Terminating;
    // Clear owner and free the name so both can be reused on a fresh claim.
    updated.owner = None;
    updated.name = String::new();

    let success = state
        .store
        .compare_and_update(&doc.id, doc.version, &updated)
        .await?;
    if !success {
        return Err(AppError::Conflict(
            "devbox was modified concurrently".into(),
        ));
    }

    let mut refreshed = state
        .store
        .get::<DevboxDoc>(id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("devbox vanished after release")))?;
    // The store record has freed the name for reuse; the response still reports
    // the released box's name so the caller's confirmation is friendly.
    refreshed.data.name = released_name;
    Ok(refreshed)
}

/// Rename a Claimed devbox to `new_name` on behalf of `caller`.
///
/// Shared by the JSON API and the HTML dashboard. Enforces:
/// - Name validity (400 on bad name).
/// - State must be `Claimed` (409 otherwise).
/// - Caller must be the box's owner (403 otherwise).
/// - No-op short-circuit when `new_name` equals the current name (200, unchanged).
/// - Uniqueness via [`compare_and_update_unique`](crate::db::DocumentStore::compare_and_update_unique).
///
/// `VersionMismatch` is retried up to `MAX_RETRIES` times: the reconciler's
/// `apply_pending_owner_tags` bumps the document version within ~30 s of a claim,
/// so a rename attempted in that window would otherwise get a spurious 409. A
/// re-fetch and retry is sufficient — no sleep — because the reconciler is the
/// only background writer between claim and stable state.
pub(crate) async fn rename_devbox(
    state: &AppState,
    caller: &str,
    id: &str,
    new_name: &str,
) -> Result<Document<DevboxDoc>, AppError> {
    let name = validate_rename_name(new_name)?;

    const MAX_RETRIES: u32 = 3;
    let mut attempts = 0u32;
    loop {
        let doc = state
            .store
            .get::<DevboxDoc>(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("devbox '{id}' not found")))?;

        if doc.data.state != DevboxState::Claimed {
            return Err(AppError::Conflict(format!(
                "cannot rename devbox in '{}' state",
                doc.data.state
            )));
        }

        let current_owner = doc.data.owner.as_deref().unwrap_or("");
        if current_owner != caller {
            return Err(AppError::Forbidden("ownership mismatch".into()));
        }

        // No-op: same name → return box unchanged without touching the store.
        if doc.data.name == name {
            return Ok(doc);
        }

        let mut updated = doc.data.clone();
        updated.name = name.clone();

        match state
            .store
            .compare_and_update_unique(&doc.id, doc.version, &updated, "name", &name)
            .await?
        {
            UpdateOutcome::Updated => {
                return state.store.get::<DevboxDoc>(id).await?.ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("devbox vanished after rename"))
                });
            }
            UpdateOutcome::VersionMismatch if attempts < MAX_RETRIES => {
                // The reconciler just bumped the version (owner-tag sync);
                // re-fetch and retry with the fresh version.
                attempts = attempts.saturating_add(1);
                continue;
            }
            UpdateOutcome::VersionMismatch => {
                return Err(AppError::Conflict(
                    "devbox was modified concurrently".into(),
                ));
            }
            UpdateOutcome::DuplicateValue => {
                return Err(AppError::Conflict(format!(
                    "name '{name}' is already in use"
                )));
            }
        }
    }
}

/// Compute pool metrics from the current document store state.
pub(crate) async fn pool_metrics(state: &AppState) -> Result<PoolMetricsResponse, AppError> {
    let docs = state.store.list_all::<DevboxDoc>().await?;

    let mut warming = 0u32;
    let mut ready = 0u32;
    let mut claimed = 0u32;
    let mut terminating = 0u32;

    for doc in &docs {
        match doc.data.state {
            DevboxState::Launching => {}
            DevboxState::Warming => warming = warming.saturating_add(1),
            DevboxState::Ready => ready = ready.saturating_add(1),
            DevboxState::Claimed => claimed = claimed.saturating_add(1),
            DevboxState::Terminating => terminating = terminating.saturating_add(1),
        }
    }

    let target = state.reconciler_config.target_warm_pool_size;
    let ready_delta = i32::try_from(target)
        .unwrap_or(i32::MAX)
        .saturating_sub(i32::try_from(ready).unwrap_or(0));

    Ok(PoolMetricsResponse {
        warming,
        ready,
        claimed,
        terminating,
        target_warm_pool_size: target,
        ready_delta,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::time::Duration;

    use devbox_common::{AmiId, InstanceType, SubnetId};
    use jiff::Timestamp;

    use super::*;
    use crate::auth::{Authenticator, Principal};
    use crate::db::DocumentStore;
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::Pool;
    use crate::reconcile::ReconcilerConfig;
    use crate::routes::AppState;

    fn claimant(login: &str) -> Principal {
        Principal {
            owner: login.to_string(),
            email: format!("{login}@example.com"),
        }
    }

    async fn test_store() -> DocumentStore {
        let pool = Pool::new_test();
        if let Pool::Sqlite(ref p) = pool {
            run_sqlite_migrations(p).await.unwrap();
        }
        DocumentStore::new(pool)
    }

    fn test_config() -> ReconcilerConfig {
        ReconcilerConfig {
            pool_id: "test".to_string(),
            server_id: "test-server".to_string(),
            target_warm_pool_size: 1,
            polling_interval: Duration::from_secs(30),
            lock_ttl: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(60),
        }
    }

    async fn setup_state() -> AppState {
        AppState {
            store: std::sync::Arc::new(test_store().await),
            reconciler_config: test_config(),
            auth: Authenticator::with_test_owner("jdoe"),
            aws_account_id: None,
        }
    }

    fn ready_devbox() -> DevboxDoc {
        DevboxDoc {
            instance_id: "i-1234567890abcdef0".to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: None,
            owner_email: None,
            claimed_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
        }
    }

    fn ready_devbox_other() -> DevboxDoc {
        let mut doc = ready_devbox();
        doc.instance_id = "i-0987654321fedcba0".to_string();
        doc.name = "brave-otter".to_string();
        doc
    }

    fn claimed_devbox_for(owner: &str) -> DevboxDoc {
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some(owner.to_string());
        doc
    }

    async fn insert(state: &AppState, doc: DevboxDoc) -> String {
        state.store.insert(&doc).await.unwrap().id
    }

    // -----------------------------------------------------------------------
    // claim tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn claim_marks_box_claimed_and_binds_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), None)
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.state, DevboxState::Claimed);
        assert_eq!(doc.data.owner.as_deref(), Some("jdoe"));
        assert_eq!(doc.data.owner_email.as_deref(), Some("jdoe@example.com"));
    }

    #[tokio::test]
    async fn claim_surfaces_region_from_doc() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), None)
            .await
            .ok()
            .unwrap();

        // The instance's region (from instance metadata, carried on the doc) is
        // surfaced so the CLI can open the SSM tunnel without client-side config.
        assert_eq!(doc.data.region, "us-east-1");
    }

    #[tokio::test]
    async fn claim_keeps_auto_name_when_no_override() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), None)
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_applies_valid_name_override() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("my-project"))
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "my-project");
        assert_eq!(doc.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn claim_blank_override_keeps_auto_name() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("   "))
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_invalid_name_is_bad_request() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let err = claim_devbox(&state, &claimant("jdoe"), Some("Bad Name"))
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn claim_name_matching_a_ready_box_claims_that_box() {
        // A ready box already carries the requested name, and an older box with a
        // different name sorts ahead of it. The claim must succeed by claiming the
        // box that holds the name — not abort because the older candidate can't
        // take it.
        let state = setup_state().await;
        // Older candidate, different name → tried first.
        let mut older = ready_devbox_other();
        older.created_at = Timestamp::from_second(0).unwrap();
        older.name = "older-box".to_string();
        insert(&state, older).await;
        // The box that already has the requested name (i-1234…, "calm-quilt").
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("calm-quilt"))
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "calm-quilt");
        assert_eq!(
            doc.data.instance_id, "i-1234567890abcdef0",
            "must claim the box that already holds the name"
        );
        assert_eq!(doc.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn claim_duplicate_name_is_conflict() {
        let state = setup_state().await;
        // An already-claimed box named "taken".
        let mut existing = ready_devbox_other();
        existing.state = DevboxState::Claimed;
        existing.owner = Some("alice".to_string());
        existing.name = "taken".to_string();
        insert(&state, existing).await;
        // A ready box to claim with the colliding name.
        insert(&state, ready_devbox()).await;

        let err = claim_devbox(&state, &claimant("jdoe"), Some("taken"))
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn concurrent_named_claims_do_not_duplicate_a_name() {
        // Two ready boxes, two simultaneous claims for the same name. Exactly one
        // must win the name; the other must be rejected and its box returned to
        // the pool — never two boxes sharing a name (the selector guarantee).
        let state = std::sync::Arc::new(setup_state().await);
        insert(&state, ready_devbox()).await;
        insert(&state, ready_devbox_other()).await;

        let s1 = state.clone();
        let s2 = state.clone();
        let p = claimant("jdoe");
        let (r1, r2) = tokio::join!(
            claim_devbox(&s1, &p, Some("shared")),
            claim_devbox(&s2, &p, Some("shared")),
        );

        let ok_count = [r1.is_ok(), r2.is_ok()].iter().filter(|b| **b).count();
        // A later committer always observes an earlier one, so at most one wins.
        assert!(ok_count <= 1, "at most one named claim may win");

        // The safety property: the name is held by exactly as many live boxes as
        // claims won — never two (which would break the `ssh <name>` selector).
        let holders = state
            .store
            .find_all::<DevboxDoc>("name", "shared")
            .await
            .unwrap();
        let live = holders
            .iter()
            .filter(|d| d.data.state != DevboxState::Terminating)
            .count();
        assert_eq!(
            live, ok_count,
            "a name must be held by exactly the winner (0 or 1)"
        );
    }

    #[tokio::test]
    async fn claim_empty_pool_is_conflict() {
        let state = setup_state().await;

        let err = claim_devbox(&state, &claimant("jdoe"), None)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn concurrent_claims_yield_one_winner_one_conflict() {
        let state = std::sync::Arc::new(setup_state().await);
        insert(&state, ready_devbox()).await;

        let s1 = state.clone();
        let s2 = state.clone();
        let p = claimant("jdoe");
        let (r1, r2) = tokio::join!(claim_devbox(&s1, &p, None), claim_devbox(&s2, &p, None),);

        let ok = [r1.is_ok(), r2.is_ok()].iter().filter(|b| **b).count();
        let conflict = [r1.is_err(), r2.is_err()].iter().filter(|b| **b).count();
        assert_eq!(ok, 1, "exactly one claim must win");
        assert_eq!(conflict, 1, "the loser must get a Conflict error");
    }

    // -----------------------------------------------------------------------
    // release tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn release_by_non_owner_is_forbidden() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some("alice".to_string());
        let id = insert(&state, doc).await;

        let err = release_devbox(&state, "bob", &id).await.err().unwrap();

        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn release_of_unclaimed_box_is_conflict() {
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let err = release_devbox(&state, "jdoe", &id).await.err().unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn release_clears_owner_and_name() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some("jdoe".to_string());
        let id = insert(&state, doc).await;

        let refreshed = release_devbox(&state, "jdoe", &id).await.ok().unwrap();

        assert_eq!(refreshed.data.state, DevboxState::Terminating);
        assert!(
            refreshed.data.owner.is_none(),
            "owner must be cleared on release"
        );
        // The response echoes the released box's name for a friendly confirmation...
        assert_eq!(refreshed.data.name, "calm-quilt");

        // ...but the persisted record frees the name for reuse on a fresh claim.
        let persisted = state
            .store
            .get::<DevboxDoc>(&id)
            .await
            .ok()
            .flatten()
            .unwrap();
        assert!(
            persisted.data.name.is_empty(),
            "name must be freed in the store"
        );
    }

    // -----------------------------------------------------------------------
    // rename tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rename_happy_path_changes_name() {
        let state = setup_state().await;
        let mut doc = claimed_devbox_for("jdoe");
        doc.name = "calm-quilt".to_string();
        let id = insert(&state, doc).await;

        let result = rename_devbox(&state, "jdoe", &id, "my-feature")
            .await
            .ok()
            .unwrap();

        assert_eq!(result.data.name, "my-feature");
        assert_eq!(result.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn rename_by_non_owner_is_forbidden() {
        let state = setup_state().await;
        let doc = claimed_devbox_for("alice");
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "bob", &id, "stolen-name")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn rename_of_ready_box_is_conflict() {
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let err = rename_devbox(&state, "jdoe", &id, "new-name")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn rename_to_taken_name_is_conflict() {
        let state = setup_state().await;
        // Another live box that already holds the target name.
        let mut other = ready_devbox_other();
        other.state = DevboxState::Claimed;
        other.owner = Some("alice".to_string());
        other.name = "taken".to_string();
        insert(&state, other).await;

        let mut doc = claimed_devbox_for("jdoe");
        doc.name = "calm-quilt".to_string();
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "jdoe", &id, "taken")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn rename_with_invalid_name_is_bad_request() {
        let state = setup_state().await;
        let doc = claimed_devbox_for("jdoe");
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "jdoe", &id, "Bad Name!!")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn rename_to_current_name_is_noop() {
        let state = setup_state().await;
        let mut doc = claimed_devbox_for("jdoe");
        doc.name = "calm-quilt".to_string();
        let id = insert(&state, doc).await;

        let before = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();

        let result = rename_devbox(&state, "jdoe", &id, "calm-quilt")
            .await
            .ok()
            .unwrap();

        assert_eq!(result.data.name, "calm-quilt");

        let after = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        assert_eq!(
            before.version, after.version,
            "no-op rename must not bump the version"
        );
    }

    #[tokio::test]
    async fn rename_of_warming_box_is_conflict() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Warming;
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "jdoe", &id, "new-name")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn rename_of_terminating_box_is_conflict() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Terminating;
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "jdoe", &id, "new-name")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn rename_of_nonexistent_devbox_is_not_found() {
        let state = setup_state().await;

        let err = rename_devbox(&state, "jdoe", "i-does-not-exist", "new-name")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn rename_with_empty_name_is_bad_request() {
        let state = setup_state().await;
        let doc = claimed_devbox_for("jdoe");
        let id = insert(&state, doc).await;

        let err = rename_devbox(&state, "jdoe", &id, "   ")
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn old_name_is_reclaimable_after_rename() {
        let state = setup_state().await;
        // A second ready box to claim into with the old name.
        insert(&state, ready_devbox_other()).await;
        // Claim the first box with a known name.
        let mut doc = claimed_devbox_for("jdoe");
        doc.name = "old-name".to_string();
        let id = insert(&state, doc).await;

        // Rename away from "old-name".
        rename_devbox(&state, "jdoe", &id, "new-name")
            .await
            .ok()
            .unwrap();

        // The old name must now be claimable (uniqueness constraint freed).
        let claimed = claim_devbox(&state, &claimant("alice"), Some("old-name")).await;
        assert!(claimed.is_ok(), "old name must be reclaimable after rename");
    }
}
