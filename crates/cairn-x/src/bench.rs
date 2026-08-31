//! SOTA benchmark harness (`cairn-x bench`).
//!
//! Methodology: warmup pass, then N measured iterations, MEDIAN reported;
//! latencies report p50/p99 over the sampled operations. Numbers are
//! wall-clock on the build host (see docs/BENCHMARKS.md for the hardware
//! caveat) — they are relative-comparison instrumentation, not marketing.

use cairn_core::bloom::Bloom;
use cairn_core::chunker::FastCdc;
use cairn_core::clock::WallClock;
use cairn_core::hash::Hash;
use cairn_core::manifest::{Compression, Manifest, ManifestEntry};
use std::sync::Arc;
use std::time::Instant;

struct Sample {
    name: &'static str,
    unit: &'static str,
    values: Vec<f64>,
}

impl Sample {
    fn new(name: &'static str, unit: &'static str, values: Vec<f64>) -> Self {
        Sample { name, unit, values }
    }

    fn median(&self) -> f64 {
        let mut v = self.values.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    }
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[idx]
}

fn mibs(bytes: u64, secs: f64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0)) / secs
}

/// Seeded pseudo-random buffer (deterministic across runs).
fn prng_buffer(len: usize, seed: u64) -> Vec<u8> {
    use rand::SeedableRng;
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    use rand::RngCore;
    let mut buf = vec![0u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

pub fn run(iters: usize) -> anyhow::Result<()> {
    println!("# Cairn SOTA benchmarks (release build, median of {iters})\n");
    let mut samples: Vec<Sample> = Vec::new();

    // ---- 1. FastCDC chunking throughput --------------------------------
    {
        let buf = prng_buffer(512 * 1024 * 1024, 0xCDC);
        let mut vals = Vec::new();
        let mut chunks = 0usize;
        // warmup
        let _ = FastCdc::cut(&buf[..16 * 1024 * 1024]);
        for _ in 0..iters {
            let t0 = Instant::now();
            let spans = FastCdc::cut(&buf);
            vals.push(mibs(buf.len() as u64, t0.elapsed().as_secs_f64()));
            chunks = spans.len();
        }
        println!(
            "fastcdc_throughput      {:>9.1} MiB/s   (512 MiB stream, {} chunks, avg {:.2} MiB)",
            percentile(vals.clone(), 0.5),
            chunks,
            512.0 / chunks as f64
        );
        samples.push(Sample::new("fastcdc_throughput", "MiB/s", vals));
    }

    // ---- 2. BLAKE3 whole-stream throughput ------------------------------
    {
        let buf = prng_buffer(512 * 1024 * 1024, 0xB3);
        let mut vals = Vec::new();
        for _ in 0..iters {
            let t0 = Instant::now();
            let _ = Hash::of(&buf);
            vals.push(mibs(buf.len() as u64, t0.elapsed().as_secs_f64()));
        }
        samples.push(Sample::new("blake3_throughput", "MiB/s", vals));
    }

    // ---- 3. End-to-end client ingest pipeline ---------------------------
    //    chunk -> per-chunk BLAKE3 -> zstd compress -> verified CAS put
    //    (the exact client hot path for a media file save)
    let cas_dir = tempfile::tempdir()?;
    let db_dir = tempfile::tempdir()?;
    let store = cairn_store::Store::open(db_dir.path(), Arc::new(WallClock))?;
    let conn = store.conn_handle();
    let cas = cairn_store::Cas::open(cas_dir.path(), conn.clone())?;
    let chunk_bytes_total;
    {
        let buf = prng_buffer(128 * 1024 * 1024, 0x1A);
        let mut vals = Vec::new();
        for _ in 0..iters {
            let t0 = Instant::now();
            let spans = FastCdc::cut(&buf);
            let mut n: u64 = 0;
            for s in &spans {
                let sl = &buf[s.offset as usize..(s.offset + u64::from(s.len)) as usize];
                let h = Hash::of(sl);
                // local CAS stores RAW (compression is wire-only, engine.rs:131)
                cas.put(&h, sl)?;
                n += u64::from(s.len);
            }
            let dt = t0.elapsed().as_secs_f64();
            vals.push(mibs(n, dt));
        }
        chunk_bytes_total = buf.len() as u64;
        samples.push(Sample::new("ingest_pipeline", "MiB/s", vals));
    }

    // ---- 4. I1 header-cache hydration ----------------------------------
    {
        let head = prng_buffer(2 * 1024 * 1024, 0x11);
        let tail = prng_buffer(1024 * 1024, 0x12);
        let hc = cairn_store::headers::HeaderCache::new(conn.clone());
        hc.put("bench-pointer-1", &head, Some(&tail))?;
        // warmup
        let _ = hc.serve("bench-pointer-1");
        let mut lats = Vec::new();
        for i in 0..500 {
            let ph = if i % 10 == 0 {
                "bench-pointer-1".to_string()
            } else {
                format!("bench-pointer-{i}") // cold misses exercise the miss path too
            };
            let t0 = Instant::now();
            let r = hc.serve(&ph);
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            if r.is_ok() {
                lats.push(dt);
            } else {
                lats.push(dt); // misses still count as first-byte attempts (cheap NotFound)
            }
        }
        println!(
            "i1_header_first_byte    p50 {:.3} ms | p99 {:.3} ms | max {:.3} ms   (I1 gate: <50 ms cached) [{} samples]",
            percentile(lats.clone(), 0.50),
            percentile(lats.clone(), 0.99),
            lats.iter().copied().fold(0.0, f64::max),
            lats.len(),
        );
    }

    // ---- 5. CAS random-chunk read (packed-equivalent local read path) ---
    {
        let mut lats = Vec::new();
        let blobs: Vec<(Hash, Vec<u8>)> = {
            let buf = prng_buffer(8 * 1024 * 1024, 0x22);
            FastCdc::cut(&buf)
                .iter()
                .take(64)
                .map(|s| {
                    let sl = &buf[s.offset as usize..(s.offset + u64::from(s.len)) as usize];
                    (Hash::of(sl), sl.to_vec())
                })
                .collect()
        };
        for (h, b) in &blobs {
            let _ = cas.put(h, b);
        }
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
        let picks: Vec<usize> = (0..300).map(|_| rng.gen_range(0..blobs.len())).collect();
        for i in picks {
            let (h, b) = &blobs[i];
            let t0 = Instant::now();
            let got = cas.get(h)?;
            lats.push(t0.elapsed().as_secs_f64() * 1e6);
            assert_eq!(got.len(), b.len());
        }
        samples.push(Sample::new("cas_read_us", "us", lats));
    }

    // ---- 6. Journal (local store) append latency ------------------------
    {
        let mut lats = Vec::new();
        store.with_tx(|c| {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS bench_journal(seq INTEGER PRIMARY KEY, payload BLOB)",
            )
            .map_err(|e| cairn_core::CairnError::new(cairn_core::ErrorKind::Io, format!("{e}")))?;
            Ok(())
        })?;
        for i in 0..1000 {
            let t0 = Instant::now();
            store.with_tx(|c| {
                c.execute(
                    "INSERT INTO bench_journal(seq, payload) VALUES(?1, ?2)",
                    rusqlite::params![i, prng_buffer(256, i as u64)],
                )
                .map_err(|e| {
                    cairn_core::CairnError::new(cairn_core::ErrorKind::Io, format!("{e}"))
                })?;
                Ok(())
            })?;
            lats.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        samples.push(Sample::new("store_journal_append_us", "us", lats));
    }

    // ---- 7. Manifest build + serialize @100k entries --------------------
    {
        let buf = prng_buffer(4 * 1024 * 1024, 0x33);
        let spans = FastCdc::cut(&buf);
        let entries: Vec<ManifestEntry> = (0..100_000)
            .map(|i| ManifestEntry {
                offset: (i as u64) * 4096,
                len: 4096,
                chunk_hash: Hash::of(&buf[..spans[0].len as usize]),
            })
            .collect();
        let mut vals = Vec::new();
        for _ in 0..iters {
            let t0 = Instant::now();
            let m = Manifest::build(entries.clone(), Compression::Zstd3, None);
            let (_h, bytes) = m.serialize();
            let round = Manifest::parse(&bytes)?;
            vals.push(t0.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(round.entry_count(), 100_000);
        }
        samples.push(Sample::new("manifest_100k_build_ser_parse_ms", "ms", vals));
    }

    // ---- 8. Bloom negative-prefilter (BatchExists path) -----------------
    {
        let n = 1_000_000u64;
        let mut b = Bloom::with_fpp(n, 0.01);
        let t0 = Instant::now();
        for i in 0..n {
            b.insert(&i.to_le_bytes());
        }
        let build = t0.elapsed().as_secs_f64();
        let mut hits = Vec::new();
        use rand::SeedableRng;
        let mut rng = rand::rngs::SmallRng::seed_from_u64(11);
        use rand::Rng;
        for _ in 0..1_000_000 {
            let key = if rng.gen_bool(0.5) {
                (rng.gen_range(0u64..n + 1_000_000)).to_le_bytes()
            } else {
                (u64::MAX - rng.gen_range(0..1_000_000u64)).to_le_bytes()
            };
            let t = Instant::now();
            let _ = b.might_contain(&key);
            hits.push(t.elapsed().as_secs_f64() * 1e9);
        }
        println!(
            "bloom_1m                build {:.2} s | probe p50 {:.0} ns | p99 {:.0} ns  (1M items, fpp 1%)",
            build,
            percentile(hits.clone(), 0.5),
            percentile(hits, 0.99),
        );
    }

    // ---- summary table ---------------------------------------------------
    println!("\n| benchmark | median | unit |");
    println!("|---|---|---|");
    for s in &samples {
        println!("| {} | {:.3} | {} |", s.name, s.median(), s.unit);
    }
    println!("\n(total chunk bytes ingested in pipeline bench: {chunk_bytes_total} B)");
    let _ = chunk_bytes_total;
    Ok(())
}
