//! Cairn harness crate: scripted kill -9 at each numbered step of the upload path (§15.4),
//! crash recovery verification, golden-corpus tooling, and the 5GB-class e2e round-trip.

#![forbid(unsafe_code)]

pub mod crash;

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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::CrashTest { steps } => crash::run_matrix(steps),
        Cmd::CrashWorker { db_dir, crash_at } => crash::worker(&db_dir, crash_at),
    }
}
