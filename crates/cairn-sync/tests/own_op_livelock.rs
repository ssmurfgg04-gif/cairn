//! Regression test (punch #5 round): replaying OWN-device journal entries in the pull
//! phase overwrote the row's local stat fields (mtime/size from the scan) with
//! journal-level values (server_ts). Once the reconcile sweep started comparing
//! row.stat to file.stat, that phantom drift re-dirtied the file every sweep →
//! re-push → new journal entry → replay → … (observed live: 1302 journal entries for
//! a 10-file corpus). The fix: the pull phase skips own-device ops — own ops are
//! folded locally by the push path (mark_synced). This test pins that behavior.

use std::sync::Arc;

use async_trait::async_trait;

use cairn_core::clock::WallClock;
use cairn_core::compress::DictRegistry;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::{JournalOp, UploadReceipt};
use cairn_store::state::LocalState;
use cairn_store::{Cas, HeaderCache, Outbox, Store};
use cairn_sync::aimd::Gate;
use cairn_sync::engine::Engine;
use cairn_sync::plane::{upsert_op, CompleteOut, Entry, Plane, Session};

/// Journal with fixed entries; upload/session paths are unused in this test.
struct ReplayPlane {
    entries: std::sync::Mutex<Vec<Entry>>,
}

#[async_trait]
impl Plane for ReplayPlane {
    async fn batch_exists(&self, _t: &str, _h: &[String]) -> Result<Vec<String>, CairnError> {
        Ok(vec![])
    }
    async fn create_session(
        &self,
        _t: &str,
        _d: &str,
        _p: &str,
        _m: &[String],
    ) -> Result<Session, CairnError> {
        Ok(Session {
            id: "s".into(),
            puts: vec![],
            expires_at: 0,
        })
    }
    async fn complete(&self, _s: &str, _r: &[UploadReceipt]) -> Result<CompleteOut, CairnError> {
        Ok(CompleteOut {
            verified: vec![],
            rejected: vec![],
        })
    }
    async fn put_presigned(&self, _u: &str, _b: &[u8], _c: &str) -> Result<(), CairnError> {
        Err(CairnError::new(ErrorKind::Internal, "unused"))
    }
    async fn put_manifest(&self, _t: &str, _h: &str, _b: &[u8]) -> Result<(), CairnError> {
        // the W4 regression pushes a real manifest before its short-circuit;
        // the livelock tests never reach this call
        Ok(())
    }
    async fn get_manifest(&self, _t: &str, _h: &str) -> Result<Vec<u8>, CairnError> {
        Err(CairnError::new(ErrorKind::Internal, "unused"))
    }
    async fn fetch_object(&self, _t: &str, _h: &str) -> Result<Vec<u8>, CairnError> {
        Err(CairnError::new(ErrorKind::Internal, "unused"))
    }
    async fn append(
        &self,
        _t: &str,
        _p: &str,
        _d: &str,
        _r: &str,
        _op: JournalOp,
        _l: u64,
    ) -> Result<(u64, bool), CairnError> {
        Err(CairnError::new(ErrorKind::Internal, "unused"))
    }
    async fn fetch_batch(
        &self,
        _t: &str,
        _p: &str,
        after: u64,
        _l: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect())
    }
}

fn engine_with(entries: Vec<Entry>) -> (tempfile::TempDir, Engine) {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path(), Arc::new(WallClock)).unwrap();
    let conn = store.conn_handle();
    let cas = Cas::open(&store.root().join("blobs"), conn.clone()).unwrap();
    let engine = Engine {
        tenant_id: "t1".into(),
        project_id: "p1".into(),
        device_id: "dev-A".into(),
        local_ns: "p1".into(),
        author_id: "dev-A".into(),
        store,
        cas,
        outbox: Outbox::new(conn.clone()),
        headers: HeaderCache::new(conn),
        plane: Arc::new(ReplayPlane {
            entries: std::sync::Mutex::new(entries),
        }),
        dicts: DictRegistry::new(),
        gate: Gate::default(),
    };
    (home, engine)
}

#[tokio::test]
async fn own_device_replay_must_not_rewrite_row_stat() {
    // Local row as the scan wrote it: mtime = the FILE's mtime (not server_ts).
    let (_h, engine) = engine_with(vec![Entry {
        seq: 1,
        device_id: "dev-A".into(), // OUR OWN op coming back in the pull
        op: upsert_op("a.mov", "aa", 10, 0),
        server_ts: 1759300000,
    }]);
    engine
        .store
        .put_file(&cairn_store::FileRow {
            path: "a.mov".into(),
            project_id: "p1".into(),
            manifest_hash: Some("aa".into()),
            size: 10,
            mode: "file".into(),
            mtime: 1758123456789, // scanned local mtime — must survive the pull
            local_state: LocalState::Synced.as_str().into(),
        })
        .unwrap();

    let stats = engine.sync_pass().await.unwrap();
    assert_eq!(stats.applied_entries, 0, "own-device op must be skipped");

    let row = engine.store.get_file("p1", "a.mov").unwrap();
    assert_eq!(
        row.mtime, 1758123456789,
        "pull replay must NOT overwrite the scanned mtime (livelock regression)"
    );
    assert_eq!(row.local_state, LocalState::Synced.as_str());

    // cursor still advanced past the skipped op (no infinite refetch)
    assert_eq!(engine.store.get_cursor("dev-A", "p1"), 1);
}

