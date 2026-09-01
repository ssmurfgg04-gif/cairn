//! Hydration probe (WO2 acceptance instrumentation): a SEPARATE process opens a
//! placeholder and reads it through the CfAPI FETCH_DATA callback. It must be a child
//! process because CfAPI blocks self-implicit hydration (the provider reading its own
//! placeholder would deadlock — see cfapi.rs self-PID guard).
//!
//! Usage: cfapi-hydration-probe <path> <expected_blake3_hex>
//! Prints: `OK first2MB_ns=<n> blake3=<hex>` on success, nonzero exit otherwise.
//! The first-2MB number is the I1 latency measured THROUGH the real filter callback —
//! the metric the review asked to exist on Windows.

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: cfapi-hydration-probe <path> <expected_blake3_hex>");
        std::process::exit(2);
    }
    let path = &args[1];
    let expected = &args[2];

    let start = std::time::Instant::now();
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("open failed: {e}");
        std::process::exit(3);
    });
    const HEAD: usize = 2 * 1024 * 1024;
    let mut head = Vec::with_capacity(HEAD);
    {
        use std::io::Read as _;
        let mut chunk = [0u8; 65536];
        loop {
            if head.len() >= HEAD {
                break;
            }
            match f.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(HEAD - head.len());
                    head.extend_from_slice(&chunk[..take]);
                }
                Err(e) => {
                    eprintln!("read failed: {e}");
                    std::process::exit(4);
                }
            }
        }
    }
    let first2mb = start.elapsed().as_nanos() as u64;

    // read the REST of the file, then hash head+rest in order (whole-file identity)
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut rest).expect("read rest");
    let mut hasher = blake3::Hasher::new();
    hasher.update(&head);
    hasher.update(&rest);
    let got = hasher.finalize().to_hex().to_string();

    if &got != expected {
        eprintln!("hash mismatch: got {got} want {expected}");
        std::process::exit(5);
    }
    println!("OK first2MB_ns={first2mb} blake3={got}");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windows-only probe");
    std::process::exit(1);
}
