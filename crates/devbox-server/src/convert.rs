//! Conversion from internal document types to API response types.

use devbox_common::DevboxResponse;

use crate::db::document_type::Document;
use crate::documents::devbox::DevboxDoc;

impl From<Document<DevboxDoc>> for DevboxResponse {
    fn from(doc: Document<DevboxDoc>) -> Self {
        DevboxResponse {
            id: doc.id,
            instance_id: doc.data.instance_id,
            name: doc.data.name,
            state: doc.data.state,
            instance_type: doc.data.instance_type,
            ami_id: doc.data.ami_id,
            owner: doc.data.owner,
            region: doc.data.region,
            created_at: doc.created_at.to_string(),
            claimed_at: doc.data.claimed_at.map(|ts| ts.to_string()),
            session: None,
        }
    }
}
