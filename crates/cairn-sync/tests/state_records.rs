//! Integration tests (ADR-0031 Phase 1): TWO real engines over ONE shared
//! in-memory journal — the `state_record` sync contract without an HTTP
//! server. The plane extends the `ReplayPlane` idea from
//! `own_op_livelock.rs`: `append` records entries in a shared Vec with
//! server-assigned seqs + request_id dedupe (the two server behaviors the
//! engine relies on), and `fetch_batch` serves entries beyond each engine's
//! own cursor — cursor replay does the convergence, exactly as in production.
//!
//! Records need no materialization, so the convergence driver is the sim's
//! `pass()` (cairn-sim/src/world.rs) minus `materialize_missing`: each
//! engine runs `sync_pass()` (flush outbox → pull) a couple of rounds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use cairn_core::clock::{SystemClock as _, WallClock};
use cairn_core::compress::DictRegistry;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::{JournalOp, UploadReceipt};
use cairn_store::{Cas, HeaderCache, Outbox, Store};
use cairn_sync::aimd::Gate;
use cairn_sync::engine::Engine;
use cairn_sync::plane::{CompleteOut, Entry, Plane, Session};
use cairn_sync::state_records::{enqueue_state_record, PublishParams};

/// One shared in-memory journal for N engines (the "server", minus storage).
struct SharedJournal {
    entries: std::sync::Mutex<Vec<Entry>>,
    seen_requests: std::sync::Mutex<std::collections::HashSet<String>>,
    next_seq: AtomicU64,
}

impl SharedJournal {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            entries: std::sync::Mutex::new(Vec::new()),
            seen_requests: std::sync::Mutex::new(std::collections::HashSet::new()),
            next_seq: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl Plane for SharedJournal {
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
        device: &str,
        request_id: &str,
        op: JournalOp,
        _l: u64,
    ) -> Result<(u64, bool), CairnError> {
        // server contract (SPEC §7.1 idempotency): request_id dedupe — a
        // crash-resume resend must not duplicate the journal entry
        if !self
            .seen_requests
            .lock()
            .unwrap()
            .insert(request_id.to_string())
        {
            return Ok((0, true));
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.entries.lock().unwrap().push(Entry {
            seq,
            device_id: device.to_string(),
            op,
            server_ts: WallClock.now_millis(),
        });
        Ok((seq, false))
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

fn engine_with(journal: Arc<SharedJournal>, author: &str) -> (tempfile::TempDir, Engine) {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path(), Arc::new(WallClock)).unwrap();
    let conn = store.conn_handle();
    let cas = Cas::open(&store.root().join("blobs"), conn.clone()).unwrap();
    let engine = Engine {
        tenant_id: "t1".into(),
        project_id: "p1".into(),
        device_id: author.into(),
        local_ns: "p1".into(),
        author_id: author.into(),
        store,
        cas,
        outbox: Outbox::new(conn.clone()),
        headers: HeaderCache::new(conn),
        plane: journal,
        dicts: DictRegistry::new(),
        gate: Gate::default(),
    };
    (home, engine)
}

/// Publish through the public enqueue path (validate → local apply → durable
/// outbox) — the daemon-surface shape; the next `sync_pass` carries it.
fn publish(engine: &Engine, family: &str, key: &str, payload: &[u8], ts_ms: i64, tombstone: bool) {
    enqueue_state_record(
        &engine.store,
        PublishParams {
            tenant_id: &engine.tenant_id,
            project_id: &engine.project_id,
            local_ns: &engine.local_ns,
            device_id: &engine.author_id,
            family,
            key,
            payload,
            ts_ms,
            tombstone,
        },
    )
    .unwrap();
}

/// The canonical convergence driver: each engine flushes its outbox then
/// pulls (records need no materialization). A couple of rounds absorbs the
/// either-order first-pass skew.
async fn converge(a: &Engine, b: &Engine, rounds: usize) {
    for _ in 0..rounds {
        a.sync_pass().await.unwrap();
        b.sync_pass().await.unwrap();
    }
}

#[tokio::test]
async fn member_record_converges_across_devices() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(journal, "dev-B");

    // dev-A enrolls dev-B: a member record keyed by the member's device id
    let payload: &[u8] =
        br#"{"device_id":"dev-B","name":"Bob","role":"colorist","added_at_ms":42,"added_by":"dev-A"}"#;
    publish(&a, "member", "dev-B", payload, 100, false);
    // own-op suppression: the publishing device holds the record from the
    // local apply at publish time — replay will never re-deliver it
    assert_eq!(a.store.list_state_records("p1", "member").len(), 1);
    assert_eq!(b.store.list_state_records("p1", "member").len(), 0);

    converge(&a, &b, 2).await;

    let ra = a.store.list_state_records("p1", "member");
    let rb = b.store.list_state_records("p1", "member");
    assert_eq!(ra.len(), 1);
    assert_eq!(rb.len(), 1, "dev-B replayed dev-A's member record");
    assert_eq!(rb[0].key, "dev-B");
    assert_eq!(
        rb[0].device_id, "dev-A",
        "authorship is the journal entry's device"
    );
    assert_eq!(rb[0].ts_ms, 100);
    assert!(!rb[0].tombstone);
    assert_eq!(
        ra[0].payload, rb[0].payload,
        "identical payload bytes on both stores"
    );
}

#[tokio::test]
async fn lww_conflict_converges_to_one_winner() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(journal, "dev-B");

