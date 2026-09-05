//! CPU-offload lane for the sync engine (ADR-0025 — the PostHog lesson).
//!
//! The production bug class this kills: FastCDC+BLAKE3 runs at ~0.8–1.2 GiB/s
//! and used to execute INLINE on tokio I/O workers inside `process_file` — a
//! 100 MiB save parked a worker for ~130 ms, and enough concurrent saves froze
//! every latency-sensitive task (header serves, presence, the dashboard) —
//! exactly PostHog's flags-service pathology (p99 2.5 s → 94 ms after the same
//! fix: "Untangling Tokio and Rayon in production", 2026-04). The lane:
//!
//! 1. small work stays inline — dispatch would cost more than the work
//!    (PostHog: 85–90 % of requests were <200 flags and stayed sequential);
//! 2. big work moves to the rayon pool via `rayon::spawn` + a `oneshot`,
//!    so I/O workers never block on CPU;
//! 3. a semaphore caps in-flight offloads — a dirty-file burst queues at the
//!    valve instead of flooding the CPU pool (their "pressure valve");
//! 4. threads are budgeted up front: tokio gets half the logical cores
//!    (it carries I/O: proxy, presence, dashboard), rayon gets all of them
//!    (hashing is the throughput-critical side); the semaphore keeps the sum
//!    honest under load instead of trusting oversubscription to behave.
//!
//! Deliberately NOT parallelized per-chunk (rejected in ADR-0025): CDC
//! boundaries are cut sequentially by construction (the Gear hash carries
//! state across bytes) and the whole-stream BLAKE3 is single-pass; the
//! parallelism unit is the FILE (multiple dirty files / devices), which this
//! lane already provides.

use std::sync::LazyLock;
use std::thread::available_parallelism;

use cairn_core::chunker::StreamHash;
use cairn_core::{CairnError, ErrorKind};
use tokio::sync::{oneshot, Semaphore};

/// Work below this size hashes inline: dispatch costs more than the compute.
/// Scaled from PostHog's "<200 flags stay sequential" to our chunk profile —
/// 8 MiB is ~2 coarse chunks, i.e. ≤ ~10 ms of hash+chunk CPU.
pub const INLINE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// In-flight offload permits = CPU lanes: a dirty-file burst of 100 files
/// queues here instead of burying the rayon pool under unbounded work.
static OFFLOAD_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(cpu_lanes()));

fn logical_cores() -> usize {
    available_parallelism().map(usize::from).unwrap_or(2)
}

/// Rayon lanes (CPU offload width) = every logical core (PostHog budget).
#[must_use]
pub fn cpu_lanes() -> usize {
    logical_cores()
}

/// Thread budget (PostHog): tokio I/O workers get half the logical cores
/// (min 2 — the runtime also carries proxy/presence/dashboard I/O), the
/// rayon CPU lane gets all of them. The ingest semaphore keeps the sum
/// honest, so oversubscription cannot resurface as the 2.5 s p99 spike.
#[must_use]
pub fn thread_budget() -> (usize, usize) {
    let logical = logical_cores();
    (logical.div_ceil(2).max(2), logical)
}

/// Install the global rayon pool at the budgeted width (idempotent).
///
/// Called once at CLI boot (before the tokio runtime exists) and by bench
/// harnesses. A second call is a no-op that logs — tests and embedding
/// harnesses may have installed their own pool first.
pub fn init_cpu_lanes() {
    let (io, cpu) = thread_budget();
    match rayon::ThreadPoolBuilder::new()
        .num_threads(cpu)
        .thread_name(|i| format!("cairn-cpu-{i}"))
        .build_global()
    {
        Ok(()) => tracing::info!(io_workers = io, cpu_lanes = cpu, "cpu lane budget"),
        Err(_) => tracing::debug!("cpu lanes: global pool already installed"),
    }
}

/// Hash+chunk `content` off the I/O runtime and hand the buffer back.
///
/// Below [`INLINE_MAX_BYTES`] the work runs inline (cheaper than dispatch).
/// Above it the buffer MOVES to a rayon worker, `StreamHash` is computed
/// there, and the result comes back over a oneshot — the calling tokio
/// worker stays free for I/O the entire time. A rayon-side panic surfaces
/// as an ordinary ingest error (the file re-dirties and retries next pass);
/// results are bit-identical to the inline path in every case.
///
/// # Errors
/// `ErrorKind::Internal` only if the CPU lane itself dies (panic or dropped
/// reply) — never for data reasons.
pub async fn hash_stream_owned(
    content: Vec<u8>,
    fine_profile: bool,
) -> Result<(StreamHash, Vec<u8>), CairnError> {
    if content.len() < INLINE_MAX_BYTES {
        return Ok((compute(&content, fine_profile), content));
    }
    // pressure valve: at most `cpu_lanes()` big batches in flight
    let _permit = OFFLOAD_SEMAPHORE.acquire().await;
    let (tx, rx) = oneshot::channel();
    rayon::spawn(move || {
        // if `compute` panics, `tx` drops and the caller gets a loud error —
        // the buffer is lost with the task, but the file just re-dirties
        let _ = tx.send((compute(&content, fine_profile), content));
    });
    let (sh, content) = rx
        .await
        .map_err(|_| CairnError::new(ErrorKind::Internal, "cpu lane panicked while hashing"))?;
    Ok((sh, content))
}

