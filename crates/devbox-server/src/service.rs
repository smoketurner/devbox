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

use std::future::Future;

use devbox_common::{
    DEVBOX_NAME_MAX_LEN, DevboxState, GitTokenResponse, PoolMetricsResponse,
    SessionArchiveDoneRequest, SessionResponse, SessionState, WarmupReportRequest,
    is_valid_devbox_name,
};

use crate::auth::{AgentIdentity, AgentRole, Principal};
use crate::db::UpdateOutcome;
use crate::db::document_type::Document;
use crate::documents::devbox::{DevboxDoc, PendingArchive, WarmupReport};
use crate::documents::session::SessionDoc;
use crate::error::AppError;
use crate::routes::AppState;
use crate::sessions::SessionArchives;

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
// Agent git-token minting
// ============================================================================

/// Mint a short-lived, repo-scoped, read-only GitHub token for a verified devbox
/// host.
///
/// Authorization is the verified [`AgentIdentity`] itself — a trusted devbox box
/// (its `sub` matched a trusted role ARN and `aws_account` the platform account in
/// [`crate::auth`]). There is deliberately **no** owner check (a warming box has
/// no owner yet) and **no** devbox-side repo allowlist: the GitHub App
/// installation is the authorization boundary, so a remote the App can't access
/// fails at GitHub rather than here.
///
/// Returns a response with `token: None` when `remote` is not a repository on the
/// App's GitHub host (the agent then fetches unauthenticated, as before).
///
/// # Errors
///
/// [`AppError::ServiceUnavailable`] when the server has no minter configured
/// (`DEVBOX_GITHUB_APP_ID`/`DEVBOX_GITHUB_KEY_PARAM` unset); [`AppError::Internal`]
/// when a GitHub API call fails (including when the App is not installed on the
/// requested repo).
pub(crate) async fn mint_git_token(
    state: &AppState,
    agent: &AgentIdentity,
    remote: &str,
) -> Result<GitTokenResponse, AppError> {
    let minter = state.minter.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailable("GitHub token minting is not configured".to_string())
    })?;

    match minter.mint_for_remote(remote).await {
        Ok(Some((repository, token))) => {
            tracing::info!(
                instance_id = %agent.instance_id,
                repository = %repository,
                "minted repo-scoped GitHub token"
            );
            Ok(GitTokenResponse {
                repository: Some(repository),
                token: Some(token),
            })
        }
        Ok(None) => Ok(GitTokenResponse {
            repository: None,
            token: None,
        }),
        Err(e) => Err(AppError::Internal(e)),
    }
}

// ============================================================================
// Optimistic-concurrency retry
// ============================================================================

/// Version-conflict retries granted to an optimistic-concurrency loop before it
/// gives up with a 409. The common conflicting writer is the reconciler (owner
/// tagging, `Warming → Ready`), which touches a document at most once per tick,
/// so one retry usually suffices.
const MAX_OCC_RETRIES: u32 = 3;

/// Outcome of one optimistic-concurrency attempt in [`with_occ_retry`].
enum OccAttempt<T> {
    /// The operation finished (successfully wrote, or legitimately no-oped).
    Done(T),
    /// The document version moved underneath the attempt; re-run on fresh state.
    Retry,
}

/// Run `attempt` until it completes or [`MAX_OCC_RETRIES`] version conflicts
/// are exhausted (then 409 Conflict).
///
/// Each attempt must **re-read its document** so a retry operates on fresh
/// state — re-running a CAS with a stale version is deterministically futile,
/// and the re-read is also where business rules get re-validated against the
/// document's *new* state (a retry may legitimately become a 403/404/409).
/// This is a different retry domain from transient DSQL errors, which the
/// store already retries generically via
/// [`with_dsql_retry!`](crate::with_dsql_retry).
///
/// Takes `FnMut() -> Fut` rather than `AsyncFnMut` so the returned future's
/// `Send`-ness stays inferable inside axum handlers (async-closure futures
/// currently fail higher-ranked `Send` proofs); capture by `Copy` reference
/// and return an `async move` block.
async fn with_occ_retry<T, Fut>(mut attempt: impl FnMut() -> Fut) -> Result<T, AppError>
where
    Fut: Future<Output = Result<OccAttempt<T>, AppError>>,
{
    let mut attempts = 0u32;
    loop {
        match attempt().await? {
            OccAttempt::Done(value) => return Ok(value),
            OccAttempt::Retry if attempts < MAX_OCC_RETRIES => {
                attempts = attempts.saturating_add(1);
            }
            OccAttempt::Retry => {
                return Err(AppError::Conflict(
                    "devbox was modified concurrently".into(),
                ));
            }
        }
    }
}

// ============================================================================
// Agent warmup report
// ============================================================================

/// Record a warm-up report from a verified pool host onto its [`DevboxDoc`].
///
/// A later report replaces an earlier one (last-writer-wins — a box warms once,
/// so a second report is a re-run, not a merge).
///
/// # Errors
///
/// [`AppError::Forbidden`] for a non-pool (builder) identity — builders have no
/// `DevboxDoc`; [`AppError::NotFound`] when no doc exists for the instance
/// (e.g. the reconciler hasn't adopted it yet, or it was already reaped);
/// [`AppError::Conflict`] when the write retries are exhausted.
pub(crate) async fn record_warmup_report(
    state: &AppState,
    agent: &AgentIdentity,
    report: &WarmupReportRequest,
) -> Result<(), AppError> {
    if agent.role != AgentRole::Pool {
        return Err(AppError::Forbidden(
            "only pool hosts have a devbox record".into(),
        ));
    }

    let stored = WarmupReport::from_request(report, jiff::Timestamp::now());
    update_devbox_by_instance(state, agent.instance_id.as_str(), |doc| {
        doc.warmup_report = Some(stored.clone());
    })
    .await?;

    tracing::info!(
        instance_id = %agent.instance_id,
        total_ms = report.total_ms,
        freshen_total_ms = report.freshen_total_ms,
        workspace_present = report.workspace_present,
        repos = report.repos.len(),
        "recorded warmup report"
    );
    Ok(())
}