    // offline concurrent edit of ONE roster entry: both devices write the
    // same key with different ts_ms — LWW must pick ONE winner everywhere
    publish(
        &a,
        "member",
        "dev-C",
        br#"{"device_id":"dev-C","role":"editor"}"#,
        100,
        false,
    );
    publish(
        &b,
        "member",
        "dev-C",
        br#"{"device_id":"dev-C","role":"owner"}"#,
        200,
        false,
    );
    assert_eq!(a.store.list_state_records("p1", "member").len(), 1);
    assert_eq!(b.store.list_state_records("p1", "member").len(), 1);

    converge(&a, &b, 2).await;

    let ra = a.store.list_state_records("p1", "member");
    let rb = b.store.list_state_records("p1", "member");
    assert_eq!(ra.len(), 1, "LWW: one register per key");
    assert_eq!(rb.len(), 1);
    // the winner is the higher (ts_ms, device_id) rank on BOTH stores
    assert_eq!((ra[0].ts_ms, ra[0].device_id.as_str()), (200, "dev-B"));
    assert_eq!((rb[0].ts_ms, rb[0].device_id.as_str()), (200, "dev-B"));
    assert_eq!(ra[0].payload, rb[0].payload);
    assert!(
        String::from_utf8_lossy(&ra[0].payload).contains("owner"),
        "the ts=200 write won; the stale ts=100 write never resurrects"
    );
}

#[tokio::test]
async fn audit_union_converges() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(journal, "dev-B");

    // each device ledgered a DISTINCT decision — append family: the union
    // grows (never LWW; the audit log only loses facts if it merged)
    publish(
        &a,
        "audit",
        "cid-a",
        br#"{"action":"dash/attach","allowed":true}"#,
        100,
        false,
    );
    publish(
        &b,
        "audit",
        "cid-b",
        br#"{"action":"ctl/detach-root","allowed":false}"#,
        101,
        false,
    );

    converge(&a, &b, 2).await;

    let ids_of = |rows: &[cairn_store::StateRecordRow]| -> Vec<String> {
        let mut ids: Vec<String> = rows.iter().map(|r| r.record_id.clone()).collect();
        ids.sort();
        ids
    };
    let ra = a.store.list_state_records("p1", "audit");
    let rb = b.store.list_state_records("p1", "audit");
    assert_eq!(ra.len(), 2, "dev-A holds both decisions (union)");
    assert_eq!(rb.len(), 2, "dev-B holds both decisions (union)");
    assert_eq!(
        ids_of(&ra),
        ids_of(&rb),
        "content ids derive identically from the same payloads"
    );
    assert!(ra
        .iter()
        .any(|r| String::from_utf8_lossy(&r.payload).contains("dash/attach")));
    assert!(ra
        .iter()
        .any(|r| String::from_utf8_lossy(&r.payload).contains("ctl/detach-root")));
}

#[tokio::test]
async fn link_tombstone_propagates() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(journal, "dev-B");

    // dev-A mints a guest link, and it rides to dev-B
    publish(
        &a,
        "review_link",
        "tok-1",
        br#"{"token":"tok-1","role":"viewer","created_at":100}"#,
        100,
        false,
    );
    converge(&a, &b, 1).await;
    let link = b
        .store
        .get_state_record("p1", "review_link", "tok-1")
        .expect("link synced to dev-B");
    assert!(!link.tombstone);

    // dev-A revokes it — the tombstone (a later LWW write on the same key)
    // must ride to every device and win there too
    publish(
        &a,
        "review_link",
        "tok-1",
        br#"{"token":"tok-1","revoked":true}"#,
        200,
        true,
    );
    converge(&a, &b, 2).await;

    for (label, row) in [
        (
            "dev-A",
            a.store.get_state_record("p1", "review_link", "tok-1"),
        ),
        (
            "dev-B",
            b.store.get_state_record("p1", "review_link", "tok-1"),
        ),
    ] {
        let r = row.unwrap_or_else(|| panic!("{label}: link row missing"));
        assert!(r.tombstone, "{label}: revocation tombstone propagated");
        assert_eq!(r.ts_ms, 200, "{label}: the revoke is the winning LWW write");
    }
}