fn compute(content: &[u8], fine_profile: bool) -> StreamHash {
    if fine_profile {
        StreamHash::compute_with(
            content,
            cairn_core::CHUNK_MIN_FINE,
            cairn_core::CHUNK_AVG_FINE,
            cairn_core::CHUNK_MAX_FINE,
        )
    } else {
        StreamHash::compute(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Deterministic incompressible-ish pattern (same LCG shape as the burst bench).
    fn pattern(n: usize, seed: u64) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        for (i, slot) in buf.chunks_mut(1 << 20).enumerate() {
            let mut x = seed ^ (u64::try_from(i).unwrap_or(0) << 32);
            for b in slot.iter_mut() {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *b = (x >> 33) as u8;
            }
        }
        buf
    }

    fn assert_same_stream(a: &StreamHash, b: &StreamHash) {
        assert_eq!(a.spans, b.spans);
        assert_eq!(a.chunk_hashes, b.chunk_hashes);
        assert_eq!(a.stream_hash, b.stream_hash);
        assert_eq!(a.file_hash, b.file_hash);
    }

    #[tokio::test]
    async fn small_buffers_hash_inline_identically() {
        let buf = pattern(64 * 1024, 7);
        let (sh, back) = hash_stream_owned(buf.clone(), false).await.unwrap();
        assert_same_stream(&sh, &StreamHash::compute(&buf));
        assert_eq!(back, buf, "buffer must come home untouched");
    }

    #[tokio::test]
    async fn big_buffers_offload_identically() {
        // > INLINE_MAX_BYTES, spans several coarse chunks, and exercises the
        // rayon round-trip end to end
        let buf = pattern(12 * 1024 * 1024, 11);
        let (sh, back) = hash_stream_owned(buf.clone(), false).await.unwrap();
        assert_same_stream(&sh, &StreamHash::compute(&buf));
        assert_eq!(back, buf);
    }

    #[tokio::test]
    async fn fine_profile_round_trips_through_the_lane() {
        let buf = pattern(12 * 1024 * 1024, 13);
        let (sh, _) = hash_stream_owned(buf.clone(), true).await.unwrap();
        assert_same_stream(
            &sh,
            &StreamHash::compute_with(
                &buf,
                cairn_core::CHUNK_MIN_FINE,
                cairn_core::CHUNK_AVG_FINE,
                cairn_core::CHUNK_MAX_FINE,
            ),
        );
    }

    /// THE PostHog pin: with a single I/O worker, a 96 MiB ingest (~120 ms of
    /// CPU) must not stall an unrelated 20 ms timer — before the lane, this
    /// timer waited out the entire hash. Generous bound (3.5×) for shared
    /// CI runners; the broken behavior fails it by ~100 ms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn offload_keeps_the_io_worker_free() {
        let buf = pattern(96 * 1024 * 1024, 17);
        let ingest = tokio::spawn(hash_stream_owned(buf, false));
        let t0 = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let dt = t0.elapsed();
        ingest.await.unwrap().unwrap();
        assert!(
            dt < Duration::from_millis(70),
            "a 20ms timer took {dt:?} — CPU work is blocking the I/O worker"
        );
    }

    /// Burst pressure: more concurrent big batches than permits — every one
    /// must still complete correctly (gated, not dropped).
    #[tokio::test]
    async fn concurrent_batches_gate_through_the_semaphore() {
        let bufs: Vec<Vec<u8>> = (0..4)
            .map(|i| {
                pattern(
                    9 * 1024 * 1024 + i * 1024,
                    u64::try_from(i).unwrap_or(0) + 100,
                )
            })
            .collect();
        let expected: Vec<StreamHash> = bufs.iter().map(|b| StreamHash::compute(b)).collect();
        let mut ingests = Vec::new();
        for b in bufs {
            ingests.push(tokio::spawn(hash_stream_owned(b, false)));
        }
        for (ing, exp) in ingests.into_iter().zip(expected.iter()) {
            let (sh, _) = ing.await.unwrap().unwrap();
            assert_same_stream(&sh, exp);
        }
    }
}