/// Apply `mutate` to the [`DevboxDoc`] whose `instance_id` matches, under
/// [`with_occ_retry`].
///
/// The version guard matters here: agent writes race the reconciler (which
/// flips the same doc `Warming → Ready` right when the warmup report arrives),
/// and an unguarded whole-document write from a stale read would silently
/// revert the reconciler's state change. Shared shape for agent-reported
/// fields (warmup report now; heartbeats later).
async fn update_devbox_by_instance(
    state: &AppState,
    instance_id: &str,
    mutate: impl Fn(&mut DevboxDoc),
) -> Result<(), AppError> {
    let mutate = &mutate;
    with_occ_retry(move || async move {
        let doc = state
            .store
            .find_one::<DevboxDoc>("instance_id", instance_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("no devbox record for instance '{instance_id}'"))
            })?;

        let mut updated = doc.data.clone();
        mutate(&mut updated);

        if state
            .store
            .compare_and_update(&doc.id, doc.version, &updated)
            .await?
        {
            Ok(OccAttempt::Done(()))
        } else {
            Ok(OccAttempt::Retry)
        }
    })
    .await
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
    resume: Option<&str>,
) -> Result<Document<DevboxDoc>, AppError> {
    let name_override = validate_name_override(name)?;

    // Resolve the session to restore before racing for a box, so a bad
    // selector fails without consuming a claim. Requires session archiving to
    // be configured: without the bucket the box's restore-url call would 503
    // and the claim would quietly come up without the session.
    let restore_session_id = match resume {
        Some(selector) => {
            if state.sessions.is_none() {
                return Err(AppError::Conflict(
                    "session archiving is not configured on this server".into(),
                ));
            }
            Some(resolve_resumable_session(state, &claimant.owner, selector).await?)
        }
        None => None,
    };

    let ready_docs = state.store.find_all::<DevboxDoc>("state", "ready").await?;
    if ready_docs.is_empty() {
        let warming = state
            .store
            .find_all::<DevboxDoc>("state", "warming")
            .await?
            .len();
        let msg = if warming > 0 {
            format!("no devboxes ready for use ({warming} warming)")
        } else {
            "no devboxes ready for use".to_string()
        };
        return Err(AppError::Conflict(msg));
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
        // The restore signal reaches the box as the `devbox:session-restore`
        // tag, applied together with the owner tags (see DevboxDoc::owner_tags).
        updated.restore_session_id = restore_session_id.clone();

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
            // Apply the owner tag immediately so the box is loginable without
            // waiting for the next reconciler tick (the dominant first-SSH
            // latency). The reconciler re-asserts the same tag as a fallback, so
            // a failure here is non-fatal; skipped when no compute client is
            // configured (tests), where the reconciler does the tagging.
            if let Some(compute) = state.compute.as_deref() {
                crate::reconcile::apply_owner_tag(&state.store, compute, &refreshed).await;
            }
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
///
/// A plain release flips the box to `Terminating`, clearing `owner` and freeing
/// `name` (so both can be reused on a fresh claim) atomically. The returned
/// document still carries the released box's `name` so callers can render a
/// friendly confirmation.
///
/// With `keep_session` the box detours through `Archiving` instead: a pending
/// [`SessionDoc`] is created, the box keeps its owner and name while the on-box
/// agent uploads the archive, and the `devbox:archive-session` tag signals the
/// agent (asserted inline here, re-asserted by the reconciler). The box flips to
/// `Terminating` when the agent reports done — or when the reconciler's archive
/// deadline passes. Requires session archiving to be configured (409 otherwise),
/// and returns the created session so the caller learns the `--resume` selector.
pub(crate) async fn release_devbox(
    state: &AppState,
    caller: &str,
    id: &str,
    keep_session: bool,
) -> Result<(Document<DevboxDoc>, Option<SessionResponse>), AppError> {
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

    if keep_session {
        return release_with_archive(state, caller, doc).await;
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
    Ok((refreshed, None))
}

/// The `--keep` half of [`release_devbox`]: create the pending session and move
/// the box to `Archiving` (owner and name kept — the box is still doing work on
/// the caller's behalf, and the session inherits the name when it completes).
async fn release_with_archive(
    state: &AppState,
    caller: &str,
    doc: Document<DevboxDoc>,
) -> Result<(Document<DevboxDoc>, Option<SessionResponse>), AppError> {
    let Some(archives) = state.sessions.as_deref() else {
        return Err(AppError::Conflict(
            "session archiving is not configured on this server".into(),
        ));
    };

    let now = jiff::Timestamp::now();
    let session_id = uuid::Uuid::now_v7().to_string();
    let session = SessionDoc {
        name: doc.data.name.clone(),
        owner: caller.to_string(),
        state: SessionState::Pending,
        source_instance_id: doc.data.instance_id.clone(),
        s3_key: SessionArchives::object_key(&session_id),
        size_bytes: None,
        created_at: now,
        completed_at: None,
        expires_at: archives.expires_at(now),
        error: None,
    };
    let inserted = state.store.insert_with_id(&session_id, &session).await?;

    let mut updated = doc.data.clone();
    updated.state = DevboxState::Archiving;
    updated.archive = Some(PendingArchive {
        session_id: session_id.clone(),
        requested_at: now,
    });

    let success = state
        .store
        .compare_and_update(&doc.id, doc.version, &updated)
        .await?;
    if !success {
        // The box moved under us; drop the just-created session record.
        state.store.delete(&session_id).await?;
        return Err(AppError::Conflict(
            "devbox was modified concurrently".into(),
        ));
    }

    // Signal the box inline so archiving starts without waiting a reconciler
    // tick; the reconciler re-asserts this tag every tick while Archiving, so
    // a failure here only delays the start.
    if let Some(compute) = state.compute.as_deref() {
        crate::reconcile::apply_archive_tag(compute, &updated).await;
    }

    let refreshed = state
        .store
        .get::<DevboxDoc>(&doc.id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("devbox vanished after release")))?;
    Ok((refreshed, Some(inserted.into())))
}

/// The caller's session archives, newest first.
pub(crate) async fn list_sessions(
    state: &AppState,
    caller: &str,
) -> Result<Vec<SessionResponse>, AppError> {
    let mut docs = state.store.find_all::<SessionDoc>("owner", caller).await?;
    docs.sort_by_key(|d| std::cmp::Reverse(d.data.created_at));
    Ok(docs.into_iter().map(SessionResponse::from).collect())
}

/// Resolve a `--resume` selector (session id or name) to the caller's newest
/// **complete** session id.
///
/// A selector that matches only pending/failed sessions is a 409 (it exists but
/// cannot be restored); no match at all is a 404.
async fn resolve_resumable_session(
    state: &AppState,
    caller: &str,
    selector: &str,
) -> Result<String, AppError> {
    let mut docs = state.store.find_all::<SessionDoc>("owner", caller).await?;
    docs.sort_by_key(|d| std::cmp::Reverse(d.data.created_at));

    let mut matched_incomplete = false;
    for doc in docs {
        if doc.id != selector && doc.data.name != selector {
            continue;
        }
        if doc.data.state == SessionState::Complete {
            return Ok(doc.id);
        }
        matched_incomplete = true;
    }

    if matched_incomplete {
        Err(AppError::Conflict(format!(
            "session '{selector}' is not complete and cannot be resumed"
        )))
    } else {
        Err(AppError::NotFound(format!("no session '{selector}'")))
    }
}

// ============================================================================
// Agent session archive/restore
// ============================================================================

/// Mint a presigned PUT URL for the archive this box was asked to produce.
///
/// The caller must be a pool host whose `DevboxDoc.archive` names exactly this
/// session — the box can only upload the archive it was assigned.
pub(crate) async fn session_archive_url(
    state: &AppState,
    agent: &AgentIdentity,
    session_id: &str,
) -> Result<String, AppError> {
    let archives = require_sessions(state)?;
    let doc = devbox_for_agent(state, agent).await?;
    let assigned = doc
        .data
        .archive
        .as_ref()
        .is_some_and(|a| a.session_id == session_id);
    if !assigned {
        return Err(AppError::Forbidden(
            "session does not match this instance's archive assignment".into(),
        ));
    }
    let session = get_session(state, session_id).await?;
    archives
        .presigned_put(&session.data.s3_key)
        .await
        .map_err(AppError::Internal)
}

/// Record the outcome of an archive upload: mark the [`SessionDoc`]
/// complete/failed and flip the box `Archiving → Terminating` (owner cleared,
/// name freed) so the reconciler terminates it on its next tick.
pub(crate) async fn session_archive_done(
    state: &AppState,
    agent: &AgentIdentity,
    report: &SessionArchiveDoneRequest,
) -> Result<(), AppError> {
    let doc = devbox_for_agent(state, agent).await?;
    let assigned = doc
        .data
        .archive
        .as_ref()
        .is_some_and(|a| a.session_id == report.session_id);
    if !assigned {
        return Err(AppError::Forbidden(
            "session does not match this instance's archive assignment".into(),
        ));
    }

    let now = jiff::Timestamp::now();
    finish_session(state, &report.session_id, |session| {
        if report.success {
            session.state = SessionState::Complete;
            session.size_bytes = report.size_bytes;
        } else {
            session.state = SessionState::Failed;
            session.error = report
                .error
                .as_deref()
                .map(SessionDoc::truncate_error)
                .or_else(|| Some("archive failed".to_string()));
        }
        session.completed_at = Some(now);
    })
    .await?;

    update_devbox_by_instance(state, agent.instance_id.as_str(), |doc| {
        doc.state = DevboxState::Terminating;
        doc.archive = None;
        doc.owner = None;
        doc.name = String::new();
    })
    .await?;

    tracing::info!(
        instance_id = %agent.instance_id,
        session_id = %report.session_id,
        success = report.success,
        "recorded session archive outcome"
    );
    Ok(())
}

/// Mint a presigned GET URL for the session this box was asked to restore
/// (`DevboxDoc.restore_session_id`, set by `claim --resume`).
pub(crate) async fn session_restore_url(
    state: &AppState,
    agent: &AgentIdentity,
    session_id: &str,
) -> Result<String, AppError> {
    let archives = require_sessions(state)?;
    let doc = devbox_for_agent(state, agent).await?;
    if doc.data.restore_session_id.as_deref() != Some(session_id) {
        return Err(AppError::Forbidden(
            "session does not match this instance's restore assignment".into(),
        ));
    }
    let session = get_session(state, session_id).await?;
    if session.data.state != SessionState::Complete {
        return Err(AppError::Conflict(
            "session is not complete and cannot be restored".into(),
        ));
    }
    archives
        .presigned_get(&session.data.s3_key)
        .await
        .map_err(AppError::Internal)
}

/// The configured session-archive presigner, or a 503 when the server has no
/// bucket configured (`DEVBOX_SESSION_BUCKET` unset).
fn require_sessions(state: &AppState) -> Result<&SessionArchives, AppError> {
    state.sessions.as_deref().ok_or_else(|| {
        AppError::ServiceUnavailable("session archiving is not configured".to_string())
    })
}

/// The pool host's own [`DevboxDoc`] (403 for builder identities, 404 when the
/// instance has no record).
async fn devbox_for_agent(
    state: &AppState,
    agent: &AgentIdentity,
) -> Result<Document<DevboxDoc>, AppError> {
    if agent.role != AgentRole::Pool {
        return Err(AppError::Forbidden(
            "only pool hosts have a devbox record".into(),
        ));
    }
    state
        .store
        .find_one::<DevboxDoc>("instance_id", agent.instance_id.as_str())
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "no devbox record for instance '{}'",
                agent.instance_id
            ))
        })
}

