//! Leases & fencing (SPEC §8): NLE correctness primitive.
//!
//! Acquire(path, device, ttl) → {token from `projects.next_lease_token` (a DB sequence —
//! survives server restart), expires_at}. Renew (jittered client-side). Release.
//! Enforcement lives in journal::append; TTL cleanup is a job; correctness = fencing.

use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;

use cairn_core::clock::SystemClock;
use cairn_core::{CairnError, ErrorKind};

/// A live lease view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub path: String,
    pub device_id: String,
    pub token: u64,
    pub expires_at: i64,
}

/// Acquire (or renew-by-acquire) a lease: token comes from the project's DB sequence
/// (restart-safe). Takeover by another device bumps the token → old holders fenced out.
pub async fn acquire(
    pool: &SqlitePool,
    clock: &Arc<dyn SystemClock>,
    tenant_id: &str,
    project_id: &str,
    path: &str,
    device_id: &str,
    ttl_ms: u64,
) -> Result<(u64, i64), CairnError> {
    let mut conn = crate::db::begin_immediate(pool).await?;
    // bump the DB sequence (restart-safe token source, SPEC §8)
    sqlx::query("UPDATE projects SET next_lease_token = next_lease_token + 1 WHERE tenant_id=?1 AND project_id=?2")
        .bind(tenant_id)
        .bind(project_id)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    let row = sqlx::query("SELECT next_lease_token FROM projects WHERE tenant_id=?1 AND project_id=?2")
        .bind(tenant_id)
        .bind(project_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
    let token: i64 = row.try_get(0).map_err(db_err)?;
    let expires_at = clock.now_millis() + ttl_ms.max(1000) as i64;
    sqlx::query(
        "INSERT INTO leases(tenant_id, project_id, path, device_id, token, expires_at)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(tenant_id, project_id, path) DO UPDATE SET
           device_id=excluded.device_id, token=excluded.token, expires_at=excluded.expires_at",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(path)
    .bind(device_id)
    .bind(token)
    .bind(expires_at)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    crate::db::commit(&mut conn).await?;
    crate::db::audit(
        pool, clock, tenant_id, device_id, "lease.takeover", path,
        &format!("token={token} ttl_ms={ttl_ms}"),
    )
    .await;
    Ok((token.max(0) as u64, expires_at))
}

/// Renew an existing lease (must still own it).
pub async fn renew(
    pool: &SqlitePool,
    clock: &Arc<dyn SystemClock>,
    tenant_id: &str,
    project_id: &str,
    path: &str,
    device_id: &str,
    token: u64,
    ttl_ms: u64,
) -> Result<i64, CairnError> {
    let now = clock.now_millis();
    let res = sqlx::query(
        "UPDATE leases SET expires_at=?4 WHERE tenant_id=?1 AND project_id=?2 AND path=?3
         AND device_id=?5 AND token=?6 AND expires_at>?7",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(path)
    .bind(now + ttl_ms.max(1000) as i64)
    .bind(device_id)
    .bind(token.max(0) as i64)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(CairnError::new(
            ErrorKind::StaleLease,
            format!("cannot renew {path}: not owner or expired"),
        ));
    }
    Ok(now + ttl_ms.max(1000) as i64)
}

/// Release (must own it).
pub async fn release(
    pool: &SqlitePool,
    tenant_id: &str,
    project_id: &str,
    path: &str,
    device_id: &str,
    token: u64,
) -> Result<(), CairnError> {
    let res = sqlx::query(
        "DELETE FROM leases WHERE tenant_id=?1 AND project_id=?2 AND path=?3
         AND device_id=?4 AND token=?5",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(path)
    .bind(device_id)
    .bind(token.max(0) as i64)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(CairnError::new(
            ErrorKind::StaleLease,
            format!("cannot release {path}: not owner"),
        ));
    }
    Ok(())
}

/// List live leases for a project.
pub async fn list(
    pool: &SqlitePool,
    clock: &Arc<dyn SystemClock>,
    tenant_id: &str,
    project_id: &str,
) -> Result<Vec<Lease>, CairnError> {
    let now = clock.now_millis();
    let rows = sqlx::query(
        "SELECT path, device_id, token, expires_at FROM leases
         WHERE tenant_id=?1 AND project_id=?2 AND expires_at>?3 ORDER BY path",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| Lease {
            path: r.try_get(0).unwrap_or_default(),
            device_id: r.try_get(1).unwrap_or_default(),
            token: r.try_get::<i64, _>(2).unwrap_or(0).max(0) as u64,
            expires_at: r.try_get(3).unwrap_or(0),
        })
        .collect())
}

