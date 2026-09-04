//! GC mark-sweep (SPEC §12): reachability walk (NOT refcounts) from
//! roots = refs ∪ trash tombstones ∪ in-flight sessions <7d ∞ legal holds.
//! 14-day grace; sweep only in non-shadow mode after the beta-month shadow-clean gate.
//! Every root/lookup is tenant-scoped (I3).

use std::collections::HashSet;

use crate::ServerState;
use cairn_core::clock::SystemClock;
use cairn_core::hash::Hash;
use cairn_core::manifest::Manifest;
use cairn_core::{CairnError, ErrorKind};
use prost::Message as _;
use sqlx::Row;

use super::bump_epoch;

const GRACE_MILLIS: i64 = 14 * 24 * 3600 * 1000;

/// Objects reachable from all roots (chunk hashes + manifest hashes).
pub async fn mark(state: &ServerState, tenant_id: &str) -> Result<HashSet<String>, CairnError> {
    let mut live = HashSet::new();

    // root 1: refs → commits → trees → manifests
    let refs: Vec<String> = sqlx::query("SELECT commit_hash FROM refs WHERE tenant_id=?1")
        .bind(tenant_id)
        .fetch_all(&state.db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    for commit_hex in refs {
        let commit_bytes = state
            .store
            .get(&crate::storage::LocalFsStore::object_key(
                tenant_id,
                &commit_hex,
            ))
            .await
            .unwrap_or_default();
        // commit layout: magic(4) ver(1) tree(32) parent(32) ...
        if commit_bytes.len() >= 37 {
            let tree_hex = Hash::from_slice(&commit_bytes[5..37])
                .map(|h| h.hex())
                .unwrap_or_default();
            if let Ok(tree_bytes) = state
                .store
                .get(&crate::storage::LocalFsStore::object_key(
                    tenant_id, &tree_hex,
                ))
                .await
            {
                // tree layout: magic(4) ver(1) u32 n | (u16 len, name, u8 kind, hash 32)*
                if tree_bytes.len() > 9 {
                    let n = u32::from_le_bytes([
                        tree_bytes[5],
                        tree_bytes[6],
                        tree_bytes[7],
                        tree_bytes[8],
                    ]) as usize;
                    let mut pos = 9;
                    for _ in 0..n {
                        if pos + 2 > tree_bytes.len() {
                            break;
                        }
                        let name_len =
                            u16::from_le_bytes([tree_bytes[pos], tree_bytes[pos + 1]]) as usize;
                        pos += 2 + name_len + 1;
                        if pos + 32 > tree_bytes.len() {
                            break;
                        }
                        let mh = Hash::from_slice(&tree_bytes[pos..pos + 32])
                            .map(|h| h.hex())
                            .unwrap_or_default();
                        pos += 32;
                        live.insert(mh.clone());
                        collect_manifest_chunks(state, tenant_id, &mh, &mut live).await;
                    }
                }
            }
        }
    }

    // root 2: trash tombstones protect their content until purge
    let trash: Vec<String> =
        sqlx::query("SELECT manifest_hash FROM trash WHERE tenant_id=?1 AND manifest_hash<>''")
            .bind(tenant_id)
            .fetch_all(&state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
    for mh in trash {
        live.insert(mh.clone());
        collect_manifest_chunks(state, tenant_id, &mh, &mut live).await;
    }

    // root 3: in-flight upload sessions <7d protect their chunk hashes
    let cutoff = state.clock.now_millis() - 7 * 24 * 3600 * 1000;
    let sessions: Vec<Vec<u8>> = sqlx::query(
        "SELECT chunk_hashes FROM upload_sessions WHERE tenant_id=?1 AND expires_at>?2 AND state<>'complete'",
    )
    .bind(tenant_id)
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .map_err(db_err)?
    .into_iter()
    .map(|r| r.get(0))
    .collect();
    for blob in sessions {
        for h in blob.split(|b| *b == b'\n') {
            if let Ok(s) = std::str::from_utf8(h) {
                if !s.is_empty() {
                    live.insert(s.to_string());
                }
            }
        }
    }

    // root 4: legal holds (∞) — protect the last known manifest of the held path
    let holds: Vec<String> = sqlx::query("SELECT path FROM legal_holds WHERE tenant_id=?1")
        .bind(tenant_id)
        .fetch_all(&state.db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    for path in holds {
        // Round 20 fix: the previous first query selected `manifest_hash`
        // from a subquery that only projected `op` — a prepare-time "no such
        // column" error that failed the ENTIRE gc_pass for any tenant with a
        // legal hold (silently, because shadow mode logged it). The op blob
        // carries the manifest hash; one query, decoded once.
        let Some(op_blob) = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT op FROM journal WHERE tenant_id=?1 AND path=?2 ORDER BY seq DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(&path)
        .fetch_optional(&state.db)
        .await
        .map_err(db_err)?
        else {
            continue;
        };
        if let Ok(op) = cairn_proto::pb::JournalOp::decode(op_blob.as_slice()) {
            if let Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) = op.op {
                collect_manifest_chunks(state, tenant_id, &u.manifest_hash, &mut live).await;
                live.insert(u.manifest_hash);
            }
        }
    }

    Ok(live)
}

async fn collect_manifest_chunks(
    state: &ServerState,
    tenant_id: &str,
    manifest_hex: &str,
    live: &mut HashSet<String>,
) {
    let bytes = match state
        .store
        .get(&crate::storage::LocalFsStore::object_key(
            tenant_id,
            manifest_hex,
        ))
        .await
    {
        Ok(b) => b,
        Err(_) => return,
    };
    let Ok(m) = Manifest::parse(&bytes) else {
        return;
    };
    // Fanout-safe (review round): `flatten()` returns NOTHING for Node manifests, which
    // marked every child chunk of a >8,192-chunk file as garbage and swept LIVE data.
    // Walk the tree recursively, fetching child manifest objects from the store.
    collect_manifest_tree(state, tenant_id, &m, live, 0).await;
}

/// Depth guard: a content-addressed manifest tree cannot cycle (a child's bytes hash to
/// a value only its parent can reference AFTER the child exists), but a corrupted/hostile
/// object store could feed us a manifest-shaped refusal-to-terminate. 8 levels cover
/// 8192^7 chunks — more bytes than exist. Bail loudly beyond it.
const MAX_MANIFEST_DEPTH: u32 = 8;

fn collect_manifest_tree<'a>(
    state: &'a ServerState,
    tenant_id: &'a str,
    m: &'a Manifest,
    live: &'a mut HashSet<String>,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_MANIFEST_DEPTH {
            tracing::error!(%tenant_id, depth, "manifest tree beyond MAX_MANIFEST_DEPTH — corrupt store? keeping chunks (fail-safe)");
            return;
        }
        match m {
            Manifest::Leaf { entries, .. } => {
                for e in entries {
                    live.insert(e.chunk_hash.hex());
                }
            }
            Manifest::Node { children, .. } => {
                for c in children {
                    let hex = c.hash.hex();
                    if let Ok(bytes) = state
                        .store
                        .get(&crate::storage::LocalFsStore::object_key(tenant_id, &hex))
                        .await
                    {
                        if let Ok(child) = Manifest::parse(&bytes) {
                            collect_manifest_tree(state, tenant_id, &child, live, depth + 1).await;
                        }
                    } else {
                        // unresolvable child: its chunks are UNREACHABLE to this walk —
                        // keep the child object itself alive so a later pass can retry
                        live.insert(hex);
                    }
                }
            }
        }
    })
}

