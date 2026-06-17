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
    use devbox_common::DevboxState;
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
            instance_id: Some("i-1234567890abcdef0".to_string()),
            state: DevboxState::Ready,
            instance_type: "m5.large".to_string(),
            ami_id: "ami-12345678".to_string(),
            subnet_id: "subnet-12345678".to_string(),
            ebs_volume_id: Some("vol-12345678".to_string()),
            owner: None,
            claimed_at: None,
            created_at: Timestamp::now(),
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
        assert_eq!(fetched.data.instance_type, "m5.large");
    }

    #[tokio::test]
    async fn test_insert_with_id() {
        let store = setup_store().await;
        let doc = sample_devbox();
        let custom_id = "custom-test-id-123";

        let inserted = store.insert_with_id(custom_id, &doc).await.unwrap();
        assert_eq!(inserted.id, custom_id);

        let fetched = store.get::<DevboxDoc>(custom_id).await.unwrap().unwrap();
        assert_eq!(fetched.data.ami_id, "ami-12345678");
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
        doc2.instance_id = Some("i-different".to_string());

        store.insert(&doc1).await.unwrap();
        store.insert(&doc2).await.unwrap();

        let found = store
            .find_all::<DevboxDoc>("state", "ready")
            .await
            .unwrap();
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
        doc2.instance_id = Some("i-second".to_string());

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
}
