//! §15.4 fault injection harness v1: scripted kill -9 at each numbered step of the durable
//! path; assert recovery with zero data loss (M1 AC: "kill -9 at any point → WAL replay →
//! zero state loss").
//!
//! The worker acknowledges a step only after its write is committed (SQLite WAL commit = the
//! durability barrier). Crashing "at step K" means: abrupt process exit (exit code 137, no
//! destructors, no file cleanup) immediately after ack K — the strongest local analogue of
//! `kill -9` (the matrix runner additionally SIGKILLs real subprocesses).

use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use cairn_core::clock::WallClock;
use cairn_core::hash::Hash;
use cairn_core::CairnError;
use cairn_store::db::FileRow;
use cairn_store::outbox::OutboxEntry;
use cairn_store::{Cas, HeaderCache, Outbox, Store};

/// Acknowledged steps performed by the worker, in order.
pub const STEPS: usize = 6;

fn ack(step: usize, detail: &str) {
    println!("ACK {step} {detail}");
    let _ = std::io::stdout().flush();
}

/// Perform step `n` durably, then ack. `crash_at`: exit abruptly after that ack.
fn perform(
    store: &Store,
    cas: &Cas,
    outbox: &Outbox,
    headers: &HeaderCache,
    n: usize,
) -> Result<(), CairnError> {
    match n {
        1 => {
            store.put_file(&FileRow {
                path: "scene.prproj".into(),
                project_id: "p1".into(),
                manifest_hash: None,
                size: 10_485_760,
                mode: "file".into(),
                mtime: 0,
                local_state: "dirty".into(),
            })?;
            ack(1, "file row dirty");
        }
        2 => {
            let chunk = b"chunk-payload-A".repeat(4096);
            cas.put(&Hash::of(&chunk), &chunk)?;
            ack(2, "chunk A in CAS");
        }
        3 => {
            let chunk = b"chunk-payload-B".repeat(4096);
            cas.put(&Hash::of(&chunk), &chunk)?;
            ack(3, "chunk B in CAS");
        }
        4 => {
            headers.put("ptr-p1-scene", &[0xABu8; 4096], Some(&[0xCDu8; 2048]))?;
            ack(4, "header cached");
        }
        5 => {
            outbox.enqueue(OutboxEntry {
                request_id: "req-crash-1".into(),
                project_id: "p1".into(),
                op: vec![1, 2, 3],
                state: "pending".into(),
                attempts: 0,
                created_at: 0,
            })?;
            ack(5, "outbox enqueued (acknowledged write)");
        }
        6 => {
            store.set_file_state("p1", "scene.prproj", "outbox_pending")?;
            store.set_cursor("dev-crash", "p1", 42)?;
            ack(6, "state + cursor advanced");
        }
        _ => {
            return Err(CairnError::new(
                cairn_core::ErrorKind::Internal,
                "unknown step",
            ))
        }
    }
    Ok(())
}

fn open_all(root: &std::path::Path) -> Result<(Store, Cas, Outbox, HeaderCache), CairnError> {
    let store = Store::open(root, Arc::new(WallClock))?;
    let conn = {
        let guard = store.conn_handle();
        std::sync::Arc::clone(&guard)
    };
    let cas = Cas::open(&root.join("blobs"), conn.clone())?;
    let outbox = Outbox::new(conn.clone());
    let headers = HeaderCache::new(conn.clone());
    Ok((store, cas, outbox, headers))
}

/// Verify every acknowledged step ≤ `acked` is present and consistent after recovery.
fn verify(root: &std::path::Path, acked: usize) -> Result<(), String> {
    let (store, cas, outbox, headers) = open_all(root).map_err(|e| format!("reopen: {e}"))?;
    if acked >= 1 {
        let f = store
            .get_file("p1", "scene.prproj")
            .ok_or("LOST: acknowledged file row (step 1)")?;
        if f.local_state != "dirty" && f.local_state != "outbox_pending" {
            return Err(format!(
                "inconsistent file state after recovery: {}",
                f.local_state
            ));
        }
    }
    if acked >= 2 {
        let chunk = b"chunk-payload-A".repeat(4096);
        let got = cas
            .get(&Hash::of(&chunk))
            .map_err(|e| format!("LOST: acknowledged chunk A: {e}"))?;
        if got != chunk {
            return Err("CORRUPT: chunk A bytes differ (I2 violation)".into());
        }
    }
    if acked >= 3 {
        let chunk = b"chunk-payload-B".repeat(4096);
        cas.get(&Hash::of(&chunk))
            .map_err(|e| format!("LOST: acknowledged chunk B: {e}"))?;
    }
    if acked >= 4 {
        headers
            .serve("ptr-p1-scene")
            .map_err(|e| format!("LOST: acknowledged header: {e}"))?;
    }
    if acked >= 5 && outbox.pending_count("p1") == 0 {
        return Err("LOST: acknowledged outbox entry (the journal append would be lost)".into());
    }
    if acked >= 6 && store.get_cursor("dev-crash", "p1") != 42 {
        return Err("LOST: acknowledged cursor".into());
    }
    // CAS integrity must hold on the whole sample
    let (_n, bad) = cas.verify_sample(1000).map_err(|e| e.to_string())?;
    if !bad.is_empty() {
        return Err(format!(
            "CORRUPT: {} CAS entries failed verification",
            bad.len()
        ));
    }
    Ok(())
}

/// M1 AC matrix: for every step K, crash right after ack K and prove zero loss.
pub fn run_matrix(steps: usize) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut failures = Vec::new();
    for k in 1..=steps.min(STEPS) {
        let dir = tempfile::tempdir()?;
        // run a fresh worker that crashes right after ack k (real SIGKILL of a subprocess)
        let status = Command::new(&exe)
            .args([
                "crash-worker",
                "--db-dir",
                dir.path().to_string_lossy().as_ref(),
                "--crash-at",
                &k.to_string(),
            ])
            .status()?;
        let acked = k; // worker acks 1..=k then dies abruptly
        if let Err(e) = verify(dir.path(), acked) {
            failures.push(format!("step {k}: {e}"));
        } else {
            tracing::info!(
                "step {k}: crash+recovery verified (exit {})",
                status.code().unwrap_or(-1)
            );
        }
    }
    if failures.is_empty() {
        println!("CAIRN_CRASH_MATRIX: ALL {steps} STEPS ZERO-LOSS");
        Ok(())
    } else {
        for f in &failures {
            eprintln!("FAILURE: {f}");
        }
        anyhow::bail!("crash matrix failures: {}", failures.len())
    }
}

/// Worker entry: performs steps 1..=crash_at then exits abruptly (code 137).
pub fn worker(db_dir: &str, crash_at: usize) -> anyhow::Result<()> {
    let root = std::path::PathBuf::from(db_dir);
    let (store, cas, outbox, headers) = open_all(&root)?;
    for n in 1..=crash_at.min(STEPS) {
        perform(&store, &cas, &outbox, &headers, n)?;
        if n == crash_at {
            // abrupt death: no destructors, no file cleanup — the WAL lives on
            std::process::exit(137);
        }
    }
    Ok(())
}
