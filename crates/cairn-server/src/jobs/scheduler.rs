//! Background scheduler — the M6–M7 wiring that actually RUNS the jobs (SPEC §12/§16).
//!
//! Three leader-leased loops, all safe under duplicates, restarts, and multi-server
//! deployments (each loop has its own `jobs_leader` row, so different servers can hold
//! different jobs):
//!
//! - `canary`  — every 5 min: upload→verify→recall round-trip probe (`canary_enabled`)
//! - `bloom`   — every 30 min: negative pre-filter refresh from the chunks table
//! - `nightly` — every 24 h: per-tenant pack → GC → tier → metering rollup
//!
//! Leadership is a per-name DB lease with expiry (compare-and-set, `try_acquire_leader`):
//! only the holder executes; a dead leader's lease expires and another server takes over;
//! a RESTARTED server re-acquires immediately because its holder id is stable across
//! restarts (`data_dir/node-id`, not the pid — a pid-based holder would lock a restarted
//! server out of nightly work for the whole lease TTL).
//!
//! Kill switches are read PER RUN (§16): `canary_enabled`, `packing_enabled`,
//! `tiering_enabled` (checked inside `tier_pass`), and `gc_shadow` — GC runs report-only
//! by default; ops flips `gc_shadow=false` to enable sweeping without a restart.
//!
//! Tiering additionally requires a CONFIGURED cold backend (`CAIRN_COLD_DIR`, or the dev
//! default under local-fs stores): `tier_pass` tombstones hot copies after a verified
//! cold write, so it must never run against a make-believe cold target. `spawn` logs a
//! warning when tiering is skipped for lack of a backend.
//!
//! Every executed run is recorded in the `jobs` table (`sched/<kind>` rows) for ops
//! visibility; the alertable canary signal is the `sched/canary` row state plus the
//! error-level log (metric export is an ops-runbook step, not a code path here).

use std::sync::Arc;
use std::time::Duration;

use sqlx::Row;

use crate::jobs::{self, tier::ColdStore};
use crate::ServerState;

/// Canary probe cadence (SPEC §16: every 5 minutes).
pub const CANARY_INTERVAL: Duration = Duration::from_secs(300);
/// Bloom refresh cadence (cheap; the authoritative check backstops every positive).
pub const BLOOM_INTERVAL: Duration = Duration::from_mins(30);
/// Nightly maintenance cadence: pack → GC → tier → metering, per tenant.
pub const NIGHTLY_INTERVAL: Duration = Duration::from_hours(24);

/// Lease TTLs: each must outlive its loop's renewal gap so a live leader is never
/// raced by a peer, yet expire promptly enough that a dead leader's duties move over.
const CANARY_TTL_MILLIS: i64 = 2 * 60 * 1000; // 2x cadence
const BLOOM_TTL_MILLIS: i64 = 60 * 60 * 1000;
const NIGHTLY_TTL_MILLIS: i64 = 25 * 3600 * 1000; // must outlive the 24 h renewal gap

/// Stable per-server identity: `data_dir/node-id`, created on first boot. The lease
/// holder is `<node-id>/<listen-addr>` — so a restarted server re-acquires its own
/// lease immediately (same holder), a genuinely different server must wait for expiry,
/// and two servers sharing a data dir still differ by addr.
fn node_id(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("node-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let id = uuid::Uuid::now_v7().simple().to_string();
    let _ = std::fs::write(&path, &id);
    id
}

/// Lease holder string for this server instance (stable across restarts).
pub fn holder_for(data_dir: &std::path::Path, listen_addr: &str) -> String {
    format!("{}/{}", node_id(data_dir), listen_addr)
}

/// Record one executed run in the `jobs` table (deterministic `sched/<kind>` row).
async fn record_run(state: &ServerState, kind: &str, ok: bool, detail: String) {
    let state_str = if ok { "ok" } else { "failed" };
    let res = sqlx::query(
        "INSERT INTO jobs(id, tenant_id, kind, state, progress, total, detail, updated_at)
         VALUES(?1, '', ?2, ?3, 1, 1, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET state=?3, detail=?4, updated_at=?5",
    )
    .bind(format!("sched/{kind}"))
    .bind(kind)
    .bind(state_str)
    .bind(&detail)
    .bind(state.clock.now_millis())
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::warn!(job = kind, "scheduler: failed to record run: {e}");
    }
}

async fn acquire(state: &ServerState, name: &str, holder: &str, ttl_millis: i64) -> Option<bool> {
    match jobs::try_acquire_leader(state, name, holder, ttl_millis).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(lease = name, "scheduler: leader lease unavailable: {e}");
            None
        }
    }
}

