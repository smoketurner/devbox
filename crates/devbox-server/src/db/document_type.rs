//! Document type trait and generic document wrapper.
//!
//! Every domain-specific document type implements [`DocumentType`] to declare
//! its storage metadata: document type string, index entries for equality
//! lookups, expiration behavior, and schema version for lazy migration.

use jiff::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A searchable index entry on a document.
///
/// Each entry becomes a row in `document_indexes` with the field name and
/// a plaintext value.
pub struct IndexEntry {
    /// The index field name (e.g., "state", "owner", "instance_id").
    pub field: &'static str,
    /// The plaintext value to index.
    pub value: String,
    /// Enforce global uniqueness of this `(field, value)` pair across all
    /// documents. Unique entries get a primary key derived from the value, so
    /// a second document writing the same value fails with a unique violation
    /// at the database level.
    pub unique: bool,
}

/// Trait implemented by every document type stored in the document store.
///
/// # Required
///
/// - [`DOC_TYPE`](Self::DOC_TYPE): Unique string identifier stored in the
///   `doc_type` column.
/// - [`index_entries`](Self::index_entries): Searchable fields for this
///   document.
///
/// # Optional
///
/// - [`CURRENT_VERSION`](Self::CURRENT_VERSION): Schema version (default 1).
///   Bump when making breaking serde changes.
/// - [`expires_at`](Self::expires_at): Return a timestamp if this document
///   should be automatically cleaned up.
/// - [`migrate`](Self::migrate): Transform old schema versions to current.
pub trait DocumentType: Serialize + DeserializeOwned + Send + Sync {
    /// Unique document type identifier (e.g., `"devbox"`).
    const DOC_TYPE: &'static str;

    /// Current schema version. Increment for breaking serde changes.
    const CURRENT_VERSION: u32 = 1;

    /// Index entries for equality lookups.
    fn index_entries(&self) -> Vec<IndexEntry>;

    /// Optional expiration timestamp.
    fn expires_at(&self) -> Option<Timestamp> {
        None
    }

    /// Migrate a document from an older schema version.
    ///
    /// # Errors
    ///
    /// Returns an error if the migration or deserialization fails.
    fn migrate(_version: u32, data: serde_json::Value) -> anyhow::Result<Self> {
        serde_json::from_value(data).map_err(|e| anyhow::anyhow!("document migration failed: {e}"))
    }
}

/// A document retrieved from the store, wrapping the typed data with metadata.
#[derive(Debug, Clone)]
pub struct Document<T> {
    /// UUID v7 document ID.
    pub id: String,
    /// The deserialized document data.
    pub data: T,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Last-update timestamp.
    pub updated_at: Timestamp,
    /// Optional expiration timestamp.
    pub expires_at: Option<Timestamp>,
    /// Optimistic concurrency version. Incremented on every update.
    pub version: i32,
    /// Lightweight last-used timestamp.
    pub last_used_at: Option<Timestamp>,
}
