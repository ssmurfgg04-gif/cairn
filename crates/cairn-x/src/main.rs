//! Cairn harness crate: scripted kill -9 at each numbered step of the upload path (§15.4),
//! crash recovery verification, golden-corpus tooling, and the 5GB-class e2e round-trip.

#![forbid(unsafe_code)]

pub mod bench;
pub mod burst;
pub mod cold_fetch;
pub mod corpus;
pub mod corpus_real;
pub mod crash;
mod http;
#[cfg(test)]
mod m3;
mod s3_conformance;

use clap::{Parser, Subcommand};

/// cairn-x CLI.
#[derive(Parser)]
#[command(name = "cairn-x", about = "Cairn test harness")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Subcommands.
#[derive(Subcommand)]
pub enum Cmd {
    /// Run the kill -9 fault-injection matrix over the durable-step script.
    CrashTest {
        /// Max steps to test (default: all steps in the script).
        #[arg(long, default_value_t = 6)]
        steps: usize,
    },
    /// Worker used by CrashTest (not for humans).
    CrashWorker {
        /// Store directory.
        #[arg(long)]
        db_dir: String,
        /// Crash right after acknowledging this step (abrupt exit, no cleanup).
        #[arg(long)]
        crash_at: usize,
    },
    /// Generate the deterministic golden-corpus seed sequences (§15.3).
    /// Run the SOTA benchmark suite (release build required for honest numbers).
    Bench {
        /// Measured iterations per benchmark (median reported).
        #[arg(long, default_value_t = 3)]
        iters: usize,
    },
    CorpusGen {
        /// Output directory (default: the cairn-core corpus location).
        #[arg(long, default_value = "crates/cairn-core/corpus")]
        out: String,
        /// Number of sequences.
        #[arg(long, default_value_t = 4)]
        sequences: usize,
        /// Saves per sequence.
        #[arg(long, default_value_t = 10)]
        saves: usize,
        /// Base size per file, MB.
        #[arg(long, default_value_t = 24)]
        base_mb: usize,
        /// Deterministic seed.
        #[arg(long, default_value_t = 2026_0901)]
        seed: u64,
    },
    /// Real-corpus ingest report (real studio media; honest numbers, JSON out).
    CorpusReal {
        /// Directory with the downloaded real-media files.
        #[arg(long)]
        dir: String,
        /// JSON report output path.
        #[arg(long, default_value = "real-corpus-report.json")]
        out: String,
        /// File (inside dir) to measure save-shaped mutation reuse on.
        #[arg(long)]
        mutation_file: Option<String>,
    },
    /// S3 wire-conformance check (WO6-4): validate SigV4 presigning against a REAL
    /// S3-compatible server (MinIO in CI / any endpoint you own). Refuses bucket
    /// targets the operator does not own — never point it at indexed/open buckets.
    S3Conformance {
        /// Endpoint of the S3-compatible server (e.g. http://127.0.0.1:19000).
        #[arg(long, env = "CAIRN_S3_ENDPOINT")]
        endpoint: String,
        #[arg(long, env = "CAIRN_S3_BUCKET")]
        bucket: String,
        #[arg(long, env = "CAIRN_S3_REGION", default_value = "us-east-1")]
        region: String,
        #[arg(long, env = "CAIRN_S3_ACCESS_KEY_ID")]
        access_key: String,
        #[arg(long, env = "CAIRN_S3_SECRET_ACCESS_KEY")]
        secret_key: String,
        /// Path-style addressing (MinIO/localhost; default for conformance).
        #[arg(long, default_value_t = true)]
        path_style: bool,
        /// Explicit confirmation that the target bucket is YOURS (or a local
        /// test server). Required — this tool never probes third-party buckets.
        #[arg(long)]
        i_own_the_target: bool,
    },
    /// COLD-FETCH first-byte measurement (WO6-4): one stored chunk fetched
    /// through the REAL plane (presign + presigned GET, streamed) from a fresh
    /// process with empty client state. Reports first-byte p50/p95/max.
    ColdFetch {
        /// Device home directory with an enrolled identity (CAIRN_HOME).
        #[arg(long, env = "CAIRN_HOME")]
        home: String,
        /// Server URL (defaults to the identity's enrolled server).
        #[arg(long)]
        server: Option<String>,
        /// Chunk hash (hex) to fetch — soak scripts pick the largest stored chunk.
        #[arg(long)]
        hash: String,
        /// Measured fetches (median/percentiles reported).
        #[arg(long, default_value_t = 5)]
        iters: usize,
    },
    /// BURST open benchmark (WO6-5): N concurrent workers open files through the
    /// FUSE-parity read path with warm header cache; gate = first-byte and
    /// first-2-MiB p95 < 50 ms (SPEC §2 I1, cached) under load.
    Burst {
        /// Distinct files in the store.
        #[arg(long, default_value_t = 32)]
        files: usize,
        /// Per-file size in MiB.
        #[arg(long, default_value_t = 8)]
        file_mb: usize,
        /// Concurrent open workers (the burst width).
        #[arg(long, default_value_t = 32)]
        workers: usize,
        /// Opens per worker (samples = workers × opens).
        #[arg(long, default_value_t = 25)]
        opens: usize,
        /// I1 gate in ms (SPEC §2).
        #[arg(long, default_value_t = 50.0)]
        gate_ms: f64,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::CrashTest { steps } => crash::run_matrix(steps),
        Cmd::CrashWorker { db_dir, crash_at } => crash::worker(&db_dir, crash_at),
        Cmd::CorpusGen {
            out,
            sequences,
            saves,
            base_mb,
            seed,
        } => corpus::run(&out, sequences, saves, base_mb, seed),
        Cmd::CorpusReal {
            dir,
            out,
            mutation_file,
        } => corpus_real::run(&dir, &out, mutation_file),
        Cmd::S3Conformance {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
            path_style,
            i_own_the_target,
        } => {
            if !i_own_the_target {
                anyhow::bail!(
                    "refusing to run: bucket targets must be YOURS (or a local test server). \
                     Testing against buckets found via public indexes (GrayHatWarfare etc.) is \
                     unauthorized access regardless of their openness. Re-run with \
                     --i-own-the-target to confirm ownership."
                );
            }
            let cfg = s3_conformance::ConformanceCfg {
                endpoint,
                bucket,
                region,
                access_key,
                secret_key,
                path_style,
            };
            let results = tokio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(s3_conformance::run(&cfg))?;
            let mut report = serde_json::Map::new();
            let mut failed = 0usize;
            for r in &results {
                println!(
                    "{:<42} {}  {}",
                    r.name,
                    if r.ok { "PASS" } else { "FAIL" },
                    r.detail
                );
                report.insert(
                    r.name.to_string(),
                    serde_json::json!({"ok": r.ok, "detail": r.detail}),
                );
                if !r.ok {
                    failed += 1;
                }
            }
            let summary = serde_json::json!({
                "endpoint": cfg.endpoint,
                "bucket": cfg.bucket,
                "path_style": cfg.path_style,
                "checks": serde_json::Value::Object(report),
                "failed": failed,
            });
            println!(
                "\n{} checks, {} failed{}",
                results.len(),
                failed,
                if failed == 0 {
                    " — SigV4 conformance proven on the wire".to_string()
                } else {
                    String::new()
                }
            );
            if let Ok(path) = std::env::var("CAIRN_S3_CONFORMANCE_REPORT") {
                std::fs::write(&path, serde_json::to_string_pretty(&summary)?)?;
                println!("report: {path}");
            }
            if failed > 0 {
                anyhow::bail!("{failed} conformance check(s) failed");
            }
            Ok(())
        }
        Cmd::Bench { iters } => bench::run(iters),
        Cmd::ColdFetch {
            home,
            server,
            hash,
            iters,
        } => cold_fetch::run(cold_fetch::ColdFetchArgs {
            home,
            server,
            hash,
            iters,
        }),
        Cmd::Burst {
            files,
            file_mb,
            workers,
            opens,
            gate_ms,
        } => burst::run(burst::BurstArgs {
            files,
            file_mb,
            workers,
            opens,
            gate_ms,
        }),
    }
}