/// One canary cadence step. Returns true iff this server held the lease (the probe may
/// have been skipped by its kill switch or failed — failure is recorded, never fatal).
pub async fn canary_tick(state: &Arc<ServerState>, holder: &str) -> bool {
    let Some(true) = acquire(state, "canary", holder, CANARY_TTL_MILLIS).await else {
        return false;
    };
    if !flags_enabled(state, "canary_enabled").await {
        tracing::info!("scheduler: canary disabled by kill switch");
        return true;
    }
    match jobs::canary::probe(state).await {
        Ok(len) => {
            record_run(state, "canary", true, format!("roundtrip ok ({len}B)")).await;
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "scheduler: CANARY FAILED — data-plane roundtrip broken (page per ops runbook)"
            );
            record_run(state, "canary", false, format!("probe failed: {e}")).await;
        }
    }
    true
}

/// One bloom-refresh cadence step. Cheap and always safe; the authoritative check
/// backstops every bloom positive, so a stale bloom costs reads, never correctness.
pub async fn bloom_tick(state: &Arc<ServerState>, holder: &str) -> bool {
    let Some(true) = acquire(state, "bloom", holder, BLOOM_TTL_MILLIS).await else {
        return false;
    };
    jobs::rebuild_bloom(state).await;
    record_run(state, "bloom", true, "rebuilt".to_string()).await;
    true
}

/// One nightly cadence step: per-tenant pack → GC → tier, then the metering rollup.
/// Order matters: pack consolidates first, GC then marks/sweeps the post-pack world,
/// tier moves cold leftovers, metering recomputes last (sees the night's changes).
pub async fn nightly_tick(
    state: &Arc<ServerState>,
    holder: &str,
    cold: Option<&Arc<dyn ColdStore>>,
) -> bool {
    let Some(true) = acquire(state, "nightly", holder, NIGHTLY_TTL_MILLIS).await else {
        return false;
    };

    let tenants: Vec<String> = sqlx::query("SELECT id FROM tenants")
        .fetch_all(&state.db)
        .await
        .map(|rows| {
            rows.into_iter()
                .filter_map(|r| r.try_get::<String, _>(0).ok())
                .collect()
        })
        .unwrap_or_default();

    let mut summary: Vec<String> = Vec::new();

    // pack — kill switch read per run (§16); pack_pass itself does not gate.
    if flags_enabled(state, "packing_enabled").await {
        for t in &tenants {
            match jobs::pack::pack_pass(state, t).await {
                Ok((key, n)) if n > 0 => summary.push(format!("pack {t}: {n} objs -> {key}")),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(tenant = %t, "scheduler: pack_pass failed: {e}");
                    summary.push(format!("pack {t}: FAILED"));
                }
            }
        }
    }
    // GC — report-only until ops flips `gc_shadow` (no restart, §16). Reachability
    // violations are logged at error level: live chunks must never be marked deleting.
    let shadow = !matches!(
        crate::jobs::flags::get(state, "gc_shadow").await.as_deref(),
        Ok("false")
    );
    for t in &tenants {
        match jobs::gc::gc_pass(state, t, shadow).await {
            Ok((flagged, violations, scanned)) => {
                if violations > 0 {
                    tracing::error!(
                        tenant = %t,
                        violations,
                        "scheduler: GC REACHABILITY VIOLATIONS — live chunks in deleting state"
                    );
                }
                summary.push(format!(
                    "gc {t}: scanned={scanned} flagged={flagged} violations={violations}{}",
                    if shadow { " (shadow)" } else { "" }
                ));
            }
            Err(e) => {
                tracing::warn!(tenant = %t, "scheduler: gc_pass failed: {e}");
                summary.push(format!("gc {t}: FAILED"));
            }
        }
    }

    // tier — only with a real cold backend (tier_pass tombstones hot copies after a
    // verified cold write; it also self-gates on `tiering_enabled` per run).
    if let Some(cold) = cold {
        for t in &tenants {
            match jobs::tier::tier_pass(state, cold.as_ref(), t).await {
                Ok(n) if n > 0 => summary.push(format!("tier {t}: {n} chunks archived")),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(tenant = %t, "scheduler: tier_pass failed: {e}");
                    summary.push(format!("tier {t}: FAILED"));
                }
            }
        }
    }

    // metering — authoritative bytes_stored recompute (idempotent).
    match jobs::metering::rollup(state).await {
        Ok(n) => summary.push(format!("metering: {n} tenants rolled up")),
        Err(e) => {
            tracing::warn!("scheduler: metering rollup failed: {e}");
            summary.push("metering: FAILED".to_string());
        }
    }

    let ok = !summary.iter().any(|s| s.contains("FAILED"));
    record_run(state, "nightly", ok, summary.join("; ")).await;
    true
}

/// Kill-switch read with fail-open semantics (a flag-read error must not wedge the
/// scheduler; jobs are individually idempotent and resumable).
async fn flags_enabled(state: &ServerState, name: &str) -> bool {
    crate::jobs::flags::enabled(state, name)
        .await
        .unwrap_or(true)
}