/// Fetch a [`SessionDoc`] by id (404 when absent).
async fn get_session(state: &AppState, id: &str) -> Result<Document<SessionDoc>, AppError> {
    state
        .store
        .get::<SessionDoc>(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no session '{id}'")))
}

/// Apply `mutate` to a [`SessionDoc`] under [`with_occ_retry`] (the reconciler's
/// deadline pass races the agent's done report on the same record).
async fn finish_session(
    state: &AppState,
    session_id: &str,
    mutate: impl Fn(&mut SessionDoc),
) -> Result<(), AppError> {
    let mutate = &mutate;
    with_occ_retry(move || async move {
        let doc = get_session(state, session_id).await?;
        let mut updated = doc.data.clone();
        mutate(&mut updated);
        if state
            .store
            .compare_and_update(&doc.id, doc.version, &updated)
            .await?
        {
            Ok(OccAttempt::Done(()))
        } else {
            Ok(OccAttempt::Retry)
        }
    })
    .await
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
/// `VersionMismatch` is retried via [`with_occ_retry`]: the reconciler's
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

    let name = &name;
    with_occ_retry(move || async move {
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
        if doc.data.name == *name {
            return Ok(OccAttempt::Done(doc));
        }

        let mut updated = doc.data.clone();
        updated.name = name.clone();

        match state
            .store
            .compare_and_update_unique(&doc.id, doc.version, &updated, "name", name)
            .await?
        {
            UpdateOutcome::Updated => {
                let refreshed = state.store.get::<DevboxDoc>(id).await?.ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("devbox vanished after rename"))
                })?;
                Ok(OccAttempt::Done(refreshed))
            }
            // The reconciler just bumped the version (owner-tag sync);
            // re-fetch and retry with the fresh version.
            UpdateOutcome::VersionMismatch => Ok(OccAttempt::Retry),
            UpdateOutcome::DuplicateValue => Err(AppError::Conflict(format!(
                "name '{name}' is already in use"
            ))),
        }
    })
    .await
}

