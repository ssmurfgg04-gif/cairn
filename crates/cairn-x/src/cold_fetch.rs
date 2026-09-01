//! WO6-4 COLD-FETCH first-byte measurement (docs/BENCHMARKS.md).
//!
//! Loads a device identity from a CAIRN_HOME store, connects the REAL plane
//! (GetDownloadUrl → presigned GET, the exact path a never-synced device's
//! hydration takes), streams the body, and reports first-byte p50/p95/max
//! over N fetches of one stored chunk. "Cold" = fresh process + empty client
//! state; the server page cache is NOT dropped unless the operator does so
//! (the soak script escalates to `sync; drop_caches` when root/sudo allows).

use cairn_core::clock::WallClock;
use cairn_store::Store;
use cairn_sync::plane_grpc::{ColdFetchSample, GrpcPlane};
use std::sync::Arc;

/// Identity meta keys — MUST match cairn-cli `projects.rs` (`auth/*` namespace).
const K_SERVER: &str = "auth/server";
const K_TOKEN: &str = "auth/token";
const K_TENANT: &str = "auth/tenant_id";
const K_TLS_CA: &str = "auth/tls_ca";

fn percentile(v: &[f64], p: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[idx.min(v.len() - 1)]
}

pub struct ColdFetchArgs {
    /// Device home (store dir with an enrolled identity).
    pub home: String,
    /// Server URL override (defaults to the identity's enrolled server).
    pub server: Option<String>,
    /// Chunk hash (hex) to fetch — the soak picks the largest stored chunk.
    pub hash: String,
    /// Measured fetches (each is a full presign + GET round trip).
    pub iters: usize,
}

pub fn run(args: ColdFetchArgs) -> anyhow::Result<()> {
    let store = Store::open(std::path::Path::new(&args.home), Arc::new(WallClock))?;
    let token = store
        .meta_get(K_TOKEN)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no identity in {} (enroll a device first)", args.home))?;
    let tenant = store
        .meta_get(K_TENANT)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no tenant in identity"))?;
    let server = match args.server {
        Some(s) => s,
        None => store
            .meta_get(K_SERVER)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("no enrolled server; pass --server"))?,
    };
    // the identity's tls_ca is stored AS the PEM content (cairn-cli convention)
    let ca_pem = store
        .meta_get(K_TLS_CA)
        .filter(|s| !s.is_empty())
        .map(|pem| pem.into_bytes());

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let plane = GrpcPlane::connect(&server, &token, &tenant, ca_pem.as_deref()).await?;
        println!(
            "COLD-FETCH chunk {hash} from {server} — {iters} measured fetches (fresh process, empty client state)",
            hash = args.hash,
            iters = args.iters
        );
        let mut firsts = Vec::new();
        let mut totals = Vec::new();
        let mut presigns = Vec::new();
        let mut bytes_seen = 0u64;
        for i in 0..args.iters {
            let s: ColdFetchSample = plane.measure_cold_fetch(&tenant, &args.hash).await?;
            println!(
                "  fetch #{:<3} first_byte {:>8.2} ms | presign {:>7.2} ms | total {:>8.2} ms | {} bytes",
                i + 1,
                s.first_byte_ms,
                s.presign_ms,
                s.total_ms,
                s.bytes
            );
            firsts.push(s.first_byte_ms);
            totals.push(s.total_ms);
            presigns.push(s.presign_ms);
            bytes_seen = s.bytes;
        }
        println!(
            "\nCOLD-FETCH first byte: p50 {:.2} ms | p95 {:.2} ms | max {:.2} ms \
             | (presign p50 {:.2} ms, total-transfer p50 {:.2} ms, last body {} bytes)",
            percentile(&firsts, 0.50),
            percentile(&firsts, 0.95),
            firsts.iter().copied().fold(0.0, f64::max),
            percentile(&presigns, 0.50),
            percentile(&totals, 0.50),
            bytes_seen
        );
        // machine-readable line for the soak script to harvest
        println!(
            "coldfetch_first_byte_p50_ms={:.2} coldfetch_first_byte_p95_ms={:.2}",
            percentile(&firsts, 0.50),
            percentile(&firsts, 0.95)
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}
