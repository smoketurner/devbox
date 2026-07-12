//! Tests for the reconciliation module.

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod reconcile_tests {
    use std::time::Duration;

    use jiff::Timestamp;

    use crate::compute::Compute as _;
    use crate::compute::mock::MockCompute;
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::{Pool, PoolConfig};
    use crate::db::store::DocumentStore;
    use crate::documents::devbox::DevboxDoc;
    use crate::reconcile::config::ReconcilerConfig;
    use crate::reconcile::tick::{
        compute_desired_capacity, reap_unready_instances, reconciliation_tick,
    };
    use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};

    /// Build an in-memory SQLite document store with migrations applied.
    async fn setup_store() -> DocumentStore {
        let pool = Pool::connect("sqlite::memory:", &PoolConfig::default())
            .await
            .unwrap();
        if let Pool::Sqlite(ref p) = pool {
            run_sqlite_migrations(p).await.unwrap();
        }
        DocumentStore::new(pool)
    }

    /// Build a `ReconcilerConfig` suitable for tests.
    ///
    /// Uses `ready_timeout = 60s` (the minimum). Tests that trigger the reaper
    /// seed docs with `created_at` far in the past (Unix epoch) to exceed it.
    fn test_config() -> ReconcilerConfig {
        ReconcilerConfig {
            pool_id: "test".to_string(),
            server_id: "test-server".to_string(),
            polling_interval: Duration::from_secs(30),
            lock_ttl: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(60),
        }
    }

    /// Find the first doc in the given state, or panic with a descriptive message.
    async fn find_doc_by_state(
        store: &DocumentStore,
        state: DevboxState,
    ) -> crate::db::document_type::Document<DevboxDoc> {
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        all.into_iter()
            .find(|d| d.data.state == state)
            .unwrap_or_else(|| panic!("no doc in state {state:?}"))
    }

    // =========================================================================
    // Lifecycle filter and Warming/Ready transition tests
    // =========================================================================

    /// Test: a new InService instance without the ready tag stays Warming.
    #[tokio::test]
    async fn test_warming_instance_without_ready_tag_stays_warming() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        // Provision an ASG with one InService instance (no ready tag).
        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Doc should exist in Warming state.
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(all.len(), 1, "expected one doc");
        assert_eq!(
            all.first().unwrap().data.state,
            DevboxState::Warming,
            "instance without devbox:ready tag must stay Warming (instance_id={instance_id})"
        );
        assert!(
            all.first().unwrap().data.ready_at.is_none(),
            "ready_at must stay unset while the box is Warming"
        );
    }

    /// Test: a Pending instance (no hook variant) also gets a Warming doc.
    ///
    /// Regression guard: if the lifecycle filter is tightened back to the exact
    /// string `"Pending:Wait"`, instances in plain `"Pending"` state would be
    /// silently skipped and never adopted into the pool.
    #[tokio::test]
    async fn test_warming_doc_created_for_pending_instance() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        // No lifecycle hook → instances transition Pending → InService. The
        // reconciler may first observe the instance in "Pending" before InService.
        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("Pending");

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(all.len(), 1, "expected one doc for Pending instance");
        let doc = all.first().unwrap();
        assert_eq!(
            doc.data.state,
            DevboxState::Warming,
            "Pending instance must produce a Warming doc (instance_id={instance_id})"
        );
        assert_eq!(
            doc.data.instance_id.as_str(),
            instance_id.as_str(),
            "doc must reference the correct instance"
        );
    }

    /// Test: every doc the reconciler creates gets a unique, friendly name.
    #[tokio::test]
    async fn test_created_docs_get_unique_names() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(3, 5, 3);
        let i1 = compute.add_instance("InService");
        compute.add_instance("InService");
        compute.add_instance("InService");

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(all.len(), 3, "expected one doc per instance");
        let names: std::collections::HashSet<&str> =
            all.iter().map(|d| d.data.name.as_str()).collect();
        assert_eq!(names.len(), 3, "names must be unique across boxes");
        for d in &all {
            assert!(!d.data.name.is_empty(), "every new box must be named");
            assert_ne!(
                d.data.name, d.data.instance_id,
                "a new box gets a generated friendly name, not its instance id"
            );
        }
        // Sanity: the generated names are friendly adjective-noun handles.
        let first = all
            .iter()
            .find(|d| d.data.instance_id == i1)
            .expect("doc for first instance");
        assert!(first.data.name.contains('-'), "got: {}", first.data.name);
    }

    /// Test: after setting the ready tag, the next tick flips the doc to Ready.
    #[tokio::test]
    async fn test_warming_instance_with_ready_tag_becomes_ready() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        // Provision an ASG with one InService instance.
        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        // First tick: doc created in Warming.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(all.first().unwrap().data.state, DevboxState::Warming);

        // Set the devbox:ready tag (simulates devbox-agent warmup completing).
        compute.set_instance_ready(&instance_id, true);

        // Second tick: Warming → Ready.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(
            all.first().unwrap().data.state,
            DevboxState::Ready,
            "instance with devbox:ready=true must transition to Ready"
        );
        assert!(
            all.first().unwrap().data.ready_at.is_some(),
            "the Warming → Ready flip must stamp ready_at"
        );
    }

    // =========================================================================
    // Reaper tests
    // =========================================================================

    /// Test: a Warming doc whose created_at exceeds ready_timeout and is not ready
    /// is set to Terminating, and the instance is terminated on the following tick.
    ///
    /// Reaper ordering (R1 fix): the reaper flips the doc to Terminating FIRST
    /// (no AWS call). `handle_terminating_instances` (step 6) then terminates the
    /// instance on the NEXT tick when it reads the doc as Terminating from the DB.
    /// The `all_docs` snapshot used by step 6 in the same tick as the reap was
    /// read BEFORE the reap ran, so step 6 still sees the doc as Warming that tick.
    #[tokio::test]
    async fn test_reap_unready_instance_after_timeout() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config(); // ready_timeout = 60s

        // Provision an ASG with one InService instance (no ready tag).
        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        // Tick 1: creates the Warming doc.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Reach into the store and back-date doc.data.created_at to Unix epoch.
        // The reaper compares doc.data.created_at (the JSON field) against the
        // current time; seeding it far in the past triggers the reap.
        let doc = find_doc_by_state(&store, DevboxState::Warming).await;
        let doc_id = doc.id.clone();
        let past = Timestamp::from_second(0).unwrap(); // 1970-01-01 — well past 60s timeout
        let mut aged_data = doc.data.clone();
        aged_data.created_at = past;
        store.update(&doc_id, &aged_data).await.unwrap();

        // Tick 2: reaper flips doc to Terminating (no AWS call yet).
        // step 6 uses the pre-reap all_docs snapshot (Warming) → skips terminate.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // After tick 2: doc is Terminating; instance is still in ASG (not yet
        // terminated — the AWS call happens on tick 3).
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        let terminating_doc = all.first().expect("doc should still exist after reap tick");
        assert_eq!(
            terminating_doc.data.state,
            DevboxState::Terminating,
            "reaped doc must be set to Terminating after tick 2"
        );
        // Assert it is the SAME doc (not a new one recreated with a reset timer).
        assert_eq!(
            terminating_doc.id, doc_id,
            "surviving doc must be the same doc that was reaped, not a fresh one"
        );
        // Instance is still in ASG after tick 2 (AWS terminate not yet called).
        assert!(
            compute.get_instance_tags(&instance_id).is_some(),
            "instance should still be in ASG after tick 2 (AWS terminate deferred to tick 3)"
        );

        // Tick 3: handle_terminating_instances sees the Terminating doc and calls
        // terminate_instance_in_asg. The mock removes the instance; stale-cleanup
        // then deletes the doc.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Instance is now gone from the mock ASG.
        assert!(
            compute.get_instance_tags(&instance_id).is_none(),
            "instance should be gone from mock ASG after tick 3 terminate"
        );
        // Doc is deleted by step 6 directly: the mock removes the instance on
        // terminate, so the doc is cleaned up in the same tick (step 6 succeeds
        // and deletes the doc; stale-cleanup would also catch it, but step 6 wins).
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert!(
            all.is_empty(),
            "doc should be deleted after instance leaves ASG"
        );
    }

    /// Test: Ready and Claimed docs with ancient created_at are NOT reaped.
    ///
    /// Regression guard: if the `state != Warming` guard in `reap_unready_instances`
    /// were removed, a claimed box could be terminated mid-session.
    ///
    /// The docs are seeded directly in the store with past `created_at` so the
    /// reaper's timeout check would fire if the state guard is missing.
    #[tokio::test]
    async fn test_reaper_does_not_touch_ready_or_claimed_docs() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config(); // ready_timeout = 60s

        compute.seed_asg(2, 5, 2);
        let ready_id = compute.add_instance("InService");
        let claimed_id = compute.add_instance("InService");

        // Mark both instances ready so the reconciler's InstanceInfo.ready is true.
        compute.set_instance_ready(&ready_id, true);
        compute.set_instance_ready(&claimed_id, true);

        // Seed the docs directly as Ready/Claimed with created_at at Unix epoch,
        // so the reaper would trigger on them if the state guard were missing.
        let past = Timestamp::from_second(0).unwrap();

        let ready_doc = DevboxDoc {
            instance_id: ready_id.clone(),
            name: "ready-box".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: None,
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: past,
            owner_tag_applied: false,
            warmup_report: None,
        };
        let claimed_doc = DevboxDoc {
            instance_id: claimed_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("alice".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: past,
            owner_tag_applied: false,
            warmup_report: None,
        };

        store.insert(&ready_doc).await.unwrap();
        store.insert(&claimed_doc).await.unwrap();

        // Run a tick: reaper must not terminate either instance.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Both instances should still be in the ASG.
        assert!(
            compute.get_instance_tags(&ready_id).is_some(),
            "Ready instance must not be reaped (ready_id={ready_id})"
        );
        assert!(
            compute.get_instance_tags(&claimed_id).is_some(),
            "Claimed instance must not be reaped (claimed_id={claimed_id})"
        );
    }

    /// Test: a `describe_instances` failure does not abort the whole tick.
    ///
    /// Regression guard (bugbot: "describe failure skips reconciliation"). A
    /// describe failure must skip only the tag-dependent steps. Owner-tagging still
    /// runs — a just-claimed box must get its `devbox:owner` tag even during a
    /// transient EC2 describe brownout — and the reaper must NOT run, since empty
    /// tag data would treat every box as unready and reap the warm pool.
    #[tokio::test]
    async fn test_describe_failure_does_not_block_owner_tags_or_reap() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config(); // ready_timeout = 60s

        compute.seed_asg(2, 5, 2);
        let claimed_id = compute.add_instance("InService");
        let warming_id = compute.add_instance("InService");

        // A claimed box awaiting its owner tag (describe-independent step 9).
        let claimed_doc = DevboxDoc {
            instance_id: claimed_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("alice".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };
        // A timed-out, unready Warming box the reaper would normally reap.
        let past = Timestamp::from_second(0).unwrap();
        let warming_doc = DevboxDoc {
            instance_id: warming_id.clone(),
            name: "warming-box".to_string(),
            state: DevboxState::Warming,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: None,
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: past,
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();
        store.insert(&warming_doc).await.unwrap();

        // Inject a one-shot describe_instances failure (consumed by step 2).
        compute.set_error("describe_instances", "throttled".to_string());

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Owner tag WAS applied despite the describe failure.
        let tags = compute
            .get_instance_tags(&claimed_id)
            .expect("claimed instance still in ASG");
        assert_eq!(
            tags.get("devbox:owner").map(String::as_str),
            Some("alice"),
            "owner tag must be applied even when describe_instances fails"
        );

        // The timed-out Warming doc was NOT reaped (reaper skipped on describe fail).
        let warming = find_doc_by_state(&store, DevboxState::Warming).await;
        assert_eq!(
            warming.data.instance_id.as_str(),
            warming_id.as_str(),
            "Warming doc must survive: reaper must not run without fresh tag data"
        );
    }

    /// Test: a claimed box's `owner_email` is published as the `devbox:owner-email`
    /// tag alongside `devbox:owner`, so `owner-sync` can set the git identity.
    #[tokio::test]
    async fn test_owner_email_tag_applied() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let claimed_id = compute.add_instance("InService");

        let claimed_doc = DevboxDoc {
            instance_id: claimed_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("alice".to_string()),
            owner_email: Some("alice@example.com".to_string()),
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let tags = compute
            .get_instance_tags(&claimed_id)
            .expect("claimed instance still in ASG");
        assert_eq!(tags.get("devbox:owner").map(String::as_str), Some("alice"));
        assert_eq!(
            tags.get("devbox:owner-email").map(String::as_str),
            Some("alice@example.com")
        );
    }

    /// Test: the reaper re-checks readiness with a fresh describe before terminating.
    ///
    /// Regression guard (bugbot: "stale tags cause false reap"). If `warmup` sets
    /// `devbox:ready=true` after the tick-start describe but before the reaper runs,
    /// the box must not be reaped. Simulated by passing a stale (empty) `info_by_id`
    /// while the mock instance actually carries the ready tag, so the reaper's fresh
    /// re-describe is what saves it.
    #[tokio::test]
    async fn test_reap_rechecks_readiness_before_terminating() {
        use std::collections::HashMap;

        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config(); // ready_timeout = 60s

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");
        // The box reported ready AFTER the (simulated) tick-start snapshot.
        compute.set_instance_ready(&instance_id, true);

        // A timed-out Warming doc that a stale snapshot would reap.
        let past = Timestamp::from_second(0).unwrap();
        let warming_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "warming-box".to_string(),
            state: DevboxState::Warming,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: None,
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: past,
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&warming_doc).await.unwrap();

        let all_docs = store.list_all::<DevboxDoc>().await.unwrap();
        // Stale snapshot: empty map → the reaper sees the instance as not-ready.
        let info_by_id = HashMap::new();

        reap_unready_instances(&store, &compute, &config, &all_docs, &info_by_id).await;

        // The fresh re-describe saw devbox:ready=true → reap skipped, doc unchanged.
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(
            all.first().unwrap().data.state,
            DevboxState::Warming,
            "box that reported ready since the snapshot must not be reaped"
        );
        assert!(
            compute.get_instance_tags(&instance_id).is_some(),
            "instance must not be terminated"
        );
    }

    // =========================================================================
    // Owner-tag re-assert tests (Step 9 defense-in-depth)
    // =========================================================================

    /// Test (a): Self-heal — a tampered `devbox:owner` tag is corrected within
    /// one reconcile tick.
    ///
    /// Simulates an IAM regression or manual edit that changed the instance's
    /// `devbox:owner` and `devbox:owner-email` tags to wrong values. After one
    /// tick, both tags must equal the doc's `owner` / `owner_email` fields.
    #[tokio::test]
    async fn test_owner_tag_self_heals_on_divergence() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        // Seed a Claimed doc with owner_tag_applied = true (first apply done).
        let claimed_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("alice".to_string()),
            owner_email: Some("alice@example.com".to_string()),
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: true,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();

        // Pre-seed WRONG owner tags on the mock instance (simulates tampering).
        compute
            .tag_instance(
                &instance_id,
                &[
                    ("devbox:owner", "tampered-user"),
                    ("devbox:owner-email", "bad@actor.com"),
                ],
            )
            .await
            .unwrap();

        // One tick must overwrite the tampered tags with the doc's values.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let tags = compute
            .get_instance_tags(&instance_id)
            .expect("instance still in ASG");
        assert_eq!(
            tags.get("devbox:owner").map(String::as_str),
            Some("alice"),
            "devbox:owner must be corrected to the doc's owner after one tick"
        );
        assert_eq!(
            tags.get("devbox:owner-email").map(String::as_str),
            Some("alice@example.com"),
            "devbox:owner-email must be corrected to the doc's owner_email after one tick"
        );
    }

    /// Test (b): First-apply — a fresh Claimed doc gets its owner tag applied and
    /// `owner_tag_applied` flipped to true within one tick.
    #[tokio::test]
    async fn test_owner_tag_first_apply_sets_flag() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        // A freshly claimed doc — owner_tag_applied is false.
        let claimed_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("bob".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Tag must be applied on the instance.
        let tags = compute
            .get_instance_tags(&instance_id)
            .expect("instance still in ASG");
        assert_eq!(
            tags.get("devbox:owner").map(String::as_str),
            Some("bob"),
            "devbox:owner must be applied on the first tick"
        );

        // The doc must have owner_tag_applied flipped to true.
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        let doc = all
            .iter()
            .find(|d| d.data.instance_id == instance_id)
            .expect("doc must still exist after tick");
        assert!(
            doc.data.owner_tag_applied,
            "owner_tag_applied must be true after first tag application"
        );
    }

    /// Test (c): Steady-state idempotency — when `owner_tag_applied` is already
    /// true, no `compare_and_update` is issued, so the doc version does not advance.
    ///
    /// The idempotent tag re-write still runs (expected and intentional); only the
    /// DB write is suppressed to avoid per-tick churn.
    #[tokio::test]
    async fn test_owner_tag_no_db_update_when_already_applied() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        // Claimed doc already past first-apply.
        let claimed_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("carol".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: true,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();

        // Capture the doc's version before the tick.
        let all_before = store.list_all::<DevboxDoc>().await.unwrap();
        let before = all_before
            .iter()
            .find(|d| d.data.instance_id == instance_id)
            .expect("doc must exist before tick");
        let version_before = before.version;

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // Doc version must not have advanced — no compare_and_update was issued.
        let all_after = store.list_all::<DevboxDoc>().await.unwrap();
        let after = all_after
            .iter()
            .find(|d| d.data.instance_id == instance_id)
            .expect("doc must still exist after tick");
        assert_eq!(
            after.version, version_before,
            "doc version must not advance when owner_tag_applied is already true"
        );
    }

    /// MUST-FIX 2: Non-Claimed docs (Ready, Warming) must not receive owner tags.
    ///
    /// Guards the `if doc.data.state != DevboxState::Claimed { continue; }` gate.
    /// A bug removing that guard would silently tag Ready/Warming instances with
    /// `devbox:owner`, which `AuthorizedPrincipalsCommand` trusts — a security
    /// regression. Verified by seeding a Ready doc with `owner` set and asserting
    /// no `devbox:owner` tag appears on the instance after a tick.
    #[tokio::test]
    async fn test_non_claimed_doc_is_not_re_tagged() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");
        // Mark ready so the reconciler's tag-dependent steps see it correctly.
        compute.set_instance_ready(&instance_id, true);

        // A Ready doc that still carries an owner field (edge case from a prior
        // state transition), but is NOT in Claimed state.
        let ready_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "ready-box".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("sneaky".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&ready_doc).await.unwrap();

        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        let tags = compute
            .get_instance_tags(&instance_id)
            .expect("instance still in ASG");
        assert_eq!(
            tags.get("devbox:owner"),
            None,
            "devbox:owner must not be set on a non-Claimed instance"
        );
    }

    /// MUST-FIX 3: A `tag_instance` error leaves `owner_tag_applied` false.
    ///
    /// Guards the error path at `tick.rs` where `Err(e)` from `tag_instance`
    /// causes a `continue` without flipping the flag. A future regression that
    /// accidentally sets `owner_tag_applied=true` on failure would suppress all
    /// future re-assert attempts for that box, defeating the defense-in-depth.
    #[tokio::test]
    async fn test_tag_instance_error_leaves_flag_false() {
        let store = setup_store().await;
        let compute = MockCompute::new();
        let config = test_config();

        compute.seed_asg(1, 5, 1);
        let instance_id = compute.add_instance("InService");

        let claimed_doc = DevboxDoc {
            instance_id: instance_id.clone(),
            name: "claimed-box".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m7g.large".to_string()),
            ami_id: AmiId("ami-mock".to_string()),
            subnet_id: SubnetId("subnet-mock".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: None,
            owner: Some("dave".to_string()),
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        };
        store.insert(&claimed_doc).await.unwrap();

        // Inject a one-shot tag_instance failure.
        compute.set_error("tag_instance", "injected error".to_string());

        // The tick must continue to completion (not abort) despite the error.
        reconciliation_tick(&store, &compute, &config)
            .await
            .unwrap();

        // owner_tag_applied must still be false — the error path must not flip it.
        let all = store.list_all::<DevboxDoc>().await.unwrap();
        let doc = all
            .iter()
            .find(|d| d.data.instance_id == instance_id)
            .expect("doc must still exist after failed tick");
        assert!(
            !doc.data.owner_tag_applied,
            "owner_tag_applied must remain false when tag_instance fails"
        );
    }

    #[test]
    fn desired_capacity_adds_warm_pool_when_below_max() {
        // claimed + warm fits under max: warm spares stack on top of claims.
        assert_eq!(compute_desired_capacity(3, 2, 10), 5);
        // Zero claims yields exactly the warm-pool (ASG min_size).
        assert_eq!(compute_desired_capacity(0, 2, 10), 2);
    }

    #[test]
    fn desired_capacity_clamps_to_max() {
        // claimed + warm exceeds max (pool saturating): clamp to the ASG ceiling.
        assert_eq!(compute_desired_capacity(9, 2, 10), 10);
        assert_eq!(compute_desired_capacity(10, 2, 10), 10);
    }

    #[test]
    fn desired_capacity_saturates_without_overflow() {
        // saturating_add must not panic; the .min() still clamps to max.
        assert_eq!(compute_desired_capacity(u32::MAX, 2, 10), 10);
    }
}