#[tokio::test]
async fn remote_device_replay_still_applies() {
    let (_h, engine) = engine_with(vec![Entry {
        seq: 2,
        device_id: "dev-B".into(), // a REMOTE device's op — must apply
        op: upsert_op("b.mov", "bb", 20, 0),
        server_ts: 1759300001,
    }]);

    let stats = engine.sync_pass().await.unwrap();
    assert_eq!(stats.applied_entries, 1, "remote ops still apply");
    let row = engine.store.get_file("p1", "b.mov").unwrap();
    assert_eq!(row.manifest_hash.as_deref(), Some("bb"));
    // fresh remote row → placeholder for hydration
    assert_eq!(row.local_state, LocalState::Placeholder.as_str());
}

// ---------- stat-only drift convergence (round 18, the Windows-matrix W4 catch) ----------
//
// The observed shape: A holds v1 (synced, real disk stat); something touches
// the file's mtime WITHOUT changing content (Windows-matrix run 33891087316,
// one firing in ~7 runs); B upserts v2. The §7.1 guard refuses the remote
// (stat drifted) and re-dirties — correct data-safety — but the entry is
// CONSUMED: without re-delivery the re-assert of identical bytes would clear
// the fork and leave A at v1 forever (silent divergence, no conflict copy,
// no warning — W4 RED after 300 s). The short-circuit resolves it: identical
// content ⇒ refresh the row stat, re-pin replay to the fork point, and the
// very same pass re-delivers + applies v2.

/// The REAL manifest of `bytes`, exactly as process_file computes it — the
/// short-circuit compares it against the row's recorded manifest, so a fake
/// string would never match.
fn real_manifest(bytes: &[u8]) -> String {
    let sh = cairn_core::chunker::StreamHash::compute(bytes);
    let entries: Vec<cairn_core::manifest::ManifestEntry> = sh
        .spans
        .iter()
        .zip(sh.chunk_hashes.iter())
        .map(|(s, h)| cairn_core::manifest::ManifestEntry {
            offset: s.offset,
            len: s.len,
            chunk_hash: *h,
        })
        .collect();
    let built = cairn_core::manifest::Manifest::build_tree_with_transform(
        entries,
        cairn_core::compress::policy_for("a.mov"),
        None,
        cairn_core::normalize::Transform::None,
    );
    let (hash, _bytes) = built.manifest.serialize();
    hash.hex()
}

#[tokio::test]
async fn stat_only_drift_converges_via_fork_repin() {
    let v1 = real_manifest(b"seed");
    let (home, engine) = engine_with(vec![
        // our own v1 push, replayed in the pull (skipped for own-device ops)
        Entry {
            seq: 5,
            device_id: "dev-A".into(),
            op: upsert_op("a.mov", &v1, 4, 0),
            server_ts: 1759300000,
        },
        // B's v2 (content differs; only the string matters for the row test)
        Entry {
            seq: 6,
            device_id: "dev-B".into(),
            op: upsert_op("a.mov", "v2-remote", 9, 0),
            server_ts: 1759300001,
        },
    ]);
    cairn_sync::workspace::set_workspace(&engine.store, "p1", home.path()).unwrap();
    let f = home.path().join("a.mov");
    std::fs::write(&f, b"seed").unwrap();
    let st = std::fs::metadata(&f).unwrap();
    engine
        .store
        .put_file(&cairn_store::FileRow {
            path: "a.mov".into(),
            project_id: "p1".into(),
            manifest_hash: Some(v1.clone()),
            size: st.len(),
            mode: "file".into(),
            mtime: cairn_sync::scan::mtime_millis(&st),
            local_state: LocalState::Synced.as_str().into(),
        })
        .unwrap();

    // the stat-only touch: mtime moves, content does NOT
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .append(true)
        .open(&f)
        .unwrap()
        .set_modified(later)
        .unwrap();

    // PASS 1: the guard correctly refuses v2 (stat drifted) and re-dirties
    let _ = engine.sync_pass().await.unwrap();
    let row = engine.store.get_file("p1", "a.mov").unwrap();
    assert_eq!(row.local_state, LocalState::Dirty.as_str(), "guard fired");
    assert_eq!(row.manifest_hash.as_deref(), Some(v1.as_str()));
    assert_eq!(
        cairn_sync::apply::fork_seq(&engine.store, "p1", "a.mov"),
        Some(6),
        "fork pinned at the refused entry"
    );

    // PASS 2: the short-circuit — identical content ⇒ stat refresh + fork
    // consumed + replay re-pinned — and the re-delivered v2 applies in the
    // SAME pass (the fake plane's append errors, so the OLD re-assert path
    // would fail this unwrap instead of converging)
    engine.sync_pass().await.unwrap();
    let row = engine.store.get_file("p1", "a.mov").unwrap();
    assert_eq!(
        row.local_state,
        LocalState::Placeholder.as_str(),
        "refused remote re-delivered and applied"
    );
    assert_eq!(row.manifest_hash.as_deref(), Some("v2-remote"));
    assert_eq!(engine.store.get_cursor("dev-A", "p1"), 6);
    assert_eq!(
        cairn_sync::apply::fork_seq(&engine.store, "p1", "a.mov"),
        None,
        "fork consumed"
    );
    // disk content untouched by the engine (hydration materializes v2 later)
    assert_eq!(std::fs::read(&f).unwrap(), b"seed");
}
