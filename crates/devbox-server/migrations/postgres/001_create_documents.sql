CREATE TABLE documents (
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

CREATE INDEX idx_documents_doc_type ON documents(doc_type);
CREATE INDEX idx_documents_expires_at ON documents(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_documents_doc_type_created ON documents(doc_type, created_at);
