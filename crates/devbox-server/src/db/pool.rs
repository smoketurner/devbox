//! Database pool type abstraction for multi-database support.
//!
//! This module provides a `Pool` enum that wraps both SQLite and PostgreSQL
//! connection pools, enabling runtime database selection based on the
//! `DATABASE_URL` environment variable scheme.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::dsql::{DsqlEndpoint, generate_dsql_token, load_sdk_config};

/// Connection pool configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum pool connections.
    pub max_connections: u32,
    /// Minimum idle connections.
    pub min_connections: u32,
    /// Idle timeout in seconds.
    pub idle_timeout_secs: u64,
    /// Acquire timeout in seconds.
    pub acquire_timeout_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 25,
            min_connections: 2,
            idle_timeout_secs: 300,
            acquire_timeout_secs: 5,
        }
    }
}

/// Database type enum for runtime SQL dialect selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseType {
    /// SQLite database
    Sqlite,
    /// PostgreSQL database (including Aurora DSQL)
    Postgres,
}

impl DatabaseType {
    /// Detect database type from URL scheme.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is not supported.
    pub fn from_url(url: &str) -> Result<Self> {
        if url.starts_with("sqlite:") {
            Ok(Self::Sqlite)
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Ok(Self::Postgres)
        } else {
            bail!(
                "Unsupported database URL scheme. Expected 'sqlite:', 'postgres:', or 'postgresql:' prefix, got: {}",
                url.split(':').next().unwrap_or("empty")
            )
        }
    }
}

/// Database connection pool that wraps both SQLite and PostgreSQL pools.
#[derive(Debug, Clone)]
pub enum Pool {
    /// SQLite connection pool
    Sqlite(sqlx::SqlitePool),
    /// PostgreSQL connection pool
    Postgres(sqlx::PgPool),
}

