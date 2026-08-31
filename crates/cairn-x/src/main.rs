//! Cairn harness crate: scripted kill -9 at each numbered step of the upload path (§15.4),
//! crash recovery verification, golden-corpus tooling, and the 5GB-class e2e round-trip.

#![forbid(unsafe_code)]

pub mod bench;
pub mod corpus;
pub mod corpus_real;
pub mod crash;
mod http;
#[cfg(test)]
mod m3;

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
        Cmd::Bench { iters } => bench::run(iters),
    }
}