/// TTL cleanup job step: remove expired rows (advisory model — correctness never depends on
/// this; SPEC §8).
pub async fn cleanup_expired(pool: &SqlitePool, clock: &Arc<dyn SystemClock>) -> Result<u64, CairnError> {
    let res = sqlx::query("DELETE FROM leases WHERE expires_at<=?1")
        .bind(clock.now_millis())
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(res.rows_affected())
}

fn db_err(e: sqlx::Error) -> CairnError {
    CairnError::new(ErrorKind::Unavailable, format!("db: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (tempfile::TempDir, SqlitePool, Arc<dyn SystemClock>) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&std::path::Path::new(dir.path()).join("meta.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO tenants(id, created_at) VALUES('t1', 0)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO projects(tenant_id, project_id, created_at) VALUES('t1','p1',0)")
            .execute(&pool)
            .await
            .unwrap();
        (dir, pool, Arc::new(cairn_core::clock::WallClock))
    }

    /// Tokens come from the DB sequence and SURVIVE server restart (SPEC §8).
    #[tokio::test]
    async fn tokens_are_restart_safe_and_monotonic() {
        let (_d, pool, clock) = setup().await;
        let (t1, _) = acquire(&pool, &clock, "t1", "p1", "a.prproj", "d1", 60_000).await.unwrap();
        // "restart" = new pool handle on the same file; tokens keep rising
        let (t2, _) = acquire(&pool, &clock, "t1", "p1", "b.prproj", "d2", 60_000).await.unwrap();
        assert_eq!((t1, t2), (1, 2));
        // takeover bumps the token → old token fenced
        let (t3, _) = acquire(&pool, &clock, "t1", "p1", "a.prproj", "d2", 60_000).await.unwrap();
        assert_eq!(t3, 3);
    }

    #[tokio::test]
    async fn renew_requires_ownership() {
        let (_d, pool, clock) = setup().await;
        let (t, _) = acquire(&pool, &clock, "t1", "p1", "a.prproj", "d1", 60_000).await.unwrap();
        renew(&pool, &clock, "t1", "p1", "a.prproj", "d1", t, 60_000).await.unwrap();
        let e = renew(&pool, &clock, "t1", "p1", "a.prproj", "d2", t, 60_000).await.unwrap_err();
        assert_eq!(e.code(), "STALE_LEASE");
    }

    #[tokio::test]
    async fn release_and_cleanup() {
        let (_d, pool, _clock) = setup().await;
        let fc = Arc::new(cairn_core::clock::FixedClock::new(1_000));
        let fixed: Arc<dyn SystemClock> = fc.clone();
        let (t, _) = acquire(&pool, &fixed, "t1", "p1", "a.prproj", "d1", 60_000).await.unwrap();
        assert_eq!(list(&pool, &fixed, "t1", "p1").await.unwrap().len(), 1);
        release(&pool, "t1", "p1", "a.prproj", "d1", t).await.unwrap();
        assert!(list(&pool, &fixed, "t1", "p1").await.unwrap().is_empty());
        // cleanup removes expired
        let (t2, _) = acquire(&pool, &fixed, "t1", "p1", "b.prproj", "d1", 5_000).await.unwrap();
        fc.advance(6_000);
        assert_eq!(cleanup_expired(&pool, &fixed).await.unwrap(), 1);
        let _ = t2;
    }
}
