//! Database layer for the devbox server.
//!
//! Provides a multi-backend document store (SQLite + PostgreSQL/Aurora DSQL)
//! with runtime backend selection based on the DATABASE_URL scheme.

pub(crate) mod document_type;
pub(crate) mod dsql;
pub mod migrations;
pub mod pool;
pub(crate) mod store;

pub use pool::{DatabaseType, Pool, PoolConfig, Transaction};
pub use store::{DocumentStore, StoreTransaction, UpdateOutcome};

#[cfg(test)]
mod tests;
