//! Integration tests (CONTRACT-DEBT #1): the conflict auto-offer lifecycle —
//! TWO real engines over ONE shared in-memory journal that ALSO emulates the
//! server's §7.1 conflict rule, so a REAL CONFLICT fires through the engine's
//! CONFLICT arm (`engine.rs send_outbox_entry`) and the merge-offer machinery
//! runs end to end: conflict copy → `create_offer_if_mergeable` →
//! accept/decline.
//!
//! The plane mirrors the server contract the engine relies on
//! (`cairn-server/src/journal.rs`):
//! * server-assigned seqs + `request_id` dedupe (idempotency, SPEC §7.1);
//! * the conflict rule: a non-state-record op is refused with CONFLICT iff an
//!   entry for the same path with seq > op.base_seq exists from a DIFFERENT
//!   device (state records are exempt, ADR-0031). Rejections are recorded;
//! * real object storage: chunk PUTs (`put_presigned`), manifests
//!   (`put_manifest`) and the hydration GETs (`get_manifest`/`fetch_object`)
//!   are backed by one content-addressed map — `create_offer_if_mergeable`
//!   hydrates base+theirs through it exactly as it would through the bucket.
//!
//! The convergence driver is the sim's `pass()` minus materialization: each
//! engine runs `sync_pass()` a couple of rounds. No sleeps anywhere — the
//! conflict fires deterministically because device B pushes its edit BEFORE
//! pulling device A's winner (push_phase precedes pull_phase).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cairn_core::clock::{SystemClock as _, WallClock};
use cairn_core::compress::DictRegistry;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_proto::pb::{JournalOp, UploadReceipt};
use cairn_store::state::LocalState;
use cairn_store::{Cas, FileRow, HeaderCache, Outbox, Store};
use cairn_sync::aimd::Gate;
use cairn_sync::engine::Engine;
use cairn_sync::plane::{CompleteOut, Entry, Plane, Session};
use cairn_tl::model::*;
use cairn_tl::rational::Rational;
use sha2::{Digest as _, Sha256};

// ---------------------------------------------------------------------------
// The shared "server": journal + §7.1 conflict rule + object storage
// ---------------------------------------------------------------------------

/// Primary path of an op (the conflict-rule key; the server's `paths_of`).
fn op_primary_path(op: &JournalOp) -> Option<String> {
    match op.op.as_ref() {
        Some(OpKind::FileUpsert(u)) => Some(u.path.clone()),
        Some(OpKind::FileDelete(d)) => Some(d.path.clone()),
        Some(OpKind::Rename(r)) => Some(r.old_path.clone()),
        Some(OpKind::LeaseEvent(l)) => Some(l.path.clone()),
        Some(OpKind::StateRecord(sr)) => {
            Some(cairn_core::pathutil::state_record_path(&sr.family, &sr.key))
        }
        None => None,
    }
}

/// The op's declared lineage (0 for lease events / state records).
fn op_base_seq(op: &JournalOp) -> u64 {
    match op.op.as_ref() {
        Some(OpKind::FileUpsert(u)) => u.base_seq,
        Some(OpKind::FileDelete(d)) => d.base_seq,
        Some(OpKind::Rename(r)) => r.base_seq,
        _ => 0,
    }
}

/// One shared in-memory journal + object store for N engines (the "server").
struct SharedJournal {
    entries: Mutex<Vec<Entry>>,
    /// request_id -> assigned seq (accepted appends only — a CONFLICTED
    /// append is rolled back on the real server and must NOT dedupe later).
    seen_requests: Mutex<HashMap<String, u64>>,
    /// Content-addressed objects: compressed chunks AND manifests, keyed by
    /// hash hex, stored exactly as uploaded (the bucket's honesty).
    objects: Mutex<HashMap<String, Vec<u8>>>,
    next_seq: AtomicU64,
    /// Every CONFLICT-refused append, as "device path" (test assertion fuel).
    rejected: Mutex<Vec<String>>,
}

