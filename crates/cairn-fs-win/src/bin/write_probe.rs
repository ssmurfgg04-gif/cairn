//! Child-process probe for WO6-1 write gates (runs on windows-latest CI against the
//! REAL cldflt driver). Modes:
//!
//!   edit   <path> <offset> <len> <seed>   — opens the placeholder for WRITE (fires
//!                                           VALIDATE_DATA → hydrate-before-write via
//!                                           FETCH_DATA), overwrites `len` bytes at
//!                                           `offset` with a deterministic xorshift64*
//!                                           pattern, flushes, closes (fires
//!                                           NOTIFY_FILE_CLOSE_COMPLETION).
//!   create <path> <len> <seed>            — creates a NEW plain file in the sync root
//!                                           (the editor-created-file path; the engine
//!                                           ingests + converts it to a placeholder).

#![forbid(unsafe_code)]

use std::io::{Seek, SeekFrom, Write};

fn xorshift_bytes(len: usize, mut state: u64) -> Vec<u8> {
    if state == 0 {
        state = 0x9E37_79B9_7F4A_7C15;
    }
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: cfapi-write-probe edit <path> <offset> <len> <seed> | create <path> <len> <seed>");
        std::process::exit(2);
    }
    let result = match args[0].as_str() {
        "edit" if args.len() == 5 => edit(
            &args[1],
            args[2].parse::<u64>().expect("offset"),
            args[3].parse::<usize>().expect("len"),
            args[4].parse::<u64>().expect("seed"),
        ),
        "create" if args.len() == 4 => create(
            &args[1],
            args[2].parse::<usize>().expect("len"),
            args[3].parse::<u64>().expect("seed"),
        ),
        _ => Err("bad arguments".into()),
    };
    match result {
        Ok(msg) => {
            println!("{msg}");
        }
        Err(e) => {
            eprintln!("write-probe failed: {e}");
            std::process::exit(1);
        }
    }
}

fn edit(path: &str, offset: u64, len: usize, seed: u64) -> Result<String, String> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("open-for-write {path}: {e}"))?;
    let pattern = xorshift_bytes(len, seed);
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek: {e}"))?;
    f.write_all(&pattern).map_err(|e| format!("write: {e}"))?;
    f.flush().map_err(|e| format!("flush: {e}"))?;
    // explicit drop closes the handle → the filter emits FILE_CLOSE_COMPLETION
    drop(f);
    Ok(format!("edited={len} offset={offset} seed={seed}"))
}

fn create(path: &str, len: usize, seed: u64) -> Result<String, String> {
    let bytes = xorshift_bytes(len, seed);
    std::fs::write(path, &bytes).map_err(|e| format!("create {path}: {e}"))?;
    Ok(format!("created={len} path={path}"))
}