impl Pool {
    /// Create a new in-memory SQLite pool for testing.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "test-only constructor; .expect surfaces broken in-memory sqlite setup"
    )]
    pub fn new_test() -> Self {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy("sqlite::memory:")
            .expect("Failed to create test SQLite pool");
        Self::Sqlite(pool)
    }

    /// Connect to a database using the URL scheme to determine the backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is not supported or the connection fails.
    pub async fn connect(url: &str, pool_cfg: &PoolConfig) -> Result<Self> {
        let db_type = DatabaseType::from_url(url)?;

        match db_type {
            DatabaseType::Sqlite => {
                let opts = url
                    .parse::<sqlx::sqlite::SqliteConnectOptions>()?
                    .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                    .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
                    .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                    .busy_timeout(Duration::from_secs(5))
                    .pragma("analysis_limit", "400");
                let pool = sqlx::SqlitePool::connect_with(opts).await?;
                Ok(Self::Sqlite(pool))
            }
            DatabaseType::Postgres => {
                if let Some(dsql) = DsqlEndpoint::from_url(url)? {
                    let parsed = url::Url::parse(url).context("failed to parse PostgreSQL URL")?;
                    Self::connect_dsql(&dsql, parsed.username(), pool_cfg).await
                } else {
                    let pool = PgPoolOptions::new()
                        .max_connections(pool_cfg.max_connections)
                        .min_connections(pool_cfg.min_connections)
                        .idle_timeout(Duration::from_secs(pool_cfg.idle_timeout_secs))
                        .acquire_timeout(Duration::from_secs(pool_cfg.acquire_timeout_secs))
                        .connect(url)
                        .await?;
                    Ok(Self::Postgres(pool))
                }
            }
        }
    }

    /// Connect to an Aurora DSQL cluster using IAM authentication.
    async fn connect_dsql(dsql: &DsqlEndpoint, user: &str, pool_cfg: &PoolConfig) -> Result<Self> {
        let region = dsql.region().to_string();

        let user = if user.is_empty() {
            std::env::var("DSQL_USER").unwrap_or_else(|_| "admin".to_string())
        } else {
            user.to_string()
        };
        let is_admin = user == "admin";

        tracing::info!(
            connect_host = dsql.connect_hostname(),
            token_host = dsql.token_hostname(),
            region = region,
            user = user,
            "connecting to Aurora DSQL with IAM authentication"
        );

        let sdk_config = load_sdk_config(Some(&region)).await;

        let token =
            generate_dsql_token(&sdk_config, dsql.token_hostname(), &region, is_admin).await?;

        let mut connect_options = PgConnectOptions::new()
            .host(dsql.connect_hostname())
            .port(5432)
            .database("postgres")
            .username(&user)
            .password(&token)
            .ssl_mode(dsql.ssl_mode());

        if let Some(opt) = dsql.pg_options() {
            connect_options = connect_options.options([opt]);
        }

        let pool = PgPoolOptions::new()
            .max_connections(pool_cfg.max_connections)
            .min_connections(pool_cfg.min_connections)
            .max_lifetime(Duration::from_secs(3300)) // 55 minutes
            .idle_timeout(Duration::from_secs(pool_cfg.idle_timeout_secs))
            .acquire_timeout(Duration::from_secs(pool_cfg.acquire_timeout_secs))
            .test_before_acquire(false)
            .before_acquire(|conn, meta| {
                Box::pin(async move {
                    if meta.idle_for.as_secs() > 30 {
                        sqlx::Connection::ping(conn).await?;
                    }
                    Ok(true)
                })
            })
            .connect_with(connect_options)
            .await
            .context("failed to connect to DSQL cluster")?;

        spawn_token_refresh(pool.clone(), dsql.clone(), user, is_admin);

        Ok(Self::Postgres(pool))
    }

    /// Get the database type for this pool.
    #[must_use]
    pub fn db_type(&self) -> DatabaseType {
        match self {
            Self::Sqlite(_) => DatabaseType::Sqlite,
            Self::Postgres(_) => DatabaseType::Postgres,
        }
    }

    /// Begin a new transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction cannot be started.
    pub async fn begin(&self) -> Result<Transaction<'_>> {
        match self {
            Self::Sqlite(pool) => {
                let tx = pool.begin().await?;
                Ok(Transaction::Sqlite(tx))
            }
            Self::Postgres(pool) => {
                let tx = pool.begin().await?;
                Ok(Transaction::Postgres(tx))
            }
        }
    }

    /// Close the pool and release all connections.
    pub async fn close(&self) {
        match self {
            Self::Sqlite(pool) => pool.close().await,
            Self::Postgres(pool) => pool.close().await,
        }
    }

    /// Check database connectivity by executing a simple query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn is_healthy(&self) -> Result<()> {
        match self {
            Self::Sqlite(pool) => {
                sqlx::query_scalar::<_, i32>("SELECT 1")
                    .fetch_one(pool)
                    .await
                    .context("SQLite health check failed")?;
                Ok(())
            }
            Self::Postgres(pool) => {
                sqlx::query_scalar::<_, i32>("SELECT 1")
                    .fetch_one(pool)
                    .await
                    .context("PostgreSQL health check failed")?;
                Ok(())
            }
        }
    }

    /// Check if the pool has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Sqlite(pool) => pool.is_closed(),
            Self::Postgres(pool) => pool.is_closed(),
        }
    }
}

/// Database transaction that wraps both SQLite and PostgreSQL transactions.
pub enum Transaction<'a> {
    /// SQLite transaction
    Sqlite(sqlx::Transaction<'a, sqlx::Sqlite>),
    /// PostgreSQL transaction
    Postgres(sqlx::Transaction<'a, sqlx::Postgres>),
}

impl Transaction<'_> {
    /// Commit the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the commit fails.
    pub async fn commit(self) -> Result<()> {
        match self {
            Self::Sqlite(tx) => {
                tx.commit().await?;
                Ok(())
            }
            Self::Postgres(tx) => {
                tx.commit().await?;
                Ok(())
            }
        }
    }

    /// Get the database type for this transaction.
    #[must_use]
    pub fn db_type(&self) -> DatabaseType {
        match self {
            Self::Sqlite(_) => DatabaseType::Sqlite,
            Self::Postgres(_) => DatabaseType::Postgres,
        }
    }
}

