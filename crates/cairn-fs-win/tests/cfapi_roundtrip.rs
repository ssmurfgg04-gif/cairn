//! WO2 CfAPI round-trip on REAL Windows (runs on windows-latest GitHub runners —
//! the "solution on GitHub" for validating without a human's box).
//!
//! Gate (from the work order):
//!   1. sync-root registers (CfRegisterSyncRoot against the real cldflt driver),
//!   2. a placeholder is created from a stored 8 MB blob and is visible via the
//!      filesystem (directory listing),
//!   3. a CHILD process opens the placeholder → FETCH_DATA fires → the callback
//!      serves hash-verified bytes from the source → the file reads back
//!      byte-identical (BLAKE3-verified),
//!   4. the first-2 MB latency is measured THROUGH the CfAPI callback (the project's
//!      first real invariant measurement on Windows; gate < 50 ms).
#![cfg(windows)]

use std::collections::HashMap;
use std::sync::Arc;

use cairn_fs_win::cfapi::{connect, create_placeholder, register_sync_root, PlaceholderSource};

const BLOB_BYTES: usize = 8 * 1024 * 1024;

struct MemSource(HashMap<String, Vec<u8>>);

impl PlaceholderSource for MemSource {
    fn fetch(&self, manifest_hash_hex: &str, offset: u64, len: u32) -> Result<Vec<u8>, i32> {
        let blob = self.0.get(manifest_hash_hex).ok_or(0xC000_0225u32 as i32)?; // STATUS_NOT_FOUND
        let start = offset as usize;
        let end = start + len as usize;
        blob.get(start..end)
            .map(|s| s.to_vec())
            .ok_or(0xC000_000Bu32 as i32) // STATUS_INVALID_PARAMETER
    }
}

fn deterministic_blob() -> (Vec<u8>, String) {
    // xorshift64* — deterministic 8 MiB, no external corpus needed
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut blob = Vec::with_capacity(BLOB_BYTES);
    while blob.len() < BLOB_BYTES {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob.truncate(BLOB_BYTES);
    let hash = blake3::hash(&blob).to_hex().to_string();
    (blob, hash)
}

#[test]
fn placeholder_round_trips_through_cfapi_with_instrumented_i1() {
    let (blob, hash) = deterministic_blob();
    let root = tempfile::tempdir().expect("tempdir");

    // ---- gate 1: register the sync root against the REAL cldflt driver ----
    register_sync_root(root.path().to_str().unwrap(), "cairn-dev-test")
        .expect("CfRegisterSyncRoot failed (is the Cloud Filter driver available on this host?)");

    // ---- connect the callback BEFORE creating the placeholder (hydration may fire
    //      the moment a reader touches the file) ----
    let mut map = HashMap::new();
    map.insert(hash.clone(), blob);
    let _conn = connect(root.path().to_str().unwrap(), Arc::new(MemSource(map)))
        .expect("CfConnectSyncRoot failed");

    // ---- gate 2: create the placeholder; it must be visible via plain fs listing ----
    create_placeholder(
        root.path().to_str().unwrap(),
        "payload.bin",
        &hash,
        BLOB_BYTES as u64,
    )
    .expect("CfCreatePlaceholders failed");
    let listed: Vec<String> = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        listed.iter().any(|n| n == "payload.bin"),
        "placeholder not visible in directory listing: {listed:?}"
    );
    // placeholder metadata: size must be the FULL on-server size, not 0
    let len = std::fs::metadata(root.path().join("payload.bin"))
        .expect("stat placeholder")
        .len();
    assert_eq!(len, BLOB_BYTES as u64, "placeholder must report full size");

    // ---- gates 3+4: child process opens + reads THROUGH the filter callback ----
    // (self-implicit hydration is blocked by design — the provider cannot read its own
    // placeholder without deadlocking, hence a separate process)
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cfapi-hydration-probe"))
        .arg(root.path().join("payload.bin"))
        .arg(&hash)
        .output()
        .expect("spawn hydration probe");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "hydration probe failed (rc={:?}): {} {}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // ---- the I1 gate measured THROUGH the callback: first 2 MB < 50 ms ----
    let first2mb_ns = stdout
        .split("first2MB_ns=")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse::<u64>().ok())
        .expect("probe must report first2MB_ns");
    let ms = first2mb_ns as f64 / 1e6;
    println!("I1 THROUGH CfAPI callback: first 2 MiB in {ms:.2} ms (gate < 50 ms)");
    assert!(
        ms < 50.0,
        "I1 violation through the CfAPI callback: {ms:.2} ms for the first 2 MiB"
    );
}
