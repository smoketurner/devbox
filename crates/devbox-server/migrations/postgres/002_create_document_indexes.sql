CREATE TABLE document_indexes (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    index_field TEXT NOT NULL,
    index_value TEXT NOT NULL,
    UNIQUE(document_id, index_field, index_value)
);

CREATE INDEX idx_document_indexes_lookup ON document_indexes(index_field, index_value);
CREATE INDEX idx_document_indexes_document_id ON document_indexes(document_id);