// ============================================================================
// Query Execution Helpers
// ============================================================================

/// Generic query result that works with both SQLite and PostgreSQL.
#[derive(Debug)]
pub(crate) struct QueryResult {
    rows_affected: u64,
}

impl QueryResult {
    /// Number of rows affected by the query.
    #[must_use]
    pub(crate) fn rows_affected(&self) -> u64 {
        self.rows_affected
    }
}

impl From<sqlx::sqlite::SqliteQueryResult> for QueryResult {
    fn from(result: sqlx::sqlite::SqliteQueryResult) -> Self {
        Self {
            rows_affected: result.rows_affected(),
        }
    }
}

impl From<sqlx::postgres::PgQueryResult> for QueryResult {
    fn from(result: sqlx::postgres::PgQueryResult) -> Self {
        Self {
            rows_affected: result.rows_affected(),
        }
    }
}

// ============================================================================
// Macros
// ============================================================================

/// Retry an async block on transient DSQL errors.
#[macro_export]
macro_rules! with_dsql_retry {
    ($body:expr) => {{
        let mut __attempt = 0u32;
        loop {
            match $body.await {
                Ok(val) => break Ok(val),
                Err(e)
                    if $crate::db::pool::is_retryable_db_error(&e)
                        && __attempt < $crate::db::pool::MAX_DSQL_RETRIES =>
                {
                    tracing::warn!(
                        attempt = __attempt,
                        error = %e,
                        "transient DSQL error, retrying"
                    );
                    tokio::time::sleep(
                        $crate::db::pool::retry_backoff(__attempt),
                    )
                    .await;
                    __attempt = __attempt.saturating_add(1);
                }
                Err(e) => break Err(e),
            }
        }
    }};
}

/// Execute a sea-query statement that returns no rows against the pool.
#[macro_export]
macro_rules! db_execute {
    ($pool:expr, $stmt:expr) => {
        match $pool {
            $crate::db::Pool::Sqlite(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                    .execute(p)
                    .await
                    .map($crate::db::pool::QueryResult::from)
            }
            $crate::db::Pool::Postgres(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                    .execute(p)
                    .await
                    .map($crate::db::pool::QueryResult::from)
            }
        }
    };
}

/// Fetch all rows from a sea-query statement against the pool.
#[macro_export]
macro_rules! db_fetch_all {
    ($pool:expr, $stmt:expr, $row_type:ty) => {
        match $pool {
            $crate::db::Pool::Sqlite(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_all(p)
                    .await
            }
            $crate::db::Pool::Postgres(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_all(p)
                    .await
            }
        }
    };
}

/// Fetch a single row from a sea-query statement against the pool.
#[macro_export]
macro_rules! db_fetch_one {
    ($pool:expr, $stmt:expr, $row_type:ty) => {
        match $pool {
            $crate::db::Pool::Sqlite(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_one(p)
                    .await
            }
            $crate::db::Pool::Postgres(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_one(p)
                    .await
            }
        }
    };
}

/// Fetch an optional row from a sea-query statement against the pool.
#[macro_export]
macro_rules! db_fetch_optional {
    ($pool:expr, $stmt:expr, $row_type:ty) => {
        match $pool {
            $crate::db::Pool::Sqlite(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_optional(p)
                    .await
            }
            $crate::db::Pool::Postgres(p) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_optional(p)
                    .await
            }
        }
    };
}

/// Execute a sea-query statement against a transaction.
#[macro_export]
macro_rules! tx_execute {
    ($tx:expr, $stmt:expr) => {
        match $tx {
            $crate::db::Transaction::Sqlite(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                    .execute(&mut **t)
                    .await
                    .map($crate::db::pool::QueryResult::from)
            }
            $crate::db::Transaction::Postgres(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                    .execute(&mut **t)
                    .await
                    .map($crate::db::pool::QueryResult::from)
            }
        }
    };
}

