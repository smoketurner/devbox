-- Document store schema for devbox-server (SQLite)

CREATE TABLE IF NOT EXISTS documents (
    id TEXT PRIMARY KEY,
    doc_type TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1,
    data TEXT NOT NULL,
    expires_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_documents_doc_type ON documents(doc_type);
CREATE INDEX IF NOT EXISTS idx_documents_expires_at ON documents(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_documents_doc_type_created ON documents(doc_type, created_at);

CREATE TABLE IF NOT EXISTS document_indexes (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    index_field TEXT NOT NULL,
    index_value TEXT NOT NULL,
    UNIQUE(document_id, index_field, index_value)
);

CREATE INDEX IF NOT EXISTS idx_document_indexes_lookup ON document_indexes(index_field, index_value);
CREATE INDEX IF NOT EXISTS idx_document_indexes_document_id ON document_indexes(document_id);