impl SharedJournal {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(Vec::new()),
            seen_requests: Mutex::new(HashMap::new()),
            objects: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            rejected: Mutex::new(Vec::new()),
        })
    }

    /// Journal entries for one path from one device above `after_seq`:
    /// (seq, manifest) for FileUpserts — the acceptance-count probe.
    fn upserts_for(&self, path: &str, device: &str, after_seq: u64) -> Vec<(u64, String)> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq > after_seq && e.device_id == device)
            .filter_map(|e| match e.op.op.as_ref() {
                Some(OpKind::FileUpsert(u)) if u.path == path => {
                    Some((e.seq, u.manifest_hash.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The manifest hash an accepted FileUpsert carries at `seq`.
    fn upsert_manifest_at(&self, seq: u64) -> Option<String> {
        self.entries.lock().unwrap().iter().find_map(|e| {
            if e.seq != seq {
                return None;
            }
            match e.op.op.as_ref() {
                Some(OpKind::FileUpsert(u)) => Some(u.manifest_hash.clone()),
                _ => None,
            }
        })
    }

    fn rejections(&self) -> Vec<String> {
        self.rejected.lock().unwrap().clone()
    }
}

#[async_trait]
impl Plane for SharedJournal {
    async fn batch_exists(&self, _t: &str, h: &[String]) -> Result<Vec<String>, CairnError> {
        // server contract (services.rs upload::batch_exists → wire `missing`):
        // the hashes the CLIENT must upload are the ones the bucket LACKS
        let objects = self.objects.lock().unwrap();
        Ok(h.iter()
            .filter(|hex| !objects.contains_key((*hex).as_str()))
            .cloned()
            .collect())
    }
    async fn create_session(
        &self,
        _t: &str,
        _d: &str,
        _p: &str,
        missing: &[String],
    ) -> Result<Session, CairnError> {
        Ok(Session {
            id: "s".into(),
            puts: missing
                .iter()
                .map(|h| (h.clone(), format!("sim://{h}")))
                .collect(),
            expires_at: 0,
        })
    }
    async fn complete(&self, _s: &str, r: &[UploadReceipt]) -> Result<CompleteOut, CairnError> {
        Ok(CompleteOut {
            verified: r.iter().map(|x| x.chunk_hash.clone()).collect(),
            rejected: vec![],
        })
    }
    async fn put_presigned(&self, url: &str, b: &[u8], c: &str) -> Result<(), CairnError> {
        let hex = url
            .strip_prefix("sim://")
            .ok_or_else(|| CairnError::new(ErrorKind::Internal, "bad presigned url"))?;
        let got = hex::encode(Sha256::digest(b));
        if got != c {
            return Err(CairnError::new(
                ErrorKind::ChecksumMismatch,
                "presigned PUT checksum mismatch",
            ));
        }
        self.objects
            .lock()
            .unwrap()
            .insert(hex.to_string(), b.to_vec());
        Ok(())
    }
    async fn put_manifest(&self, _t: &str, h: &str, b: &[u8]) -> Result<(), CairnError> {
        self.objects
            .lock()
            .unwrap()
            .insert(h.to_string(), b.to_vec());
        Ok(())
    }
    async fn get_manifest(&self, _t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
        self.objects
            .lock()
            .unwrap()
            .get(h)
            .cloned()
            .ok_or_else(|| CairnError::new(ErrorKind::NotFound, format!("manifest {h}")))
    }
    async fn fetch_object(&self, _t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
        self.objects
            .lock()
            .unwrap()
            .get(h)
            .cloned()
            .ok_or_else(|| CairnError::new(ErrorKind::NotFound, format!("object {h}")))
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
        // idempotency first (SPEC §7.1): an ACCEPTED request_id replays its seq
        if let Some(seq) = self.seen_requests.lock().unwrap().get(request_id) {
            return Ok((*seq, true));
        }
        // the §7.1 conflict rule, implemented exactly (journal.rs): accepted
        // iff NO entry from a DIFFERENT device has seq > base_seq for the same
        // path; state records are exempt (ADR-0031). A rejection is NOT
        // recorded as accepted (the real server rolls the tx back).
        if !matches!(op.op.as_ref(), Some(OpKind::StateRecord(_))) {
            let path = op_primary_path(&op).unwrap_or_default();
            let base = op_base_seq(&op);
            let conflicting = self.entries.lock().unwrap().iter().any(|e| {
                e.seq > base
                    && e.device_id != device
                    && op_primary_path(&e.op).as_deref() == Some(path.as_str())
            });
            if conflicting {
                self.rejected
                    .lock()
                    .unwrap()
                    .push(format!("{device} {path}"));
                return Err(CairnError::new(
                    ErrorKind::Conflict,
                    format!("path {path}: a different device has seq>{base}; upsert diverged"),
                ));
            }
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen_requests
            .lock()
            .unwrap()
            .insert(request_id.to_string(), seq);
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
        limit: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq > after)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Engines + the two-device conflict recipe
// ---------------------------------------------------------------------------

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

/// Project-relative -> absolute workspace path (the engine's `rooted` is
/// pub(crate); tests resolve through the same `workspace_dir`).
fn ws_file(engine: &Engine, path: &str) -> std::path::PathBuf {
    let full = cairn_sync::workspace_dir(&engine.store, &engine.local_ns).join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    full
}

/// Write the bytes + mark the row Dirty (the scan's discovery shape).
fn write_and_dirty(engine: &Engine, path: &str, bytes: &[u8]) {
    let full = ws_file(engine, path);
    std::fs::write(&full, bytes).unwrap();
    let meta = std::fs::metadata(&full).unwrap();
    engine
        .store
        .put_file(&FileRow {
            path: path.into(),
            project_id: engine.local_ns.clone(),
            manifest_hash: None,
            size: bytes.len() as u64,
            mode: "file".into(),
            mtime: cairn_sync::scan::mtime_millis(&meta),
            local_state: LocalState::Dirty.as_str().into(),
        })
        .unwrap();
}

/// Conflict-copy files left in a device's workspace (the §7.1 naming rule's
/// "name (conflict — ..." prefix; the date/device tail is wall-clock).
fn conflict_copies(engine: &Engine, stem: &str) -> Vec<std::path::PathBuf> {
    let ws = cairn_sync::workspace_dir(&engine.store, &engine.local_ns);
    let prefix = format!("{stem} (conflict");
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(&ws)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// OTIO fixtures (helper shapes from crates/cairn-tl/tests/two_editors.rs)
// ---------------------------------------------------------------------------

fn tv(v: i128, r: i128) -> TimeVal {
    TimeVal {
        value: Rational::new(v, 1).unwrap(),
        rate: Rational::new(r, 1).unwrap(),
    }
}

fn clip(name: &str, url: &str, start: i128, dur: i128) -> Element {
    let mut c = Element::leaf(Kind::Clip, name);
    c.media = Some(MediaRef::single(
        MediaKind::External,
        String::new(),
        Some(url.into()),
    ));
    c.source_range = Some(TimeRange {
        start: tv(start, 24),
        duration: tv(dur, 24),
    });
    c
}

fn doc(tracks: Vec<(&str, Vec<Element>)>) -> Timeline {
    let track_els: Vec<Element> = tracks
        .into_iter()
        .map(|(name, items)| Element::container(Kind::Track(TrackKind::Video), name, items))
        .collect();
    Timeline {
        name: "session".into(),
        global_start_time: None,
        metadata: JsonMap::new(),
        tracks: Element::container(Kind::Stack, "tracks", track_els),
        extra: JsonMap::new(),
    }
}

fn stamp(tl: &mut Timeline) {
    tl.walk_mut(|e| {
        if e.cairn_uuid().is_none() {
            let id = uuid::Uuid::now_v7().to_string();
            e.stamp_uuid(&id);
        }
    });
}

/// Canonical OTIO bytes for a timeline (what the engine chunk-stores).
fn otio(t: &Timeline) -> Vec<u8> {
    cairn_tl::canon::serialize(t).unwrap().into_bytes()
}

/// One hero clip, 96 frames — the two_editors semantic-merge stage.
fn hero_base() -> Timeline {
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    base
}

/// The deterministic two-device timeline conflict:
/// 1. A authors the base and syncs (journal seq 1);
/// 2. B pulls the base (cursor 1, no local edit);
/// 3. A re-cuts the HEAD of Hero and syncs (seq 2) while B is offline;
/// 4. B re-cuts the TAIL offline, then syncs: B's append claims base_seq 1
///    while A's seq-2 entry for the same path is already in the journal from
///    a DIFFERENT device → the §7.1 rule refuses it → the engine's CONFLICT
///    arm runs conflict_copy (+ the merge offer when the flag is on). B
///    never syncs between (3) and (4), so push beats pull and the conflict
///    is deterministic (no W5 drift-guard detour needed).
///
/// Returns B's conflicting pass stats (its `merge_offered` is the contract).
async fn hero_conflict(a: &Engine, b: &Engine, flag_on: bool) -> cairn_sync::PassStats {
    let base = hero_base();
    // theirs = A's head re-cut (in-point 6 frames later) — the journal winner
    let mut theirs = base.clone();
    theirs.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(6, 24),
        duration: tv(90, 24),
    });
    // ours = B's tail re-cut (8 frames off the end) — the local edit
    let mut ours = base.clone();
    ours.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(0, 24),
        duration: tv(88, 24),
    });

    write_and_dirty(a, "seq.otio", &otio(&base));
    a.sync_pass().await.unwrap();
    b.sync_pass().await.unwrap(); // B discovers the base only
    write_and_dirty(a, "seq.otio", &otio(&theirs));
    a.sync_pass().await.unwrap(); // A's winner lands while B is offline
    if flag_on {
        b.store.meta_set("flag:semantic_merge", "true").unwrap();
    }
    write_and_dirty(b, "seq.otio", &otio(&ours));
    b.sync_pass().await.unwrap() // THE conflict pass
}

/// The same recipe over arbitrary file bytes (non-timeline / attr shapes).
async fn bytes_conflict(
    a: &Engine,
    b: &Engine,
    path: &str,
    base: &[u8],
    theirs: &[u8],
    ours: &[u8],
    flag_on: bool,
) -> cairn_sync::PassStats {
    write_and_dirty(a, path, base);
    a.sync_pass().await.unwrap();
    b.sync_pass().await.unwrap();
    write_and_dirty(a, path, theirs);
    a.sync_pass().await.unwrap();
    if flag_on {
        b.store.meta_set("flag:semantic_merge", "true").unwrap();
    }
    write_and_dirty(b, path, ours);
    b.sync_pass().await.unwrap()
}

/// The canonical convergence driver: each engine flushes + pulls per round.
async fn converge(a: &Engine, b: &Engine, rounds: usize) {
    for _ in 0..rounds {
        a.sync_pass().await.unwrap();
        b.sync_pass().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Flag OFF: a real .otio CONFLICT still produces the classic conflict copy
/// and NO offer (the affordance is strictly opt-in).
#[tokio::test]
async fn flag_off_conflict_leaves_no_offer() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let stats = hero_conflict(&a, &b, false).await;

    // the server rule fired exactly once, for B's upsert of the timeline
    assert_eq!(
        journal.rejections(),
        vec!["dev-B seq.otio".to_string()],
        "B's append must be the one refused"
    );
    assert_eq!(stats.conflicts_resolved, 1);
    assert_eq!(stats.merge_offered, 0, "flag off: no offer, ever");
    assert!(b.store.get_merge_offer("p1", "seq.otio").is_none());
    // the classic §7.1 end state holds: the copy preserves B's edit
    let copies = conflict_copies(&b, "seq");
    assert_eq!(copies.len(), 1, "exactly one conflict copy");
    let copy_bytes = std::fs::read(&copies[0]).unwrap();
    let copy_tl = cairn_tl::parse::parse_otio(std::str::from_utf8(&copy_bytes).unwrap()).unwrap();
    assert_eq!(
        copy_tl.tracks.children[0].children[0]
            .source_range
            .as_ref()
            .unwrap()
            .duration
            .value
            .num,
        88,
        "the copy carries B's tail re-cut"
    );
}

/// Flag ON: the same real conflict now ALSO creates one offer row carrying
/// the manifest identities + the semantic report, and the pass reports it.
#[tokio::test]
async fn flag_on_conflict_creates_merge_offer() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let stats = hero_conflict(&a, &b, true).await;

    assert_eq!(stats.conflicts_resolved, 1);
    assert_eq!(stats.merge_offered, 1, "exactly one offer for the conflict");
    let offer = b
        .store
        .get_merge_offer("p1", "seq.otio")
        .expect("offer row exists");
    // the offer stores MANIFESTS, not bytes: base = the seq-1 ancestor B's
    // append claimed, theirs = A's winning seq-2 upsert, copy = B's side
    assert_eq!(
        offer.base_manifest,
        journal.upsert_manifest_at(1).unwrap(),
        "base manifest = the claimed ancestor entry"
    );
    assert_eq!(
        offer.theirs_manifest,
        journal.upsert_manifest_at(2).unwrap(),
        "theirs manifest = the journal head that won the race"
    );
    assert!(
        Path::new(&offer.copy_path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("otio")),
        "copy_path points at the conflict copy: {}",
        offer.copy_path
    );
    // the report is the semantic policy's: head vs tail re-cuts = C11, Notes
    let report: serde_json::Value = serde_json::from_str(&offer.report_json).unwrap();
    assert_eq!(report["policy"], "semantic");
    assert_eq!(report["outcome"], "notes");
    assert_eq!(report["histogram"]["C11"], 1);
    // passive affordance: the conflict copy still exists alongside the offer
    assert_eq!(conflict_copies(&b, "seq").len(), 1);
    // and the OTHER device never sees an offer (it never conflicted)
    assert!(a.store.get_merge_offer("p1", "seq.otio").is_none());
}

/// Declining keeps the §7.1 contract: only the affordance goes — the offer
/// row is removed, the conflict copy (file + row) stays for manual resolution.
#[tokio::test]
async fn decline_keeps_conflict_copy_and_clears_offer() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let _stats = hero_conflict(&a, &b, true).await;
    let offer = b.store.get_merge_offer("p1", "seq.otio").unwrap();
    let copy_path = offer.copy_path.clone();

    let declined = b.decline_offer("seq.otio").unwrap();
    assert!(declined, "an existing offer was removed");

    assert!(b.store.get_merge_offer("p1", "seq.otio").is_none());
    // the copy survives on disk AND in the file table (B's edit is preserved)
    let copy_full = ws_file(&b, &copy_path);
    assert!(copy_full.is_file(), "decline keeps the conflict copy file");
    let copy_row = b
        .store
        .get_file("p1", &copy_path)
        .expect("decline keeps the copy's file row");
    assert_eq!(copy_row.mode, "file");
    // declining is idempotent: no offer -> Ok(false), nothing to delete
    assert!(!b.decline_offer("seq.otio").unwrap());
}

/// Acceptance: ONE journal entry for the merged timeline, both devices
/// converge on the merged head, and the accepting device's conflict story is
/// fully cleaned up (offer row gone, copy row + file gone).
#[tokio::test]
async fn accept_writes_one_journal_entry_and_both_devices_converge() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let _stats = hero_conflict(&a, &b, true).await;
    let offer = b.store.get_merge_offer("p1", "seq.otio").unwrap();
    let copy_path = offer.copy_path.clone();
    let seq_before_accept = journal.upserts_for("seq.otio", "dev-B", 0).len() as u64;

    let outcome = b.accept_offer("seq.otio").await.unwrap();
    let report: serde_json::Value = serde_json::from_str(&outcome.report_json).unwrap();
    assert_eq!(
        report["outcome"], "notes",
        "the recomputed merge is the same one"
    );
    assert_eq!(report["policy"], "semantic");

    // EXACTLY ONE new journal entry for the original path from this device
    // (the accept pipeline; a re-push loop here would livelock both devices)
    let upserts = journal.upserts_for("seq.otio", "dev-B", seq_before_accept);
    assert_eq!(
        upserts.len(),
        1,
        "accept must append the merged timeline exactly once, got {upserts:?}"
    );
    let merged_manifest = upserts[0].1.clone();

    // offer row gone; conflict copy row + file gone on the accepting device
    assert!(b.store.get_merge_offer("p1", "seq.otio").is_none());
    assert!(b.store.get_file("p1", &copy_path).is_none());
    assert!(!ws_file(&b, &copy_path).exists(), "copy file removed");
    // the merged bytes ARE on the original path now
    assert!(ws_file(&b, "seq.otio").is_file());

    converge(&a, &b, 2).await;

    // BOTH devices converged on the merged head for the original path
    let a_manifest = a
        .store
        .get_file("p1", "seq.otio")
        .expect("A has the original path")
        .manifest_hash;
    let b_manifest = b
        .store
        .get_file("p1", "seq.otio")
        .expect("B has the original path")
        .manifest_hash;
    assert_eq!(b_manifest.as_deref(), Some(merged_manifest.as_str()));
    assert_eq!(
        a_manifest, b_manifest,
        "both stores report the SAME manifest for the merged timeline"
    );
    // no churn: convergence added no further entries for the path
    assert_eq!(
        journal
            .upserts_for("seq.otio", "dev-B", seq_before_accept)
            .len(),
        1,
        "the merged upsert must not re-push during convergence"
    );
    // offer rows exist on neither device (B consumed it; A never had one)
    assert!(a.store.get_merge_offer("p1", "seq.otio").is_none());
    assert!(b.store.get_merge_offer("p1", "seq.otio").is_none());

    // the merged file parses as OTIO and carries BOTH re-cuts composed:
    // in-point 6 (A's head), duration 96 - 6 - 8 = 82 (B's tail)
    let merged_bytes = std::fs::read(ws_file(&b, "seq.otio")).unwrap();
    let merged = cairn_tl::parse::parse_otio(std::str::from_utf8(&merged_bytes).unwrap()).unwrap();
    let hero = &merged.tracks.children[0].children[0];
    assert_eq!(hero.source_range.as_ref().unwrap().start.value.num, 6);
    assert_eq!(hero.source_range.as_ref().unwrap().duration.value.num, 82);

    // The copy's journey is per-device BY DESIGN (§7.1): B removed its local
    // copy above; the copy was journaled as a normal upsert (seq 3) and A
    // keeps it as the preserved record of B's edit — the copy's removal never
    // propagates (hard delete, no journal op; the module doc is explicit).
    let a_copy = a
        .store
        .get_file("p1", &copy_path)
        .expect("A retains the journaled copy of B's edit");
    assert_eq!(a_copy.mode, "file");
}

/// A conflicted NON-timeline (.txt) never offers, flag or no flag: the offer
/// gate is the timeline predicate, not the flag alone.
#[tokio::test]
async fn non_timeline_conflict_creates_no_offer() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let stats = bytes_conflict(
        &a,
        &b,
        "notes.txt",
        b"line: base\n",
        b"line: A won the head\n",
        b"line: B rewrote it\n",
        true,
    )
    .await;

    assert_eq!(stats.conflicts_resolved, 1);
    assert_eq!(
        journal.rejections(),
        vec!["dev-B notes.txt".to_string()],
        "a real conflict happened"
    );
    assert_eq!(stats.merge_offered, 0, "txt is not a timeline: no offer");
    assert!(b.store.get_merge_offer("p1", "notes.txt").is_none());
    // the classic conflict resolution still ran
    assert_eq!(conflict_copies(&b, "notes").len(), 1);
}

