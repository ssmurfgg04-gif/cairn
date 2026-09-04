//! Metadata-row push regression (round 13, the W1 windows-runner catch).
//!
//! On Windows, ReadDirectoryChangesW fires a parent-directory event the
//! moment children are created; the watch handler used to mark the DIR row
//! dirty, and `push_phase` (which only excluded symlinks) then called
//! `fs::read()` on a DIRECTORY: EACCES on Windows ("Access is denied",
//! wedging every sync pass) / EISDIR on Linux. The engine must skip
//! non-`file` rows in the push phase entirely — the scan walk re-puts
//! metadata rows; they carry no content to chunk.

use std::sync::Arc;

use async_trait::async_trait;

use cairn_core::clock::WallClock;
use cairn_core::compress::DictRegistry;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::JournalOp;
use cairn_store::state::LocalState;
use cairn_store::{Cas, FileRow, HeaderCache, Outbox, Store};
use cairn_sync::aimd::Gate;
use cairn_sync::engine::Engine;
use cairn_sync::plane::{CompleteOut, Entry, Plane, Session};

/// Minimal plane whose upload + append paths SUCCEED (the ingest must
/// complete so the pass-level assertions are about the DIR row, not about
/// upload plumbing).
struct WorkingPlane;

#[async_trait]
impl Plane for WorkingPlane {
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
    async fn complete(
        &self,
        _s: &str,
        _r: &[cairn_proto::pb::UploadReceipt],
    ) -> Result<CompleteOut, CairnError> {
        Ok(CompleteOut {
            verified: vec![],
            rejected: vec![],
        })
    }
    async fn put_presigned(&self, _u: &str, _b: &[u8], _c: &str) -> Result<(), CairnError> {
        Ok(())
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
        _d: &str,
        _r: &str,
        _op: JournalOp,
        _l: u64,
    ) -> Result<(u64, bool), CairnError> {
        Ok((1, true))
    }
    async fn fetch_batch(
        &self,
        _t: &str,
        _p: &str,
        _after: u64,
        _l: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        Ok(vec![])
    }
}

fn engine_with() -> (tempfile::TempDir, Engine) {
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
        plane: Arc::new(WorkingPlane),
        dicts: DictRegistry::new(),
        gate: Gate::default(),
    };
    (home, engine)
}

#[tokio::test]
async fn dirty_dir_row_never_reaches_fs_read() {
    let (home, engine) = engine_with();
    // the sim-default workspace + a real directory in it
    let ws = home.path().join("workspace");
    std::fs::create_dir_all(ws.join("project")).unwrap();
    std::fs::write(ws.join("project/a.mov"), b"content").unwrap();

    // the dir row, DIRTIED exactly like the windows watcher did (parent-dir
    // event on child creation)
    engine
        .store
        .put_file(&FileRow {
            path: "project".into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: 0,
            mode: "dir".into(),
            mtime: 1,
            local_state: LocalState::Dirty.as_str().into(),
        })
        .unwrap();
    // and a regular dirty FILE row (the ingest must proceed normally)
    engine
        .store
        .put_file(&FileRow {
            path: "project/a.mov".into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: 7,
            mode: "file".into(),
            mtime: 2,
            local_state: LocalState::Dirty.as_str().into(),
        })
        .unwrap();

    // pre-fix: push_phase -> process_file("project") -> fs::read(directory)
    // -> EACCES (windows) / EISDIR (linux) -> the pass ERRORS and the engine
    // wedges. post-fix: metadata rows are skipped; the pass completes.
    let stats = engine.sync_pass().await.expect(
        "a dirty DIR row must never fail the pass (fs::read on a directory: \
         EACCES on windows / EISDIR on linux)",
    );
    // the dirty FILE row is processed normally (1 append)
    assert_eq!(stats.appended, 1);

    // and the dirty dir row is left for the scan walk to re-put as metadata
    let row = engine.store.get_file("p1", "project").unwrap();
    assert_eq!(row.mode, "dir");
}

#[tokio::test]
async fn dirty_symlink_row_is_also_skipped() {
    let (home, engine) = engine_with();
    let ws = home.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();

    engine
        .store
        .put_file(&FileRow {
            path: "alias".into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: 0,
            mode: "symlink".into(),
            mtime: 1,
            local_state: LocalState::Dirty.as_str().into(),
        })
        .unwrap();

    engine
        .sync_pass()
        .await
        .expect("a dirty symlink row must be skipped, not fs::read");
}
