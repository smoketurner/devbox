//! Document store backed by 2 tables.
//!
//! [`DocumentStore`] provides typed CRUD operations over the `documents` and
//! `document_indexes` tables. Data is stored as plain JSON (no encryption).
//! Index values are stored in plaintext.

use anyhow::{Context, Result};
use jiff::Timestamp;
use sea_query::{Alias, Expr, ExprTrait, Iden, Order, Query};

use super::document_type::{Document, DocumentType, IndexEntry};
use super::pool::Pool;

// ============================================================================
// Schema Iden Enums
// ============================================================================

#[derive(Iden)]
enum Documents {
    Table,
    Id,
    DocType,
    SchemaVersion,
    Data,
    ExpiresAt,
    CreatedAt,
    UpdatedAt,
    Version,
    LastUsedAt,
}

/// All document columns for SELECT statements.
const DOC_COLUMNS: [Documents; 9] = [
    Documents::Id,
    Documents::DocType,
    Documents::SchemaVersion,
    Documents::Data,
    Documents::ExpiresAt,
    Documents::CreatedAt,
    Documents::UpdatedAt,
    Documents::Version,
    Documents::LastUsedAt,
];

/// All document columns qualified with the table name, for joins.
const DOC_TABLE_COLUMNS: [(Documents, Documents); 9] = [
    (Documents::Table, Documents::Id),
    (Documents::Table, Documents::DocType),
    (Documents::Table, Documents::SchemaVersion),
    (Documents::Table, Documents::Data),
    (Documents::Table, Documents::ExpiresAt),
    (Documents::Table, Documents::CreatedAt),
    (Documents::Table, Documents::UpdatedAt),
    (Documents::Table, Documents::Version),
    (Documents::Table, Documents::LastUsedAt),
];

#[derive(Iden)]
enum DocumentIndexes {
    Table,
    Id,
    DocumentId,
    IndexField,
    IndexValue,
}

/// Build an INSERT statement for a single document index entry.
fn build_index_insert(doc_id: &str, entry: &IndexEntry) -> Result<sea_query::InsertStatement> {
    let index_id = uuid::Uuid::now_v7().to_string();
    let stmt = Query::insert()
        .into_table(DocumentIndexes::Table)
        .columns([
            DocumentIndexes::Id,
            DocumentIndexes::DocumentId,
            DocumentIndexes::IndexField,
            DocumentIndexes::IndexValue,
        ])
        .values([
            index_id.as_str().into(),
            doc_id.into(),
            entry.field.into(),
            entry.value.as_str().into(),
        ])?
        .to_owned();
    Ok(stmt)
}

// ============================================================================
// Raw Row Types
// ============================================================================

/// Raw row from the `documents` table.
#[derive(sqlx::FromRow)]
struct RawDocumentRow {
    id: String,
    #[expect(dead_code, reason = "reserved for future use")]
    doc_type: String,
    schema_version: i32,
    data: String,
    expires_at: Option<String>,
    created_at: String,
    updated_at: String,
    version: i32,
    last_used_at: Option<String>,
}

/// Raw row with just an id column.
#[derive(sqlx::FromRow)]
struct IdRow {
    id: String,
}

/// Outcome of [`DocumentStore::compare_and_update_unique`].
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The document was updated.
    Updated,
    /// The version guard failed — the document changed concurrently.
    VersionMismatch,
    /// Another document already holds the requested unique index value.
    DuplicateValue,
}

// ============================================================================
// Helpers
// ============================================================================

/// Deserialize a raw row into a typed document.
fn raw_to_document<T: DocumentType>(row: RawDocumentRow) -> Result<Document<T>> {
    #[expect(
        clippy::cast_sign_loss,
        reason = "schema_version stored as i32 but always non-negative"
    )]
    let version = row.schema_version as u32;
    let typed_data = if version < T::CURRENT_VERSION {
        let value: serde_json::Value =
            serde_json::from_str(&row.data).context("failed to parse document JSON")?;
        T::migrate(version, value)?
    } else if version == T::CURRENT_VERSION {
        serde_json::from_str(&row.data).context("failed to deserialize document")?
    } else {
        anyhow::bail!(
            "document schema version {version} is newer than supported version {} for type {}",
            T::CURRENT_VERSION,
            T::DOC_TYPE
        )
    };

    let created_at: Timestamp = row
        .created_at
        .parse()
        .context("failed to parse created_at timestamp")?;
    let updated_at: Timestamp = row
        .updated_at
        .parse()
        .context("failed to parse updated_at timestamp")?;
    let expires_at = row
        .expires_at
        .map(|s| s.parse::<Timestamp>())
        .transpose()
        .context("failed to parse expires_at timestamp")?;
    let last_used_at = row
        .last_used_at
        .map(|s| s.parse::<Timestamp>())
        .transpose()
        .context("failed to parse last_used_at timestamp")?;

    Ok(Document {
        id: row.id,
        data: typed_data,
        created_at,
        updated_at,
        expires_at,
        version: row.version,
        last_used_at,
    })
}

