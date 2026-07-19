//! Integration tests for the document store using in-memory SQLite.

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod store_tests {
    use crate::db::migrations::run_sqlite_migrations;
    use crate::db::pool::{Pool, PoolConfig};
    use crate::db::store::DocumentStore;
    use crate::documents::devbox::DevboxDoc;
    use crate::documents::name_claim::{NameClaimDoc, claim_doc_id, sync_name_claim};
    use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};
    use jiff::Timestamp;

    async fn setup_store() -> DocumentStore {
        let pool = Pool::connect("sqlite::memory:", &PoolConfig::default())
            .await
            .unwrap();
        if let Pool::Sqlite(ref p) = pool {
            run_sqlite_migrations(p).await.unwrap();
        }
        DocumentStore::new(pool)
    }

    fn sample_devbox() -> DevboxDoc {
        DevboxDoc {
            instance_id: "i-1234567890abcdef0".to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Ready,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-12345678".to_string()),
            subnet_id: SubnetId("subnet-12345678".to_string()),
            region: "us-east-1".to_string(),
            ebs_volume_id: Some("vol-12345678".to_string()),
            owner: None,
            owner_email: None,
            claimed_at: None,
            ready_at: None,
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        }
    }

    #[tokio::test]
    async fn test_insert_and_get() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();
        assert_eq!(inserted.data.state, DevboxState::Ready);
        assert_eq!(inserted.version, 1);

        let fetched = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, inserted.id);
        assert_eq!(
            fetched.data.instance_type,
            InstanceType("m5.large".to_string())
        );
    }

    #[tokio::test]
    async fn test_insert_with_id() {
        let store = setup_store().await;
        let doc = sample_devbox();
        let custom_id = "custom-test-id-123";

        let inserted = store.insert_with_id(custom_id, &doc).await.unwrap();
        assert_eq!(inserted.id, custom_id);

        let fetched = store.get::<DevboxDoc>(custom_id).await.unwrap().unwrap();
        assert_eq!(fetched.data.ami_id, AmiId("ami-12345678".to_string()));
    }

    #[tokio::test]
    async fn test_find_one_by_index() {
        let store = setup_store().await;
        let doc = sample_devbox();

        store.insert(&doc).await.unwrap();

        let found = store
            .find_one::<DevboxDoc>("state", "ready")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.data.state, DevboxState::Ready);
    }

    #[tokio::test]
    async fn test_find_all_by_index() {
        let store = setup_store().await;

        // Insert two ready devboxes
        let doc1 = sample_devbox();
        let mut doc2 = sample_devbox();
        doc2.instance_id = "i-different".to_string();
        doc2.name = "brave-otter".to_string();

        store.insert(&doc1).await.unwrap();
        store.insert(&doc2).await.unwrap();

        let found = store.find_all::<DevboxDoc>("state", "ready").await.unwrap();
        assert_eq!(found.len(), 2);
    }

    #[tokio::test]
    async fn test_update() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();

        let mut updated_doc = inserted.data.clone();
        updated_doc.state = DevboxState::Claimed;
        updated_doc.owner = Some("user@example.com".to_string());

        store.update(&inserted.id, &updated_doc).await.unwrap();

        let fetched = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert_eq!(fetched.data.state, DevboxState::Claimed);
        assert_eq!(fetched.data.owner, Some("user@example.com".to_string()));
        assert_eq!(fetched.version, 2);
    }

    #[tokio::test]
    async fn test_compare_and_update_success() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();

        let mut updated_doc = inserted.data.clone();
        updated_doc.state = DevboxState::Claimed;

        let success = store
            .compare_and_update(&inserted.id, 1, &updated_doc)
            .await
            .unwrap();
        assert!(success);

        let fetched = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert_eq!(fetched.version, 2);
        assert_eq!(fetched.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn test_compare_and_update_version_mismatch() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();

        let mut updated_doc = inserted.data.clone();
        updated_doc.state = DevboxState::Claimed;

        // Use wrong version
        let success = store
            .compare_and_update(&inserted.id, 99, &updated_doc)
            .await
            .unwrap();
        assert!(!success);

        // Document should be unchanged
        let fetched = store.get::<DevboxDoc>(&inserted.id).await.unwrap().unwrap();
        assert_eq!(fetched.version, 1);
        assert_eq!(fetched.data.state, DevboxState::Ready);
    }

    /// Insert a devbox doc and acquire its name claim in one transaction —
    /// the same composition `reconcile/tick.rs` uses at doc creation.
    async fn insert_with_claim(
        store: &DocumentStore,
        id: &str,
        doc: &DevboxDoc,
    ) -> anyhow::Result<()> {
        crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            tx.insert_with_id(id, doc).await?;
            sync_name_claim(&mut tx, id, "", &doc.name).await?;
            tx.commit().await?;
            Ok(())
        })
    }

    /// Version-guarded update plus name-claim swap in one transaction — the
    /// same composition the claim/rename/release service paths use.
    async fn update_with_claim(
        store: &DocumentStore,
        id: &str,
        version: i32,
        doc: &DevboxDoc,
        old_name: &str,
    ) -> anyhow::Result<bool> {
        crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            if !tx.compare_and_update(id, version, doc).await? {
                return Ok(false);
            }
            sync_name_claim(&mut tx, id, old_name, &doc.name).await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    #[tokio::test]
    async fn test_name_claim_outcomes() {
        let store = setup_store().await;

        // Box A claims "taken"; box B claims "free".
        let mut a = sample_devbox();
        a.name = "taken".to_string();
        insert_with_claim(&store, "doc-a", &a).await.unwrap();
        let mut b = sample_devbox();
        b.instance_id = "i-second".to_string();
        b.name = "free".to_string();
        insert_with_claim(&store, "doc-b", &b).await.unwrap();
        assert_eq!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("taken"))
                .await
                .unwrap()
                .unwrap()
                .data
                .devbox_id,
            "doc-a"
        );

        // Renaming B to A's name fails on the claim doc's primary key, and
        // the whole transaction rolls back — B is untouched.
        let b_doc = store.get::<DevboxDoc>("doc-b").await.unwrap().unwrap();
        let mut want_taken = b_doc.data.clone();
        want_taken.name = "taken".to_string();
        let err = update_with_claim(&store, "doc-b", b_doc.version, &want_taken, "free")
            .await
            .unwrap_err();
        assert!(crate::db::pool::is_unique_violation(&err));
        let fetched = store.get::<DevboxDoc>("doc-b").await.unwrap().unwrap();
        assert_eq!(fetched.data.name, "free", "B must be untouched");
        assert_eq!(fetched.version, b_doc.version);
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("free"))
                .await
                .unwrap()
                .is_some(),
            "B's claim must survive the rollback"
        );

        // A stale version is a mismatch and writes no claim.
        let mut want_fresh = b_doc.data.clone();
        want_fresh.name = "fresh".to_string();
        let updated = update_with_claim(&store, "doc-b", 99, &want_fresh, "free")
            .await
            .unwrap();
        assert!(!updated);
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("fresh"))
                .await
                .unwrap()
                .is_none()
        );

        // A free name with the correct version succeeds, swapping the claim.
        let updated = update_with_claim(&store, "doc-b", b_doc.version, &want_fresh, "free")
            .await
            .unwrap();
        assert!(updated);
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("free"))
                .await
                .unwrap()
                .is_none(),
            "old claim must be released"
        );
        assert_eq!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("fresh"))
                .await
                .unwrap()
                .unwrap()
                .data
                .devbox_id,
            "doc-b"
        );

        // The released name is reusable: A can rename onto "free" now.
        let a_doc = store.get::<DevboxDoc>("doc-a").await.unwrap().unwrap();
        let mut a_wants_free = a_doc.data.clone();
        a_wants_free.name = "free".to_string();
        let updated = update_with_claim(&store, "doc-a", a_doc.version, &a_wants_free, "taken")
            .await
            .unwrap();
        assert!(updated);
    }

    #[tokio::test]
    async fn test_unchanged_name_backfills_missing_claim() {
        let store = setup_store().await;

        // A legacy doc: named, but inserted without a claim (predates claims).
        let mut legacy = sample_devbox();
        legacy.name = "calm-quilt".to_string();
        let legacy = store.insert(&legacy).await.unwrap();
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("calm-quilt"))
                .await
                .unwrap()
                .is_none()
        );

        // A name-preserving write (e.g. a plain claim) acquires the claim.
        let mut claimed = legacy.data.clone();
        claimed.state = DevboxState::Claimed;
        let updated = update_with_claim(&store, &legacy.id, legacy.version, &claimed, "calm-quilt")
            .await
            .unwrap();
        assert!(updated);
        assert_eq!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("calm-quilt"))
                .await
                .unwrap()
                .unwrap()
                .data
                .devbox_id,
            legacy.id
        );

        // A claim already held by another box is left untouched (transitional
        // legacy duplicate), not an error.
        let mut dup = sample_devbox();
        dup.instance_id = "i-second".to_string();
        dup.name = "calm-quilt".to_string();
        let dup = store.insert(&dup).await.unwrap();
        let mut dup_claimed = dup.data.clone();
        dup_claimed.state = DevboxState::Claimed;
        let updated = update_with_claim(&store, &dup.id, dup.version, &dup_claimed, "calm-quilt")
            .await
            .unwrap();
        assert!(updated);
        assert_eq!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("calm-quilt"))
                .await
                .unwrap()
                .unwrap()
                .data
                .devbox_id,
            legacy.id,
            "existing claim must keep its holder"
        );
    }

    #[tokio::test]
    async fn test_name_claim_released_on_clear_and_delete() {
        let store = setup_store().await;

        let mut a = sample_devbox();
        a.name = "calm-quilt".to_string();
        insert_with_claim(&store, "doc-a", &a).await.unwrap();

        // Clearing the name (release-shaped update) deletes the claim.
        let a_doc = store.get::<DevboxDoc>("doc-a").await.unwrap().unwrap();
        let mut cleared = a_doc.data.clone();
        cleared.name = String::new();
        let updated = update_with_claim(&store, "doc-a", a_doc.version, &cleared, "calm-quilt")
            .await
            .unwrap();
        assert!(updated);
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("calm-quilt"))
                .await
                .unwrap()
                .is_none()
        );

        // A named doc deleted with its claim frees the name for reuse.
        let mut b = sample_devbox();
        b.instance_id = "i-second".to_string();
        b.name = "calm-quilt".to_string();
        insert_with_claim(&store, "doc-b", &b).await.unwrap();
        crate::with_dsql_retry!(async {
            let mut tx = store.begin().await?;
            tx.delete("doc-b").await?;
            sync_name_claim(&mut tx, "doc-b", "calm-quilt", "").await?;
            tx.commit().await?;
            Ok(())
        })
        .unwrap();
        assert!(store.get::<DevboxDoc>("doc-b").await.unwrap().is_none());
        assert!(
            store
                .get::<NameClaimDoc>(&claim_doc_id("calm-quilt"))
                .await
                .unwrap()
                .is_none()
        );
        let mut c = sample_devbox();
        c.instance_id = "i-third".to_string();
        c.name = "calm-quilt".to_string();
        insert_with_claim(&store, "doc-c", &c).await.unwrap();
    }

    #[tokio::test]
    async fn test_delete() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();

        let deleted = store.delete(&inserted.id).await.unwrap();
        assert!(deleted);

        let fetched = store.get::<DevboxDoc>(&inserted.id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let store = setup_store().await;

        let deleted = store.delete("nonexistent-id").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_list_all() {
        let store = setup_store().await;

        let doc1 = sample_devbox();
        let mut doc2 = sample_devbox();
        doc2.instance_id = "i-second".to_string();
        doc2.name = "brave-otter".to_string();

        store.insert(&doc1).await.unwrap();
        store.insert(&doc2).await.unwrap();

        let all = store.list_all::<DevboxDoc>().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_count() {
        let store = setup_store().await;

        let count = store.count::<DevboxDoc>().await.unwrap();
        assert_eq!(count, 0);

        let doc = sample_devbox();
        store.insert(&doc).await.unwrap();

        let count = store.count::<DevboxDoc>().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_index_updates_on_state_change() {
        let store = setup_store().await;
        let doc = sample_devbox();

        let inserted = store.insert(&doc).await.unwrap();

        // Initially findable by "ready" state
        let found = store.find_one::<DevboxDoc>("state", "ready").await.unwrap();
        assert!(found.is_some());

        // Update state to claimed
        let mut updated = inserted.data.clone();
        updated.state = DevboxState::Claimed;
        updated.owner = Some("user@test.com".to_string());
        store.update(&inserted.id, &updated).await.unwrap();

        // No longer findable by "ready" state
        let found = store.find_one::<DevboxDoc>("state", "ready").await.unwrap();
        assert!(found.is_none());

        // Findable by "claimed" state
        let found = store
            .find_one::<DevboxDoc>("state", "claimed")
            .await
            .unwrap();
        assert!(found.is_some());

        // Findable by owner
        let found = store
            .find_one::<DevboxDoc>("owner", "user@test.com")
            .await
            .unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn test_pool_health_check() {
        let pool = Pool::connect("sqlite::memory:", &PoolConfig::default())
            .await
            .unwrap();
        pool.is_healthy().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_keeping_own_name_is_not_a_conflict() {
        // A doc rewriting its own index rows (delete + reinsert in one
        // transaction) must not collide with itself on the name-derived key.
        let store = setup_store().await;
        let doc = sample_devbox();
        let inserted = store.insert(&doc).await.unwrap();

        let mut updated = inserted.data.clone();
        updated.state = DevboxState::Claimed;
        store.update(&inserted.id, &updated).await.unwrap();

        let found = store
            .find_one::<DevboxDoc>("name", "calm-quilt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.data.state, DevboxState::Claimed);
    }

    #[tokio::test]
    async fn test_duplicate_id_insert_is_unique_violation() {
        use crate::documents::leader_lock::LeaderLockDoc;

        let store = setup_store().await;
        let doc = LeaderLockDoc {
            holder_id: "server-a".to_string(),
            expires_at: Timestamp::now(),
        };

        store.insert_with_id("dup-lock-id", &doc).await.unwrap();

        let err = store.insert_with_id("dup-lock-id", &doc).await.unwrap_err();
        assert!(crate::db::pool::is_unique_violation(&err));
    }
}