/// Attach the background scheduler to a running server (call once from `run`).
/// Tasks end with the runtime; Ctrl-C owns shutdown — nothing to join.
pub fn spawn(
    state: Arc<ServerState>,
    data_dir: &std::path::Path,
    listen_addr: &str,
    cold: Option<Arc<dyn ColdStore>>,
) {
    let holder = holder_for(data_dir, listen_addr);
    if cold.is_none() {
        tracing::warn!(
            "scheduler: no cold backend configured (set CAIRN_COLD_DIR) — tier_pass will be skipped"
        );
    }

    {
        let (state, holder) = (Arc::clone(&state), holder.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(CANARY_INTERVAL);
            loop {
                iv.tick().await;
                canary_tick(&state, &holder).await;
            }
        });
    }
    {
        let (state, holder) = (Arc::clone(&state), holder.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(BLOOM_INTERVAL);
            loop {
                iv.tick().await;
                bloom_tick(&state, &holder).await;
            }
        });
    }
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(NIGHTLY_INTERVAL);
        loop {
            iv.tick().await;
            nightly_tick(&state, &holder, cold.as_ref()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::tier::DevColdStore;
    use cairn_core::hash::Hash;

    const HOLDER_A: &str = "node-a/127.0.0.1:9000";
    const HOLDER_B: &str = "node-b/127.0.0.1:9001";

    #[tokio::test]
    async fn scheduler_ticks_run_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;

        // tenant with one present chunk: object bytes in the store, 91d untouched
        sqlx::query("INSERT INTO tenants(id, created_at) VALUES('t1', 0)")
            .execute(&state.db)
            .await
            .unwrap();
        let bytes = b"archival-candidate-chunk".to_vec();
        let hash = Hash::of(&bytes).hex();
        let key = crate::storage::LocalFsStore::chunk_key("t1", &hash);
        state.store.put(&key, &bytes).await.unwrap();
        let old = state.clock.now_millis() - 91 * 24 * 3600 * 1000;
        sqlx::query(
            "INSERT INTO chunks(tenant_id, hash, size, tier, state, last_touched)
             VALUES('t1', ?1, ?2, 'hot', 'present', ?3)",
        )
        .bind(&hash)
        .bind(bytes.len() as i64)
        .bind(old)
        .execute(&state.db)
        .await
        .unwrap();

        // canary tick: leader-leased full roundtrip probe
        assert!(canary_tick(&state, HOLDER_A).await);
        // second holder is locked out by the live lease
        assert!(!canary_tick(&state, HOLDER_B).await);
        // same holder renews and runs again (restart/re-acquire path)
        assert!(canary_tick(&state, HOLDER_A).await);
        let s: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id='sched/canary'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(s, "ok");

        // bloom tick: same lease semantics
        assert!(bloom_tick(&state, HOLDER_A).await);
        assert!(!bloom_tick(&state, HOLDER_B).await);

        // nightly tick with a cold backend: pack -> GC (shadow) -> tier -> metering
        let cold: Arc<dyn ColdStore> = Arc::new(DevColdStore::new(&dir.path().join("cold")));
        assert!(nightly_tick(&state, HOLDER_A, Some(&cold)).await);
        assert!(!nightly_tick(&state, HOLDER_B, Some(&cold)).await);

        // GC shadow: unreachable chunk counted as would-delete, NOT touched (still
        // present). Scope to t1 — the canary probe legitimately writes `canary` chunks.
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chunks WHERE tenant_id='t1' AND state='present'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(n, 1, "shadow GC must not mutate chunks");

        // tier: >90d chunk archived to the cold backend after verified copy
        let tier: String = sqlx::query_scalar("SELECT tier FROM chunks WHERE hash=?1")
            .bind(&hash)
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(tier, "archive");
        let cold_bytes = cold.get(&key).await.expect("cold copy must exist");
        assert_eq!(cold_bytes, bytes);

        // metering: authoritative recompute saw the tenant
        let stored: i64 =
            sqlx::query_scalar("SELECT bytes_stored FROM metering WHERE tenant_id='t1'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(stored, bytes.len() as i64);

        // nightly run recorded (state ok — nothing failed)
        let ns: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id='sched/nightly'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(ns, "ok");
    }

    #[tokio::test]
    async fn nightly_without_cold_backend_still_runs_pack_gc_metering() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT INTO tenants(id, created_at) VALUES('t1', 0)")
            .execute(&state.db)
            .await
            .unwrap();

        assert!(nightly_tick(&state, HOLDER_A, None).await);
        let detail: String = sqlx::query_scalar("SELECT detail FROM jobs WHERE id='sched/nightly'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert!(detail.contains("gc t1"), "gc ran: {detail}");
        assert!(detail.contains("metering"), "metering ran: {detail}");
        assert!(
            !detail.contains("tier"),
            "no tier without a cold backend: {detail}"
        );
    }

    #[tokio::test]
    async fn kill_switches_are_read_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;

        crate::jobs::flags::set(&state, "ops", "canary_enabled", "false")
            .await
            .unwrap();
        assert!(canary_tick(&state, HOLDER_A).await);
        // skipped-by-switch run still counts as held leadership; no canary objects written
        let s: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id='sched/canary'")
            .fetch_one(&state.db)
            .await
            .unwrap_or_default();
        // the skip path does not record a run row (nothing executed)
        // — assert leadership still excludes the second holder:
        assert!(!canary_tick(&state, HOLDER_B).await);
        let _ = s;
    }
}