/// A merge that STILL lands in Outcome::Conflicts under the semantic policy
/// (both sides changed the SAME attribute of the same clip — C3) creates NO
/// offer: an offer whose result needs a human is a second conflict wearing a
/// badge. The conflict copy alone remains.
#[tokio::test]
async fn conflicting_merge_outcome_creates_no_offer() {
    let journal = SharedJournal::shared();
    let (_ha, a) = engine_with(Arc::clone(&journal), "dev-A");
    let (_hb, b) = engine_with(Arc::clone(&journal), "dev-B");

    let base = hero_base();
    // both sides rename the SAME clip (same key, different values) — the
    // golden C3 recipe (golden.rs golden_c3_attr_same_key_conflict)
    let mut theirs = base.clone();
    theirs.tracks.children[0].children[0].name = "TheirsName".into();
    let mut ours = base.clone();
    ours.tracks.children[0].children[0].name = "OursName".into();

    // sanity at the tl layer: semantic policy does NOT rescue a C3 pair
    let (merged, report) = cairn_tl::merge::merge_with(
        &base,
        &ours,
        &theirs,
        &cairn_tl::merge::MergeOptions { semantic: true },
    )
    .unwrap();
    assert_eq!(report.outcome, cairn_tl::merge::Outcome::Conflicts);
    assert_eq!(merged.tracks.children[0].children[0].name, "Hero");

    let stats = bytes_conflict(
        &a,
        &b,
        "seq.otio",
        &otio(&base),
        &otio(&theirs),
        &otio(&ours),
        true,
    )
    .await;

    assert_eq!(stats.conflicts_resolved, 1);
    assert_eq!(
        stats.merge_offered, 0,
        "a still-conflicted merge is never offered"
    );
    assert!(b.store.get_merge_offer("p1", "seq.otio").is_none());
    assert_eq!(
        conflict_copies(&b, "seq").len(),
        1,
        "the copy is the only artifact"
    );
}