// ============================================================================
// DocumentStore
// ============================================================================

/// Core abstraction for the document store (no encryption).
#[derive(Clone)]
pub struct DocumentStore {
    pool: Pool,
}

impl DocumentStore {
    /// Create a new document store.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Access the underlying pool.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Begin a new store transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction cannot be started.
    pub async fn begin(&self) -> Result<StoreTransaction<'_>> {
        let tx = self.pool.begin().await?;
        Ok(StoreTransaction { tx })
    }

    // ========================================================================
    // Insert
    // ========================================================================

    /// Insert a new document with an auto-generated UUID v7 ID.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn insert<T: DocumentType>(&self, doc: &T) -> Result<Document<T>> {
        crate::with_dsql_retry!(async {
            let id = uuid::Uuid::now_v7().to_string();
            let mut tx = self.begin().await?;
            let result = tx.insert_with_id(&id, doc).await?;
            tx.commit().await?;
            Ok(result)
        })
    }

    /// Insert a new document with a caller-specified ID.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn insert_with_id<T: DocumentType>(&self, id: &str, doc: &T) -> Result<Document<T>> {
        crate::with_dsql_retry!(async {
            let mut tx = self.begin().await?;
            let result = tx.insert_with_id(id, doc).await?;
            tx.commit().await?;
            Ok(result)
        })
    }

    // ========================================================================
    // Get by ID
    // ========================================================================

    /// Get a single document by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    pub async fn get<T: DocumentType>(&self, id: &str) -> Result<Option<Document<T>>> {
        let stmt = Query::select()
            .columns(DOC_COLUMNS)
            .from(Documents::Table)
            .and_where(Expr::col(Documents::Id).eq(id))
            .and_where(Expr::col(Documents::DocType).eq(T::DOC_TYPE))
            .to_owned();

        let row: Option<RawDocumentRow> =
            crate::db_fetch_optional!(&self.pool, stmt, RawDocumentRow)?;

        match row {
            Some(row) => raw_to_document::<T>(row).map(Some),
            None => Ok(None),
        }
    }

    // ========================================================================
    // Find by Index
    // ========================================================================

    /// Find a single document by an indexed field.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    pub async fn find_one<T: DocumentType>(
        &self,
        field: &str,
        value: &str,
    ) -> Result<Option<Document<T>>> {
        let stmt = Query::select()
            .columns(DOC_TABLE_COLUMNS)
            .from(Documents::Table)
            .inner_join(
                DocumentIndexes::Table,
                Expr::col((Documents::Table, Documents::Id))
                    .equals((DocumentIndexes::Table, DocumentIndexes::DocumentId)),
            )
            .and_where(Expr::col((Documents::Table, Documents::DocType)).eq(T::DOC_TYPE))
            .and_where(Expr::col((DocumentIndexes::Table, DocumentIndexes::IndexField)).eq(field))
            .and_where(Expr::col((DocumentIndexes::Table, DocumentIndexes::IndexValue)).eq(value))
            .order_by((Documents::Table, Documents::CreatedAt), Order::Desc)
            .limit(1)
            .to_owned();

        let row: Option<RawDocumentRow> =
            crate::db_fetch_optional!(&self.pool, stmt, RawDocumentRow)?;

        row.map(raw_to_document::<T>).transpose()
    }

    /// Find all documents matching an indexed field.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    pub async fn find_all<T: DocumentType>(
        &self,
        field: &str,
        value: &str,
    ) -> Result<Vec<Document<T>>> {
        let stmt = Query::select()
            .columns(DOC_TABLE_COLUMNS)
            .from(Documents::Table)
            .inner_join(
                DocumentIndexes::Table,
                Expr::col((Documents::Table, Documents::Id))
                    .equals((DocumentIndexes::Table, DocumentIndexes::DocumentId)),
            )
            .and_where(Expr::col((Documents::Table, Documents::DocType)).eq(T::DOC_TYPE))
            .and_where(Expr::col((DocumentIndexes::Table, DocumentIndexes::IndexField)).eq(field))
            .and_where(Expr::col((DocumentIndexes::Table, DocumentIndexes::IndexValue)).eq(value))
            .to_owned();

        let rows: Vec<RawDocumentRow> = crate::db_fetch_all!(&self.pool, stmt, RawDocumentRow)?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            results.push(raw_to_document::<T>(row)?);
        }
        Ok(results)
    }

    // ========================================================================
    // Update
    // ========================================================================

    /// Update a document's data by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn update<T: DocumentType>(&self, id: &str, doc: &T) -> Result<()> {
        crate::with_dsql_retry!(async {
            let mut tx = self.begin().await?;
            tx.update(id, doc).await?;
            tx.commit().await
        })
    }

    /// Conditionally update a document only if its version matches.
    ///
    /// Returns `true` if the update succeeded, `false` on version mismatch.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn compare_and_update<T: DocumentType>(
        &self,
        id: &str,
        expected_version: i32,
        doc: &T,
    ) -> Result<bool> {
        crate::with_dsql_retry!(async {
            let json = serde_json::to_string(doc).context("failed to serialize document")?;
            let now_str = Timestamp::now().to_string();
            let expires = doc.expires_at();
            let indexes = doc.index_entries();

            let expires_str = expires.map(|ts| ts.to_string());
            let expires_ref: Option<&str> = expires_str.as_deref();

            let mut tx = self.pool.begin().await?;

            // UPDATE with version guard (optimistic concurrency)
            let update_stmt = {
                let mut q = Query::update();
                q.table(Documents::Table)
                    .value(Documents::Data, Expr::val(json.as_str()))
                    .value(Documents::ExpiresAt, Expr::val(expires_ref))
                    .value(
                        Documents::SchemaVersion,
                        Expr::val(T::CURRENT_VERSION.cast_signed()),
                    )
                    .value(Documents::UpdatedAt, Expr::val(now_str.as_str()))
                    .value(
                        Documents::Version,
                        Expr::val(expected_version.saturating_add(1)),
                    )
                    .and_where(Expr::col(Documents::Id).eq(id))
                    .and_where(Expr::col(Documents::Version).eq(expected_version));
                q.to_owned()
            };

            let result = crate::tx_execute!(tx, update_stmt)?;

            if result.rows_affected() == 0 {
                return Ok(false);
            }

            // DELETE old indexes
            let delete_idx_stmt = Query::delete()
                .from_table(DocumentIndexes::Table)
                .and_where(Expr::col(DocumentIndexes::DocumentId).eq(id))
                .to_owned();

            crate::tx_execute!(tx, delete_idx_stmt)?;

            // INSERT new indexes
            for entry in &indexes {
                let idx_stmt = build_index_insert(id, entry)?;
                crate::tx_execute!(tx, idx_stmt)?;
            }

            tx.commit().await?;
            Ok(true)
        })
    }

    /// Conditionally update a document, rejecting the write when another
    /// document already holds the index entry `(unique_field, unique_value)`.
    ///
    /// The uniqueness read and the version-guarded update run in one
    /// transaction. Under serializable isolation (Aurora DSQL) two concurrent
    /// writers of the same value conflict; [`with_dsql_retry!`](crate::with_dsql_retry)
    /// re-runs the loser, which then observes the value and returns
    /// [`UpdateOutcome::DuplicateValue`]. On single-writer SQLite the
    /// transaction serializes to the same effect. Callers needing no uniqueness
    /// use [`compare_and_update`](Self::compare_and_update).
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or a database operation fails.
    pub async fn compare_and_update_unique<T: DocumentType>(
        &self,
        id: &str,
        expected_version: i32,
        doc: &T,
        unique_field: &str,
        unique_value: &str,
    ) -> Result<UpdateOutcome> {
        crate::with_dsql_retry!(async {
            let json = serde_json::to_string(doc).context("failed to serialize document")?;
            let now_str = Timestamp::now().to_string();
            let expires = doc.expires_at();
            let indexes = doc.index_entries();

            let expires_str = expires.map(|ts| ts.to_string());
            let expires_ref: Option<&str> = expires_str.as_deref();

            let mut tx = self.pool.begin().await?;

            // Reject if any *other* document already holds the unique value.
            // Reading it inside the transaction is what lets serializable
            // isolation detect a concurrent writer of the same value.
            let clash_stmt = Query::select()
                .expr_as(Expr::col(DocumentIndexes::DocumentId), Alias::new("id"))
                .from(DocumentIndexes::Table)
                .and_where(Expr::col(DocumentIndexes::IndexField).eq(unique_field))
                .and_where(Expr::col(DocumentIndexes::IndexValue).eq(unique_value))
                .and_where(Expr::col(DocumentIndexes::DocumentId).ne(id))
                .limit(1)
                .to_owned();
            let clash: Option<IdRow> = crate::tx_fetch_optional!(tx, clash_stmt, IdRow)?;
            if clash.is_some() {
                return Ok(UpdateOutcome::DuplicateValue);
            }

            // UPDATE with version guard (optimistic concurrency)
            let update_stmt = {
                let mut q = Query::update();
                q.table(Documents::Table)
                    .value(Documents::Data, Expr::val(json.as_str()))
                    .value(Documents::ExpiresAt, Expr::val(expires_ref))
                    .value(
                        Documents::SchemaVersion,
                        Expr::val(T::CURRENT_VERSION.cast_signed()),
                    )
                    .value(Documents::UpdatedAt, Expr::val(now_str.as_str()))
                    .value(
                        Documents::Version,
                        Expr::val(expected_version.saturating_add(1)),
                    )
                    .and_where(Expr::col(Documents::Id).eq(id))
                    .and_where(Expr::col(Documents::Version).eq(expected_version));
                q.to_owned()
            };

            let result = crate::tx_execute!(tx, update_stmt)?;
            if result.rows_affected() == 0 {
                return Ok(UpdateOutcome::VersionMismatch);
            }

            // DELETE old indexes
            let delete_idx_stmt = Query::delete()
                .from_table(DocumentIndexes::Table)
                .and_where(Expr::col(DocumentIndexes::DocumentId).eq(id))
                .to_owned();
            crate::tx_execute!(tx, delete_idx_stmt)?;

            // INSERT new indexes
            for entry in &indexes {
                let idx_stmt = build_index_insert(id, entry)?;
                crate::tx_execute!(tx, idx_stmt)?;
            }

            tx.commit().await?;
            Ok(UpdateOutcome::Updated)
        })
    }

    // ========================================================================
    // Delete
    // ========================================================================

    /// Delete a document by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        crate::with_dsql_retry!(async {
            let mut tx = self.pool.begin().await?;

            // Delete indexes first
            let delete_idx_stmt = Query::delete()
                .from_table(DocumentIndexes::Table)
                .and_where(Expr::col(DocumentIndexes::DocumentId).eq(id))
                .to_owned();
            crate::tx_execute!(tx, delete_idx_stmt)?;

            // Delete document
            let delete_doc_stmt = Query::delete()
                .from_table(Documents::Table)
                .and_where(Expr::col(Documents::Id).eq(id))
                .to_owned();
            let result = crate::tx_execute!(tx, delete_doc_stmt)?;

            tx.commit().await?;
            Ok(result.rows_affected() > 0)
        })
    }

    /// Delete all expired documents.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn delete_expired(&self) -> Result<u64> {
        let now_str = Timestamp::now().to_string();

        // Find expired document IDs
        let find_stmt = Query::select()
            .column(Documents::Id)
            .from(Documents::Table)
            .and_where(Expr::col(Documents::ExpiresAt).is_not_null())
            .and_where(Expr::col(Documents::ExpiresAt).lte(now_str.as_str()))
            .to_owned();

        let rows: Vec<IdRow> = crate::db_fetch_all!(&self.pool, find_stmt, IdRow)?;

        let mut deleted = 0u64;
        for row in &rows {
            if self.delete(&row.id).await? {
                deleted = deleted.saturating_add(1);
            }
        }
        Ok(deleted)
    }

    /// Count all documents of a given type.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn count<T: DocumentType>(&self) -> Result<i64> {
        #[derive(sqlx::FromRow)]
        struct CountRow {
            cnt: i64,
        }

        let stmt = Query::select()
            .expr_as(
                Expr::col(Documents::Id).count(),
                sea_query::Alias::new("cnt"),
            )
            .from(Documents::Table)
            .and_where(Expr::col(Documents::DocType).eq(T::DOC_TYPE))
            .to_owned();

        let row: CountRow = crate::db_fetch_one!(&self.pool, stmt, CountRow)?;

        Ok(row.cnt)
    }

    /// List all documents of a given type.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn list_all<T: DocumentType>(&self) -> Result<Vec<Document<T>>> {
        let stmt = Query::select()
            .columns(DOC_COLUMNS)
            .from(Documents::Table)
            .and_where(Expr::col(Documents::DocType).eq(T::DOC_TYPE))
            .order_by(Documents::CreatedAt, Order::Desc)
            .to_owned();

        let rows: Vec<RawDocumentRow> = crate::db_fetch_all!(&self.pool, stmt, RawDocumentRow)?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            results.push(raw_to_document::<T>(row)?);
        }
        Ok(results)
    }
}