/// GC pass. `shadow=true` → report only (beta gate); `false` → mark + tombstone + sweep.
/// Returns (would_delete | deleted, violations, scanned).
pub async fn gc_pass(
    state: &ServerState,
    tenant_id: &str,
    shadow: bool,
) -> Result<(u64, u64, u64), CairnError> {
    let _epoch = bump_epoch(state).await?; // epoch guard vs packing (§12)
    let now = state.clock.now_millis();
    let live = mark(state, tenant_id).await?;

    // scan all chunk rows for the tenant
    let rows: Vec<(String, String, i64)> =
        sqlx::query("SELECT hash, state, last_touched FROM chunks WHERE tenant_id=?1")
            .bind(tenant_id)
            .fetch_all(&state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
    let scanned = rows.len() as u64;

    let mut removed = 0u64;
    let mut violations = 0u64;
    for (hash, tier_state, last_touched) in &rows {
        let _reachable = live.contains(hash) || tier_state == "deleting";
        if live.contains(hash) {
            // (d): reachable objects must NEVER be swept
            if tier_state == "deleting" {
                violations += 1;
            }
            continue;
        }
        if *tier_state == "deleting" {
            // grace check: sweep only after 14d
            if now - last_touched >= GRACE_MILLIS && !shadow {
                let key = crate::storage::LocalFsStore::chunk_key(tenant_id, hash);
                state.store.delete(&key).await?;
                sqlx::query("DELETE FROM chunks WHERE tenant_id=?1 AND hash=?2")
                    .bind(tenant_id)
                    .bind(hash)
                    .execute(&state.db)
                    .await
                    .map_err(db_err)?;
                removed += 1;
            } else if shadow {
                removed += 1; // would-delete count in shadow mode
            }
        } else {
            // mark phase: unreachable → deleting (grace period starts)
            if !shadow {
                sqlx::query("UPDATE chunks SET state='deleting', last_touched=?3 WHERE tenant_id=?1 AND hash=?2")
                    .bind(tenant_id)
                    .bind(hash)
                    .bind(now)
                    .execute(&state.db)
                    .await
                    .map_err(db_err)?;
            }
            removed += 1;
        }
    }
    crate::db::audit(
        &state.db,
        &state.clock,
        tenant_id,
        "gc-job",
        if shadow { "gc.shadow" } else { "gc.sweep" },
        tenant_id,
        &format!("scanned={scanned} flagged={removed} violations={violations}"),
    )
    .await;
    Ok((removed, violations, scanned))
}

fn db_err(e: sqlx::Error) -> CairnError {
    CairnError::new(ErrorKind::Unavailable, format!("db: {e}"))
}

/// Clock helper re-exported for tests.
pub fn now(state: &ServerState) -> i64 {
    SystemClock::now_millis(&*state.clock)
}