/// Fetch all rows from a sea-query statement against a transaction.
#[macro_export]
macro_rules! tx_fetch_all {
    ($tx:expr, $stmt:expr, $row_type:ty) => {
        match $tx {
            $crate::db::Transaction::Sqlite(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_all(&mut **t)
                    .await
            }
            $crate::db::Transaction::Postgres(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_all(&mut **t)
                    .await
            }
        }
    };
}

/// Fetch a single row from a sea-query statement against a transaction.
#[macro_export]
macro_rules! tx_fetch_one {
    ($tx:expr, $stmt:expr, $row_type:ty) => {
        match $tx {
            $crate::db::Transaction::Sqlite(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_one(&mut **t)
                    .await
            }
            $crate::db::Transaction::Postgres(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_one(&mut **t)
                    .await
            }
        }
    };
}

/// Fetch an optional row from a sea-query statement against a transaction.
#[macro_export]
macro_rules! tx_fetch_optional {
    ($tx:expr, $stmt:expr, $row_type:ty) => {
        match $tx {
            $crate::db::Transaction::Sqlite(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::SqliteQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_optional(&mut **t)
                    .await
            }
            $crate::db::Transaction::Postgres(ref mut t) => {
                use sea_query_sqlx::SqlxBinder;
                let (sql, values) = $stmt.build_sqlx(sea_query::PostgresQueryBuilder);
                sqlx::query_as_with::<_, $row_type, _>(sqlx::AssertSqlSafe(sql), values)
                    .fetch_optional(&mut **t)
                    .await
            }
        }
    };
}

// ============================================================================
// DSQL Token Refresh
// ============================================================================

/// Spawn a background task that periodically refreshes DSQL authentication tokens.
fn spawn_token_refresh(pool: sqlx::PgPool, dsql: DsqlEndpoint, user: String, is_admin: bool) {
    tokio::spawn(async move {
        let refresh_interval = Duration::from_secs(600); // 10 minutes
        let region = dsql.region().to_string();

        tracing::info!(
            "DSQL token refresh task started (region={}, refresh_interval=10m)",
            region,
        );

        let mut interval = tokio::time::interval(refresh_interval);
        interval.tick().await; // skip the immediate first tick

        loop {
            tokio::select! {
                () = pool.close_event() => {
                    tracing::debug!("DSQL pool closed, stopping token refresh task");
                    break;
                }
                _ = interval.tick() => {
                    let sdk_config = load_sdk_config(Some(&region)).await;

                    match generate_dsql_token(
                        &sdk_config,
                        dsql.token_hostname(),
                        &region,
                        is_admin,
                    )
                    .await
                    {
                        Ok(new_token) => {
                            let mut new_options = PgConnectOptions::new()
                                .host(dsql.connect_hostname())
                                .port(5432)
                                .database("postgres")
                                .username(&user)
                                .password(&new_token)
                                .ssl_mode(dsql.ssl_mode());

                            if let Some(opt) = dsql.pg_options() {
                                new_options = new_options.options([opt]);
                            }

                            pool.set_connect_options(new_options);
                            tracing::debug!("DSQL authentication token refreshed successfully");
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "failed to refresh DSQL authentication token"
                            );
                        }
                    }
                }
            }
        }
    });
}

// ============================================================================
// DSQL Retry Helpers
// ============================================================================

/// Maximum number of retries for transient DSQL errors.
pub(crate) const MAX_DSQL_RETRIES: u32 = 3;

/// SQLSTATE codes that indicate a transient, retryable error.
const RETRYABLE_SQL_STATES: &[&str] = &[
    "40001", "OC000", "OC001", "5", "6", "261", "517", "773", "262", "518",
];

/// Match `code` against retryable SQL states.
fn is_retryable_code(code: &str) -> bool {
    RETRYABLE_SQL_STATES.contains(&code)
}

