//! Server DB: SQLite-compatible pool + idempotent DDL migrations (SPEC §5.2, ADR-0006).
//!
//! Dev uses a local SQLite file via sqlx (bundled driver). The same portable SQL runs on
//! libsql/D1 in production — no stored procedures, no vendor functions beyond RETURNING
//! (supported by SQLite ≥3.35, libsql, and D1).

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cairn_core::clock::SystemClock;
use cairn_core::CairnError;

const DDL: &str = include_str!("../migrations/0001_init.sql");

/// Open the metadata DB pool.
pub async fn open(path: &Path) -> Result<sqlx::SqlitePool, CairnError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("mkdir db: {e}")))?;
    }
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("db url: {e}")))?
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(5000))
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("db connect: {e}")))?;
    Ok(pool)
}

/// Apply DDL (CREATE TABLE IF NOT EXISTS — safe at every boot).
pub async fn migrate(pool: &sqlx::SqlitePool) -> Result<(), CairnError> {
    // sqlite driver executes one statement per execute; split on ';' boundaries safely
    for stmt in split_sql(DDL) {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("ddl: {e}")))?;
    }
    Ok(())
}

fn split_sql(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(|chunk| {
            // strip comment lines first — a leading comment block must not kill the statement
            chunk
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// BEGIN IMMEDIATE transaction helper (portable; serializes writers for the conflict rule).
pub async fn begin_immediate(
    pool: &sqlx::SqlitePool,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, CairnError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("pool: {e}")))?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("begin: {e}")))?;
    Ok(conn)
}

pub async fn commit(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
) -> Result<(), CairnError> {
    sqlx::query("COMMIT")
        .execute(&mut **conn)
        .await
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("commit: {e}")))?;
    Ok(())
}

pub async fn rollback(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
}

/// Audit log helper (SPEC §13: authz denials, ref updates, lease takeover, GC sweeps,
/// tiering/recall, admin actions).
pub async fn audit(
    pool: &sqlx::SqlitePool,
    clock: &Arc<dyn SystemClock>,
    tenant_id: &str,
    actor: &str,
    action: &str,
    resource: &str,
    detail: &str,
) {
    let _ = sqlx::query(
        "INSERT INTO audit_log(tenant_id, actor, action, resource, ts, detail)
         VALUES(?1,?2,?3,?4,?5,?6)",
    )
    .bind(tenant_id)
    .bind(actor)
    .bind(action)
    .bind(resource)
    .bind(clock.now_millis())
    .bind(detail)
    .execute(pool)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(&dir.path().join("meta.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        migrate(&pool).await.unwrap(); // second boot: no error, no loss
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(n >= 15, "expected full DDL, got {n} tables");
    }
}
