//! DSQL-compatible migration runner.
//!
//! Aurora DSQL has restrictions that prevent standard sqlx migrations:
//! - No `pg_advisory_lock` support
//! - No mixing DDL and DML in the same transaction

use anyhow::{Context, Result};
use sqlx::PgPool;

/// Result of running migrations: (newly_applied, total).
pub(crate) type MigrationResult = (usize, usize);

/// Run PostgreSQL migrations with DSQL compatibility.
///
/// # Errors
///
/// Returns an error if a migration fails to apply.
pub async fn run_dsql_migrations(pool: &PgPool) -> Result<MigrationResult> {
    let migrator = sqlx::migrate!("./migrations/postgres");
    let total = migrator.iter().count();

    create_migrations_table(pool).await?;

    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .context("failed to query applied migrations")?;

    let applied_set: std::collections::HashSet<i64> = applied.into_iter().collect();

    let mut newly_applied: usize = 0;

    for migration in migrator.iter() {
        let version = migration.version;

        if applied_set.contains(&version) {
            continue;
        }

        tracing::info!(version, description = %migration.description, "applying migration");

        let start = std::time::Instant::now();

        match sqlx::raw_sql(migration.sql.clone()).execute(pool).await {
            Ok(_) => {}
            Err(e) if is_duplicate_object_error(&e) => {
                tracing::warn!(
                    version,
                    description = %migration.description,
                    "migration DDL already applied on a prior attempt, recovering",
                );
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!(
                    "failed to execute migration {}: {}",
                    version, migration.description
                )));
            }
        }

        let elapsed = start.elapsed();

        record_migration(pool, migration, elapsed)
            .await
            .with_context(|| format!("failed to record migration {}", version))?;

        newly_applied = newly_applied.saturating_add(1);
    }

    Ok((newly_applied, total))
}

/// Run SQLite migrations using the embedded migrator.
///
/// # Errors
///
/// Returns an error if migrations fail to apply.
pub async fn run_sqlite_migrations(pool: &sqlx::SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations/sqlite")
        .run(pool)
        .await
        .context("failed to run SQLite migrations")?;
    Ok(())
}

/// Returns true if `err` is a PostgreSQL duplicate-object error.
fn is_duplicate_object_error(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|c| is_duplicate_object_sqlstate(c.as_ref()))
}

/// True if `code` is a "duplicate object" SQLSTATE.
fn is_duplicate_object_sqlstate(code: &str) -> bool {
    matches!(code, "42P07" | "42710" | "42P06" | "42701" | "42P16")
}

/// Create the _sqlx_migrations table if it doesn't exist.
async fn create_migrations_table(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("failed to create _sqlx_migrations table")?;

    Ok(())
}

/// Record a completed migration in the tracking table.
async fn record_migration(
    pool: &PgPool,
    migration: &sqlx::migrate::Migration,
    elapsed: std::time::Duration,
) -> Result<()> {
    use aws_lc_rs::digest;

    let checksum = digest::digest(&digest::SHA384, migration.sql.as_str().as_bytes())
        .as_ref()
        .to_vec();

    let elapsed_nanos = i64::try_from(elapsed.as_nanos())
        .context("migration elapsed time exceeds i64 nanoseconds")?;

    sqlx::query(
        r#"
        INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (version) DO NOTHING
        "#,
    )
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(true)
    .bind(&checksum)
    .bind(elapsed_nanos)
    .execute(pool)
    .await
    .context("failed to insert migration record")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_duplicate_object_sqlstate;

    #[test]
    fn classifies_duplicate_table() {
        assert!(is_duplicate_object_sqlstate("42P07"));
    }

    #[test]
    fn classifies_duplicate_object() {
        assert!(is_duplicate_object_sqlstate("42710"));
    }

    #[test]
    fn does_not_classify_unrelated_errors() {
        assert!(!is_duplicate_object_sqlstate("23505"));
        assert!(!is_duplicate_object_sqlstate("42703"));
        assert!(!is_duplicate_object_sqlstate(""));
    }
}