/// Check whether an error is a transient, retryable database error.
pub(crate) fn is_retryable_db_error(err: &anyhow::Error) -> bool {
    if let Some(sqlx_err) = err.downcast_ref::<sqlx::Error>()
        && let sqlx::Error::Database(db_err) = sqlx_err
        && let Some(code) = db_err.code()
    {
        return is_retryable_code(code.as_ref());
    }
    false
}

/// Check whether an error is a unique/primary-key constraint violation.
#[allow(dead_code, reason = "utility used by higher-level store operations")]
pub(crate) fn is_unique_violation(err: &anyhow::Error) -> bool {
    if let Some(sqlx_err) = err.downcast_ref::<sqlx::Error>()
        && let sqlx::Error::Database(db_err) = sqlx_err
    {
        return db_err.is_unique_violation();
    }
    false
}

/// Compute a jittered exponential backoff duration for the given attempt.
pub(crate) fn retry_backoff(attempt: u32) -> Duration {
    let base_ms = 100u64.saturating_mul(1u64 << attempt);
    let jitter_range = base_ms.saturating_div(4);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let modulus = jitter_range.saturating_mul(2).saturating_add(1);
    let jitter = u64::from(nanos)
        .checked_rem(modulus)
        .unwrap_or(0)
        .saturating_sub(jitter_range);
    Duration::from_millis(base_ms.saturating_add(jitter))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    #[test]
    fn test_database_type_from_url_sqlite() {
        assert_eq!(
            DatabaseType::from_url("sqlite::memory:").unwrap(),
            DatabaseType::Sqlite
        );
        assert_eq!(
            DatabaseType::from_url("sqlite:test.db").unwrap(),
            DatabaseType::Sqlite
        );
    }

    #[test]
    fn test_database_type_from_url_postgres() {
        assert_eq!(
            DatabaseType::from_url("postgres://localhost/devbox").unwrap(),
            DatabaseType::Postgres
        );
        assert_eq!(
            DatabaseType::from_url("postgresql://localhost/devbox").unwrap(),
            DatabaseType::Postgres
        );
    }

    #[test]
    fn test_database_type_from_url_invalid() {
        assert!(DatabaseType::from_url("mysql://localhost/db").is_err());
        assert!(DatabaseType::from_url("invalid").is_err());
        assert!(DatabaseType::from_url("").is_err());
    }

    #[tokio::test]
    async fn test_pool_connect_sqlite() {
        let pool = Pool::connect("sqlite::memory:", &PoolConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.db_type(), DatabaseType::Sqlite);
    }

    #[test]
    fn test_is_retryable_db_error_non_db() {
        let err = anyhow::anyhow!("some random error");
        assert!(!is_retryable_db_error(&err));
    }

    #[test]
    fn test_is_retryable_code_dsql() {
        assert!(is_retryable_code("40001"));
        assert!(is_retryable_code("OC000"));
        assert!(is_retryable_code("OC001"));
    }

    #[test]
    fn test_is_retryable_code_sqlite() {
        assert!(is_retryable_code("5"));
        assert!(is_retryable_code("6"));
        assert!(is_retryable_code("261"));
        assert!(is_retryable_code("517"));
        assert!(is_retryable_code("773"));
    }

    #[test]
    fn test_is_retryable_code_non_retryable() {
        assert!(!is_retryable_code("23505"));
        assert!(!is_retryable_code(""));
        assert!(!is_retryable_code("not-a-code"));
    }

    #[test]
    fn test_is_unique_violation_non_db() {
        let err = anyhow::anyhow!("some random error");
        assert!(!is_unique_violation(&err));
    }

    #[test]
    fn test_retry_backoff_increases() {
        let d0 = retry_backoff(0);
        let d1 = retry_backoff(1);
        let d2 = retry_backoff(2);
        assert!(d0.as_millis() <= 200);
        assert!(d1.as_millis() <= 400);
        assert!(d2.as_millis() <= 800);
    }
}