/// Compute pool metrics from the current document store state.
pub(crate) async fn pool_metrics(state: &AppState) -> Result<PoolMetricsResponse, AppError> {
    let docs = state.store.list_all::<DevboxDoc>().await?;

    let mut warming = 0u32;
    let mut ready = 0u32;
    let mut claimed = 0u32;
    let mut archiving = 0u32;
    let mut terminating = 0u32;
    let mut warm = 0u32;

    for doc in &docs {
        match doc.data.state {
            DevboxState::Launching => {}
            DevboxState::Warming => warming = warming.saturating_add(1),
            DevboxState::Ready => ready = ready.saturating_add(1),
            DevboxState::Claimed => claimed = claimed.saturating_add(1),
            DevboxState::Archiving => archiving = archiving.saturating_add(1),
            DevboxState::Terminating => terminating = terminating.saturating_add(1),
        }
        // Warmth is only meaningful for claimable/claimed boxes: Warming can't
        // have a report yet and Archiving/Terminating are leaving the pool.
        if matches!(doc.data.state, DevboxState::Ready | DevboxState::Claimed)
            && doc.data.warmup_report.as_ref().is_some_and(|r| r.warm)
        {
            warm = warm.saturating_add(1);
        }
    }

    Ok(PoolMetricsResponse {
        warming,
        ready,
        claimed,
        archiving,
        terminating,
        warm,
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
    use devbox_common::{AmiId, InstanceId, InstanceType, SubnetId};
    use jiff::Timestamp;

    use super::*;
    use crate::auth::{AgentRole, Authenticator, Principal};
    use crate::compute::mock::MockCompute;
    use crate::db::DocumentStore;
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::Pool;
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

    async fn setup_state() -> AppState {
        AppState {
            store: std::sync::Arc::new(test_store().await),
            auth: Authenticator::with_test_owner("jdoe"),
            aws_account_id: None,
            minter: None,
            compute: None,
            sessions: None,
        }
    }

    fn claimed_devbox(instance_id: &str, owner: &str) -> DevboxDoc {
        DevboxDoc {
            instance_id: instance_id.to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some(owner.to_string()),
            owner_email: Some(format!("{owner}@example.com")),
            claimed_at: Some(Timestamp::now()),
            ready_at: None,
            archive: None,
            restore_session_id: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        }
    }

    /// A successful inline tag applies both owner tags to the instance and flips
    /// `owner_tag_applied` so the reconciler's next tick is a no-op.
    #[tokio::test]
    async fn inline_tag_applies_owner_tags_and_flips_flag() {
        let store = test_store().await;
        let compute = MockCompute::new();
        let iid = compute.add_instance("InService");
        let inserted = store.insert(&claimed_devbox(&iid, "jdoe")).await.unwrap();
        let doc = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();

        crate::reconcile::apply_owner_tag(&store, &compute, &doc).await;

        let tags = compute.get_instance_tags(&iid).unwrap();
        assert_eq!(tags.get("devbox:owner").map(String::as_str), Some("jdoe"));
        assert_eq!(
            tags.get("devbox:owner-email").map(String::as_str),
            Some("jdoe@example.com")
        );

        let after = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert!(after.data.owner_tag_applied);
    }

    /// A failed inline tag leaves `owner_tag_applied` false so the reconciler
    /// re-applies it on its next tick (best-effort, not fatal to the claim).
    #[tokio::test]
    async fn inline_tag_failure_leaves_flag_false_for_reconciler() {
        let store = test_store().await;
        let compute = MockCompute::new();
        let iid = compute.add_instance("InService");
        compute.set_error("tag_instance", "transient EC2 error".to_string());
        let inserted = store.insert(&claimed_devbox(&iid, "jdoe")).await.unwrap();
        let doc = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();

        crate::reconcile::apply_owner_tag(&store, &compute, &doc).await;

        let after = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert!(!after.data.owner_tag_applied);
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
            ready_at: None,
            archive: None,
            restore_session_id: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
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

    fn pool_agent() -> AgentIdentity {
        AgentIdentity {
            instance_id: InstanceId("i-1234567890abcdef0".to_string()),
            role: AgentRole::Pool,
            owner: None,
        }
    }

    // -----------------------------------------------------------------------
    // git-token tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mint_git_token_without_minter_is_service_unavailable() {
        // The default test state configures no minter; mint_git_token must report
        // 503 Service Unavailable (not 500), so a box can distinguish "minting not
        // configured" from a genuine server fault.
        let state = setup_state().await;
        let err = mint_git_token(&state, &pool_agent(), "https://github.com/o/r.git")
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::ServiceUnavailable(_)));
    }

    // -----------------------------------------------------------------------
    // with_occ_retry tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn occ_retry_reruns_until_done() {
        // An attempt that conflicts a few times then succeeds must be re-run
        // (on what would be fresh state) and yield the final value.
        let calls = std::cell::Cell::new(0u32);
        let calls = &calls;
        let result = with_occ_retry(move || async move {
            calls.set(calls.get().saturating_add(1));
            if calls.get() <= MAX_OCC_RETRIES {
                Ok(OccAttempt::Retry)
            } else {
                Ok(OccAttempt::Done(calls.get()))
            }
        })
        .await
        .ok()
        .unwrap();

        assert_eq!(result, MAX_OCC_RETRIES + 1);
    }

    #[tokio::test]
    async fn occ_retry_exhaustion_is_conflict() {
        // A persistently conflicting attempt is bounded: after the retries are
        // spent the caller gets a 409, never an unbounded loop.
        let calls = std::cell::Cell::new(0u32);
        let calls = &calls;
        let err = with_occ_retry(move || async move {
            calls.set(calls.get().saturating_add(1));
            Ok::<OccAttempt<()>, AppError>(OccAttempt::Retry)
        })
        .await
        .err()
        .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
        // Initial attempt + MAX_OCC_RETRIES re-runs.
        assert_eq!(calls.get(), MAX_OCC_RETRIES + 1);
    }

    #[tokio::test]
    async fn occ_retry_propagates_domain_errors_immediately() {
        // A domain error from the attempt (403/404/409 from re-validation) is
        // returned as-is, not retried — only version conflicts re-run.
        let calls = std::cell::Cell::new(0u32);
        let calls = &calls;
        let err = with_occ_retry(move || async move {
            calls.set(calls.get().saturating_add(1));
            Err::<OccAttempt<()>, AppError>(AppError::Forbidden("ownership mismatch".into()))
        })
        .await
        .err()
        .unwrap();

        assert!(matches!(err, AppError::Forbidden(_)));
        assert_eq!(calls.get(), 1);
    }

    // -----------------------------------------------------------------------
    // warmup-report tests
    // -----------------------------------------------------------------------

    fn sample_report() -> WarmupReportRequest {
        WarmupReportRequest {
            docker_start_ms: 850,
            freshen_total_ms: 12_000,
            total_ms: 13_500,
            workspace_present: true,
            repos: vec![devbox_common::RepoFreshenReport {
                repo: "devbox".to_string(),
                success: true,
                duration_ms: 11_000,
                error: None,
            }],
            warm: true,
        }
    }

    #[tokio::test]
    async fn warmup_report_persists_on_devbox_doc() {
        // pool_agent()'s instance id matches ready_devbox()'s, so the report
        // resolves to that doc and lands on its warmup_report field.
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        record_warmup_report(&state, &pool_agent(), &sample_report())
            .await
            .ok()
            .unwrap();

        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        let report = doc.data.warmup_report.unwrap();
        assert_eq!(report.docker_start_ms, 850);
        assert_eq!(report.freshen_total_ms, 12_000);
        assert_eq!(report.total_ms, 13_500);
        assert!(report.workspace_present);
        assert_eq!(report.repos.len(), 1);
        assert_eq!(report.repos.first().unwrap().repo, "devbox");
        assert!(report.warm);
        // reported_at is stamped from the server clock at receive time.
        assert!(report.reported_at <= Timestamp::now());
    }

    // -----------------------------------------------------------------------
    // pool-metrics tests
    // -----------------------------------------------------------------------

    /// `warm` counts only Ready/Claimed boxes whose report says warm: a cold
    /// report and a warm-but-still-Warming box are both excluded.
    #[tokio::test]
    async fn pool_metrics_counts_states_and_warm_boxes() {
        let state = setup_state().await;
        let now = Timestamp::now();
        let warm_report = || WarmupReport::from_request(&sample_report(), now);

        let mut warm_ready = ready_devbox();
        warm_ready.warmup_report = Some(warm_report());
        insert(&state, warm_ready).await;

        let mut cold_request = sample_report();
        cold_request.warm = false;
        let mut cold_ready = ready_devbox_other();
        cold_ready.warmup_report = Some(WarmupReport::from_request(&cold_request, now));
        insert(&state, cold_ready).await;

        let mut still_warming = ready_devbox();
        still_warming.instance_id = "i-warming000000000".to_string();
        still_warming.name = "witty-yak".to_string();
        still_warming.state = DevboxState::Warming;
        still_warming.warmup_report = Some(warm_report());
        insert(&state, still_warming).await;

        let mut warm_claimed = claimed_devbox_for("jdoe");
        warm_claimed.instance_id = "i-claimed000000000".to_string();
        warm_claimed.name = "brave-fox".to_string();
        warm_claimed.warmup_report = Some(warm_report());
        insert(&state, warm_claimed).await;

        let metrics = pool_metrics(&state).await.ok().unwrap();

        assert_eq!(metrics.ready, 2);
        assert_eq!(metrics.warming, 1);
        assert_eq!(metrics.claimed, 1);
        assert_eq!(metrics.terminating, 0);
        assert_eq!(metrics.warm, 2);
    }

    /// Boxes without a report (older agent, lost report) never count as warm.
    #[tokio::test]
    async fn pool_metrics_unreported_boxes_are_not_warm() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let metrics = pool_metrics(&state).await.ok().unwrap();

        assert_eq!(metrics.ready, 1);
        assert_eq!(metrics.warm, 0);
    }

    #[tokio::test]
    async fn warmup_report_unknown_instance_is_not_found() {
        // No DevboxDoc for the calling instance (not yet adopted, or already
        // reaped) → 404, so the agent's best-effort send logs and moves on.
        let state = setup_state().await;

        let err = record_warmup_report(&state, &pool_agent(), &sample_report())
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn warmup_report_builder_role_is_forbidden() {
        // Snapshot-builder hosts authenticate on the same path but have no
        // DevboxDoc; they must be rejected up front, not fall through to a 404.
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;
        let builder = AgentIdentity {
            instance_id: InstanceId("i-1234567890abcdef0".to_string()),
            role: AgentRole::Builder,
            owner: None,
        };

        let err = record_warmup_report(&state, &builder, &sample_report())
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn warmup_report_overwrites_previous_report() {
        // A box warms once per life, so a second report is a re-run replacing
        // the first (last-writer-wins), not a merge.
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        record_warmup_report(&state, &pool_agent(), &sample_report())
            .await
            .ok()
            .unwrap();
        let mut second = sample_report();
        second.total_ms = 99_000;
        record_warmup_report(&state, &pool_agent(), &second)
            .await
            .ok()
            .unwrap();

        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        assert_eq!(doc.data.warmup_report.unwrap().total_ms, 99_000);
    }

    #[tokio::test]
    async fn warmup_report_bounds_oversized_input() {
        // The stored report is size-bounded at the trust boundary: repo entries
        // capped, error strings truncated — a misbehaving host cannot grow the
        // document row without bound.
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let mut report = sample_report();
        report.repos = (0..100)
            .map(|i| devbox_common::RepoFreshenReport {
                repo: format!("repo-{i}"),
                success: false,
                duration_ms: 1,
                error: Some("e".repeat(10_000)),
            })
            .collect();
        record_warmup_report(&state, &pool_agent(), &report)
            .await
            .ok()
            .unwrap();

        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        let stored = doc.data.warmup_report.unwrap();
        assert_eq!(stored.repos.len(), 64, "repo entries must be capped");
        let first_error = stored
            .repos
            .first()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .chars()
            .count();
        assert_eq!(first_error, 256, "error strings must be truncated");
    }

    // -----------------------------------------------------------------------
    // claim tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn claim_marks_box_claimed_and_binds_owner() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), None, None)
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

        let doc = claim_devbox(&state, &claimant("jdoe"), None, None)
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

        let doc = claim_devbox(&state, &claimant("jdoe"), None, None)
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_applies_valid_name_override() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("my-project"), None)
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

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("   "), None)
            .await
            .ok()
            .unwrap();

        assert_eq!(doc.data.name, "calm-quilt");
    }

    #[tokio::test]
    async fn claim_invalid_name_is_bad_request() {
        let state = setup_state().await;
        insert(&state, ready_devbox()).await;

        let err = claim_devbox(&state, &claimant("jdoe"), Some("Bad Name"), None)
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

        let doc = claim_devbox(&state, &claimant("jdoe"), Some("calm-quilt"), None)
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

        let err = claim_devbox(&state, &claimant("jdoe"), Some("taken"), None)
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
            claim_devbox(&s1, &p, Some("shared"), None),
            claim_devbox(&s2, &p, Some("shared"), None),
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

        let err = claim_devbox(&state, &claimant("jdoe"), None, None)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn claim_with_only_warming_boxes_reports_count() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Warming;
        insert(&state, doc).await;

        let err = claim_devbox(&state, &claimant("jdoe"), None, None)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
        let msg = err.user_message();
        assert!(msg.contains("1 warming"), "unexpected message: {msg}");
    }

    #[tokio::test]
    async fn concurrent_claims_yield_one_winner_one_conflict() {
        let state = std::sync::Arc::new(setup_state().await);
        insert(&state, ready_devbox()).await;

        let s1 = state.clone();
        let s2 = state.clone();
        let p = claimant("jdoe");
        let (r1, r2) = tokio::join!(
            claim_devbox(&s1, &p, None, None),
            claim_devbox(&s2, &p, None, None),
        );

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

        let err = release_devbox(&state, "bob", &id, false)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn release_of_unclaimed_box_is_conflict() {
        let state = setup_state().await;
        let id = insert(&state, ready_devbox()).await;

        let err = release_devbox(&state, "jdoe", &id, false)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn release_clears_owner_and_name() {
        let state = setup_state().await;
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some("jdoe".to_string());
        let id = insert(&state, doc).await;

        let (refreshed, session) = release_devbox(&state, "jdoe", &id, false)
            .await
            .ok()
            .unwrap();
        assert!(session.is_none(), "plain release creates no session");

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
    // session archive/restore tests
    // -----------------------------------------------------------------------

    /// A presigner over placeholder static credentials (SigV4 presigning is
    /// offline — no AWS access involved).
    fn test_archives() -> SessionArchives {
        let creds =
            aws_sdk_s3::config::Credentials::new("AKIDEXAMPLE", "test-secret", None, None, "test");
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(creds)
            .build();
        SessionArchives::new(
            aws_sdk_s3::Client::from_conf(config),
            "devbox-sessions-test".to_string(),
            30,
        )
    }

    async fn setup_state_with_sessions() -> AppState {
        let mut state = setup_state().await;
        state.sessions = Some(std::sync::Arc::new(test_archives()));
        state
    }

    /// Insert a Claimed box for `owner` and return its doc id.
    async fn claimed_box(state: &AppState, owner: &str) -> String {
        let mut doc = ready_devbox();
        doc.state = DevboxState::Claimed;
        doc.owner = Some(owner.to_string());
        insert(state, doc).await
    }

    /// Insert a Claimed box for `owner` on a distinct instance, so several
    /// archive flows can coexist in one store without instance-id collisions.
    async fn claimed_box_on(state: &AppState, owner: &str, instance: &str, name: &str) -> String {
        let mut doc = ready_devbox();
        doc.instance_id = instance.to_string();
        doc.name = name.to_string();
        doc.state = DevboxState::Claimed;
        doc.owner = Some(owner.to_string());
        insert(state, doc).await
    }

    fn agent_on(instance: &str) -> AgentIdentity {
        AgentIdentity {
            instance_id: InstanceId(instance.to_string()),
            role: AgentRole::Pool,
            owner: None,
        }
    }

    #[tokio::test]
    async fn release_keep_without_config_is_conflict() {
        let state = setup_state().await; // sessions: None
        let id = claimed_box(&state, "jdoe").await;

        let err = release_devbox(&state, "jdoe", &id, true)
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
        // The box is untouched — still Claimed, still owned.
        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        assert_eq!(doc.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn release_keep_creates_pending_session_and_archives_box() {
        let state = setup_state_with_sessions().await;
        let id = claimed_box(&state, "jdoe").await;

        let (doc, session) = release_devbox(&state, "jdoe", &id, true)
            .await
            .ok()
            .unwrap();
        let session = session.unwrap();

        // The box is Archiving, keeping its owner and name until the archive
        // resolves; the archive assignment names the created session.
        assert_eq!(doc.data.state, DevboxState::Archiving);
        assert_eq!(doc.data.owner.as_deref(), Some("jdoe"));
        assert_eq!(doc.data.name, "calm-quilt");
        let pending = doc.data.archive.unwrap();
        assert_eq!(pending.session_id, session.id);

        // The session record is pending, owner-scoped, named after the box.
        let stored = state
            .store
            .get::<SessionDoc>(&session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.data.state, SessionState::Pending);
        assert_eq!(stored.data.owner, "jdoe");
        assert_eq!(stored.data.name, "calm-quilt");
        assert_eq!(
            stored.data.s3_key,
            format!("sessions/{}.tar.gz", session.id)
        );
        assert!(stored.data.expires_at > stored.data.created_at);
    }

    #[tokio::test]
    async fn archive_done_completes_session_and_flips_box_to_terminating() {
        let state = setup_state_with_sessions().await;
        let id = claimed_box(&state, "jdoe").await;
        let (_, session) = release_devbox(&state, "jdoe", &id, true)
            .await
            .ok()
            .unwrap();
        let session_id = session.unwrap().id;

        // pool_agent()'s instance id matches ready_devbox()'s.
        let report = devbox_common::SessionArchiveDoneRequest {
            session_id: session_id.clone(),
            success: true,
            size_bytes: Some(4096),
            error: None,
        };
        session_archive_done(&state, &pool_agent(), &report)
            .await
            .ok()
            .unwrap();

        let stored = state
            .store
            .get::<SessionDoc>(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.data.state, SessionState::Complete);
        assert_eq!(stored.data.size_bytes, Some(4096));
        assert!(stored.data.completed_at.is_some());

        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        assert_eq!(doc.data.state, DevboxState::Terminating);
        assert!(doc.data.archive.is_none());
        assert!(doc.data.owner.is_none());
        assert!(doc.data.name.is_empty(), "name must be freed");
    }

    #[tokio::test]
    async fn archive_done_failure_fails_session_but_still_terminates() {
        let state = setup_state_with_sessions().await;
        let id = claimed_box(&state, "jdoe").await;
        let (_, session) = release_devbox(&state, "jdoe", &id, true)
            .await
            .ok()
            .unwrap();
        let session_id = session.unwrap().id;

        let report = devbox_common::SessionArchiveDoneRequest {
            session_id: session_id.clone(),
            success: false,
            size_bytes: None,
            error: Some("disk full".to_string()),
        };
        session_archive_done(&state, &pool_agent(), &report)
            .await
            .ok()
            .unwrap();

        let stored = state
            .store
            .get::<SessionDoc>(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.data.state, SessionState::Failed);
        assert_eq!(stored.data.error.as_deref(), Some("disk full"));

        let doc = state.store.get::<DevboxDoc>(&id).await.unwrap().unwrap();
        assert_eq!(doc.data.state, DevboxState::Terminating);
    }

    #[tokio::test]
    async fn archive_url_requires_matching_assignment() {
        let state = setup_state_with_sessions().await;
        let id = claimed_box(&state, "jdoe").await;
        let (_, session) = release_devbox(&state, "jdoe", &id, true)
            .await
            .ok()
            .unwrap();
        let session_id = session.unwrap().id;

        let url = session_archive_url(&state, &pool_agent(), &session_id)
            .await
            .ok()
            .unwrap();
        assert!(url.contains(&format!("sessions/{session_id}.tar.gz")));
        assert!(url.contains("X-Amz-Signature="));

        // A session id the box was not assigned is refused.
        let err = session_archive_url(&state, &pool_agent(), "some-other-session")
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn archive_url_unconfigured_is_service_unavailable() {
        let state = setup_state().await; // sessions: None
        let err = session_archive_url(&state, &pool_agent(), "whatever")
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::ServiceUnavailable(_)));
    }

    /// Drive a full archive to completion on a dedicated instance and return
    /// the session id. `tag` makes the instance id and box name unique so
    /// several sessions coexist without index collisions.
    async fn archived_session(state: &AppState, owner: &str, tag: &str) -> String {
        let instance = format!("i-archive-{tag}");
        let name = format!("box-{tag}");
        let id = claimed_box_on(state, owner, &instance, &name).await;
        let (_, session) = release_devbox(state, owner, &id, true).await.ok().unwrap();
        let session_id = session.unwrap().id;
        let report = devbox_common::SessionArchiveDoneRequest {
            session_id: session_id.clone(),
            success: true,
            size_bytes: Some(1),
            error: None,
        };
        session_archive_done(state, &agent_on(&instance), &report)
            .await
            .ok()
            .unwrap();
        session_id
    }

    #[tokio::test]
    async fn claim_resume_assigns_restore_session() {
        let state = setup_state_with_sessions().await;
        let session_id = archived_session(&state, "jdoe", "resume").await;

        // A fresh Ready box to claim onto.
        insert(&state, ready_devbox_other()).await;

        // Resolvable by the released box's name...
        let doc = claim_devbox(&state, &claimant("jdoe"), None, Some("box-resume"))
            .await
            .ok()
            .unwrap();
        assert_eq!(
            doc.data.restore_session_id.as_deref(),
            Some(session_id.as_str())
        );
        // ...and the restore tag rides the owner tag set.
        assert!(
            doc.data
                .owner_tags()
                .contains(&("devbox:session-restore", session_id.as_str()))
        );
    }

    #[tokio::test]
    async fn claim_resume_rejects_foreign_or_missing_sessions() {
        let state = setup_state_with_sessions().await;
        let session_id = archived_session(&state, "alice", "foreign").await;
        insert(&state, ready_devbox_other()).await;

        // Another owner's session is invisible: 404, and no box is consumed.
        let err = claim_devbox(&state, &claimant("jdoe"), None, Some(session_id.as_str()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::NotFound(_)));

        let err = claim_devbox(&state, &claimant("jdoe"), None, Some("no-such-session"))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn claim_resume_without_config_is_conflict() {
        let state = setup_state().await; // sessions: None
        insert(&state, ready_devbox()).await;

        let err = claim_devbox(&state, &claimant("jdoe"), None, Some("anything"))
            .await
            .err()
            .unwrap();

        assert!(matches!(err, AppError::Conflict(_)));
        // No box was consumed by the failed resume.
        let doc = state
            .store
            .find_one::<DevboxDoc>("state", "ready")
            .await
            .unwrap();
        assert!(doc.is_some(), "the ready box must remain claimable");
    }

    #[tokio::test]
    async fn claim_resume_rejects_incomplete_session() {
        let state = setup_state_with_sessions().await;
        let id = claimed_box(&state, "jdoe").await;
        let (_, session) = release_devbox(&state, "jdoe", &id, true)
            .await
            .ok()
            .unwrap();
        let session_id = session.unwrap().id;
        insert(&state, ready_devbox_other()).await;

        // Still pending — resumable only once complete.
        let err = claim_devbox(&state, &claimant("jdoe"), None, Some(session_id.as_str()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn restore_url_requires_matching_assignment_and_complete_session() {
        let state = setup_state_with_sessions().await;
        let session_id = archived_session(&state, "jdoe", "restore").await;

        let mut fresh = ready_devbox_other();
        fresh.state = DevboxState::Claimed;
        fresh.owner = Some("jdoe".to_string());
        fresh.restore_session_id = Some(session_id.clone());
        insert(&state, fresh).await;

        let other_agent = AgentIdentity {
            instance_id: InstanceId("i-0987654321fedcba0".to_string()),
            role: AgentRole::Pool,
            owner: None,
        };
        let url = session_restore_url(&state, &other_agent, &session_id)
            .await
            .ok()
            .unwrap();
        assert!(url.contains(&format!("sessions/{session_id}.tar.gz")));

        let err = session_restore_url(&state, &other_agent, "not-assigned")
            .await
            .err()
            .unwrap();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[tokio::test]
    async fn list_sessions_is_owner_scoped_and_newest_first() {
        let state = setup_state_with_sessions().await;
        let first = archived_session(&state, "jdoe", "one").await;
        let second = archived_session(&state, "jdoe", "two").await;
        archived_session(&state, "alice", "three").await;

        let sessions = list_sessions(&state, "jdoe").await.ok().unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions.first().map(|s| s.id.as_str()),
            Some(second.as_str())
        );
        assert_eq!(sessions.last().map(|s| s.id.as_str()), Some(first.as_str()));
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
        let claimed = claim_devbox(&state, &claimant("alice"), Some("old-name"), None).await;
        assert!(claimed.is_ok(), "old name must be reclaimable after rename");
    }
}
