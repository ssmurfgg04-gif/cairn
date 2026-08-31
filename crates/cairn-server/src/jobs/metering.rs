//! Metering (SPEC §5.2/§16): server-side counters; nightly rollup recomputes `bytes_stored`
//! from the authoritative chunks table. Presentation is a NON-GOAL — counters only.

use cairn_core::{CairnError, ErrorKind};
use crate::ServerState;

/// YYYY-MM-DD for UTC millis (no chrono dep).
#[must_use]
pub fn day_string(now_millis: i64) -> String {
    let secs = now_millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Nightly rollup: bytes_stored per tenant (authoritative recompute — idempotent).
pub async fn rollup(state: &ServerState) -> Result<u64, CairnError> {
    let day = day_string(state.clock.now_millis());
    let tenants: Vec<String> = sqlx::query("SELECT id FROM tenants")
        .fetch_all(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("tenants: {e}")))?
        .into_iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect();
    let mut n = 0;
    for t in tenants {
        let stored: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(size),0) FROM chunks WHERE tenant_id=?1 AND state='present'",
        )
        .bind(&t)
        .fetch_one(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("sum: {e}")))?;
        sqlx::query(
            "INSERT INTO metering(tenant_id, day, bytes_stored) VALUES(?1,?2,?3)
             ON CONFLICT(tenant_id, day) DO UPDATE SET bytes_stored=?3",
        )
        .bind(&t)
        .bind(&day)
        .bind(stored)
        .execute(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("metering: {e}")))?;
        n += 1;
    }
    Ok(n)
}

use sqlx::Row;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rollup_writes_bytes_stored() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
            .execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO chunks(tenant_id, hash, size, tier, state, last_touched) VALUES('t1','aa',1000,'hot','present',0)")
            .execute(&state.db).await.unwrap();
        rollup(&state).await.unwrap();
        let stored: i64 = sqlx::query_scalar("SELECT bytes_stored FROM metering WHERE tenant_id='t1'")
            .fetch_one(&state.db).await.unwrap();
        assert_eq!(stored, 1000);
    }
}