// ============================================================================
// StoreTransaction
// ============================================================================

/// A store-level transaction wrapping the underlying database transaction.
pub struct StoreTransaction<'a> {
    tx: super::pool::Transaction<'a>,
}

impl StoreTransaction<'_> {
    /// Insert a document with a specified ID within the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn insert_with_id<T: DocumentType>(
        &mut self,
        id: &str,
        doc: &T,
    ) -> Result<Document<T>> {
        let json = serde_json::to_string(doc).context("failed to serialize document")?;
        let now = Timestamp::now();
        let now_str = now.to_string();
        let expires_str = doc.expires_at().map(|ts| ts.to_string());
        let expires_ref: Option<&str> = expires_str.as_deref();
        let indexes = doc.index_entries();

        let stmt = Query::insert()
            .into_table(Documents::Table)
            .columns(DOC_COLUMNS)
            .values([
                id.into(),
                T::DOC_TYPE.into(),
                (T::CURRENT_VERSION.cast_signed()).into(),
                json.as_str().into(),
                expires_ref.into(),
                now_str.as_str().into(),
                now_str.as_str().into(),
                1_i32.into(),
                Option::<&str>::None.into(),
            ])?
            .to_owned();

        crate::tx_execute!(self.tx, stmt)?;

        // Insert index entries
        for entry in &indexes {
            let idx_stmt = build_index_insert(id, entry)?;
            crate::tx_execute!(self.tx, idx_stmt)?;
        }

        // Deserialize back into the typed document
        let data: T = serde_json::from_str(&json).context("failed to deserialize inserted doc")?;

        Ok(Document {
            id: id.to_string(),
            data,
            created_at: now,
            updated_at: now,
            expires_at: doc.expires_at(),
            version: 1,
            last_used_at: None,
        })
    }

    /// Update a document within the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the database write fails.
    pub async fn update<T: DocumentType>(&mut self, id: &str, doc: &T) -> Result<()> {
        let json = serde_json::to_string(doc).context("failed to serialize document")?;
        let now_str = Timestamp::now().to_string();
        let expires_str = doc.expires_at().map(|ts| ts.to_string());
        let expires_ref: Option<&str> = expires_str.as_deref();
        let indexes = doc.index_entries();

        let update_stmt = {
            let mut q = Query::update();
            q.table(Documents::Table)
                .value(Documents::Data, Expr::val(json.as_str()))
                .value(Documents::ExpiresAt, Expr::val(expires_ref))
                .value(
                    Documents::SchemaVersion,
                    Expr::val(T::CURRENT_VERSION.cast_signed()),
                )
                .value(Documents::UpdatedAt, Expr::val(now_str.as_str()))
                .value(Documents::Version, Expr::col(Documents::Version).add(1))
                .and_where(Expr::col(Documents::Id).eq(id));
            q.to_owned()
        };

        crate::tx_execute!(self.tx, update_stmt)?;

        // Rebuild indexes
        let delete_idx_stmt = Query::delete()
            .from_table(DocumentIndexes::Table)
            .and_where(Expr::col(DocumentIndexes::DocumentId).eq(id))
            .to_owned();
        crate::tx_execute!(self.tx, delete_idx_stmt)?;

        for entry in &indexes {
            let idx_stmt = build_index_insert(id, entry)?;
            crate::tx_execute!(self.tx, idx_stmt)?;
        }

        Ok(())
    }

    /// Commit the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the commit fails.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use crate::documents::devbox::DevboxDoc;
    use devbox_common::{AmiId, DevboxState, InstanceType, SubnetId};

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
            created_at: Timestamp::now(),
            owner_tag_applied: false,
            warmup_report: None,
        }
    }

    fn row_with_schema_version(schema_version: i32) -> RawDocumentRow {
        let data = serde_json::to_string(&sample_devbox()).unwrap();
        RawDocumentRow {
            id: "doc-1".to_string(),
            doc_type: DevboxDoc::DOC_TYPE.to_string(),
            schema_version,
            data,
            expires_at: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            version: 1,
            last_used_at: None,
        }
    }

    #[test]
    fn raw_to_document_accepts_current_version() {
        let row = row_with_schema_version(1);
        assert!(raw_to_document::<DevboxDoc>(row).is_ok());
    }

    #[test]
    fn raw_to_document_migrates_older_version() {
        let row = row_with_schema_version(0);
        assert!(raw_to_document::<DevboxDoc>(row).is_ok());
    }

    #[test]
    fn raw_to_document_rejects_newer_version() {
        let row = row_with_schema_version(2);
        let err = raw_to_document::<DevboxDoc>(row).unwrap_err();
        assert!(err.to_string().contains("newer"));
    }
}
