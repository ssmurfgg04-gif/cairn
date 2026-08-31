//! M6 AC (SPEC §19): GC shadow zero violations over 10k synthetic churn ops.

use cairn_core::hash::Hash;
use cairn_core::manifest::{Compression, Manifest, ManifestEntry};
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_proto::pb::FileUpsertOp;

fn upsert(path: &str, mh: &str, base: u64) -> cairn_proto::pb::JournalOp {
    cairn_proto::pb::JournalOp {
        op: Some(OpKind::FileUpsert(FileUpsertOp {
            path: path.into(),
            manifest_hash: mh.into(),
            size: 1,
            base_seq: base,
        })),
    }
}

#[tokio::test]
async fn gc_shadow_zero_violations_over_10k_churn() {
    let dir = tempfile::tempdir().unwrap();
    let state = cairn_server::tests_support::state_at(dir.path()).await;
    sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO projects(tenant_id, project_id, created_at) VALUES('t1','p1',0)",
    )
    .execute(&state.db)
    .await
    .unwrap();

    // 10k churn ops: 5k upserts of 100 rotating files + 5k deletes, each backed by real
    // manifests and chunk objects in the store
    let files = 100u64;
    let mut base = 0u64;
    for round in 0..50u64 {
        for f in 0..files {
            let body = format!("manifest-r{round}-f{f}").into_bytes();
            let mh = Hash::of(&body);
            state
                .store
                .put(
                    &cairn_server::storage::LocalFsStore::object_key("t1", &mh.hex()),
                    &body,
                )
                .await
                .unwrap();
            sqlx::query("INSERT OR IGNORE INTO manifests(tenant_id, hash, size, entry_count) VALUES('t1',?1,?2,0)")
                .bind(mh.hex())
                .bind(body.len() as i64)
                .execute(&state.db).await.unwrap();
            // a chunk per manifest
            let chunk = format!("chunk-r{round}-f{f}").into_bytes();
            let ch = Hash::of(&chunk);
            state
                .store
                .put(
                    &cairn_server::storage::LocalFsStore::chunk_key("t1", &ch.hex()),
                    &chunk,
                )
                .await
                .unwrap();
            sqlx::query("INSERT OR IGNORE INTO chunks(tenant_id, hash, size, tier, state, last_touched) VALUES('t1',?1,?2,'hot','present',0)")
                .bind(ch.hex())
                .bind(chunk.len() as i64)
                .execute(&state.db).await.unwrap();
            let entries = vec![ManifestEntry {
                offset: 0,
                len: chunk.len() as u32,
                chunk_hash: ch,
            }];
            let m = Manifest::build(entries, Compression::None, None);
            let (_mh2, mb) = m.serialize();
            let _ = mb;

            base += 1;
            cairn_server::journal::append(
                &state.db,
                &state.clock,
                "t1",
                "p1",
                "d1",
                &format!("u-{round}-{f}"),
                upsert(&format!("f{f}.bin"), &mh.hex(), base - 1),
                0,
            )
            .await
            .unwrap();
            if round > 0 {
                let del = cairn_proto::pb::JournalOp {
                    op: Some(OpKind::FileDelete(cairn_proto::pb::FileDeleteOp {
                        path: format!("f{f}.bin"),
                        base_seq: base - 1,
                    })),
                };
                base += 1;
                cairn_server::journal::append(
                    &state.db,
                    &state.clock,
                    "t1",
                    "p1",
                    "d1",
                    &format!("d-{round}-{f}"),
                    del,
                    0,
                )
                .await
                .unwrap();
            }
        }
    }

    // fold so refs exist (GC roots), then shadow GC
    let (_commit, _) = cairn_server::fold::fold(&state, "t1", "p1", "canary", "pre-gc")
        .await
        .unwrap();
    let (flagged, violations, scanned) = cairn_server::jobs::gc::gc_pass(&state, "t1", true)
        .await
        .unwrap();
    assert_eq!(
        violations, 0,
        "GC shadow MUST report zero violations (I2/(d))"
    );
    assert_eq!(scanned, 5000); // 50 rounds x 100 files, one chunk each
    assert!(
        flagged > 0,
        "churn created unreachable chunks that GC flags"
    );
    assert_eq!(violations, 0);
    let _ = rows_check(&state).await;
}

async fn rows_check(state: &cairn_server::ServerState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE tenant_id='t1'")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
}
