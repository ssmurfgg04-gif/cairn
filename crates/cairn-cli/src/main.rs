//! Cairn CLI + local daemon entry point.
//!
//! Daemon architecture (SPEC §11): single process; localhost gRPC ctl on 127.0.0.1:17777
//! (token-authenticated); local diagnostics dashboard on 127.0.0.1:17778 (ADR-0009). The ctl
//! contract is frozen in docs/ctl-api.md — breaking changes are bugs.

#![forbid(unsafe_code)]

mod daemon;
mod dashboard;
mod doctor;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cairn",
    about = "Cairn — content-addressed sync & storage for video teams"
)]
pub struct Cli {
    /// Data directory (default ~/.cairn)
    #[arg(long, env = "CAIRN_HOME")]
    pub home: Option<String>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Enroll this device (token stored in OS keychain)
    Login {
        /// Server address (host:port)
        #[arg(long)]
        server: String,
        /// Enrollment code from an admin
        #[arg(long)]
        code: String,
        /// Device name
        #[arg(long, default_value = "workstation")]
        name: String,
        /// Dev fallback: store token in a 0600 file instead of the keychain
        #[arg(long, hide = true)]
        allow_plaintext_file: bool,
    },
    /// Remove the stored device token (revokes nothing server-side)
    Logout,
    /// Show daemon + project sync status
    Status {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Run one sync pass over all attached roots
    Sync {
        /// Project id (default: all)
        #[arg(long)]
        project: Option<String>,
    },
    /// Snapshot operations
    Snapshot {
        #[command(subcommand)]
        cmd: snapshot::SnapshotCmd,
    },
    /// Pin a path (fetch + local CAS pin; eviction-exempt)
    Pin {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path
        #[arg(long)]
        path: String,
    },
    /// Unpin a path
    Unpin {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path
        #[arg(long)]
        path: String,
    },
    /// List active leases for a project
    Lease {
        /// Project id
        #[arg(long)]
        project: String,
    },
    /// Recall archived (cold) content with progress + ETA
    Recall {
        /// Project id
        #[arg(long)]
        project: String,
        /// Optional single path
        #[arg(long)]
        path: Option<String>,
    },
    /// Run the doctor diagnostics suite
    Doctor {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// GC shadow-mode report (beta gate: must run clean)
    GcShadowReport {
        /// Tenant id
        #[arg(long)]
        tenant: String,
        /// Optional project filter
        #[arg(long)]
        project: Option<String>,
    },
    /// Run the storage server (metadata + data planes)
    Server {
        /// Data dir
        #[arg(long, default_value = "./.cairn-server")]
        data_dir: String,
        /// gRPC address
        #[arg(long, default_value = "127.0.0.1:7443")]
        grpc_addr: String,
        /// Object-store HTTP address (dev backend)
        #[arg(long, default_value = "127.0.0.1:7444")]
        objects_addr: String,
        /// Dev bootstrap: enroll codes without an admin token (DEV ONLY)
        #[arg(long)]
        dev_insecure: bool,
    },
    /// Run the local daemon (ctl gRPC :17777 + dashboard :17778)
    Daemon {
        /// Bind address for ctl gRPC (loopback only)
        #[arg(long, default_value = "127.0.0.1:17777")]
        ctl_addr: String,
        /// Bind address for the local dashboard (loopback only, ADR-0009)
        #[arg(long, default_value = "127.0.0.1:17778")]
        ui_addr: String,
    },
}

pub mod snapshot {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum SnapshotCmd {
        /// Create a snapshot (fold trigger, on demand — SPEC §7.2)
        Create {
            #[arg(long)]
            project: String,
            #[arg(long, default_value = "")]
            label: String,
        },
        /// List snapshots for a project
        List {
            #[arg(long)]
            project: String,
        },
        /// Restore a snapshot to a target path
        Restore {
            #[arg(long)]
            project: String,
            #[arg(long)]
            commit: String,
            #[arg(long)]
            target: Option<String>,
        },
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .try_init()
        .ok();
    let cli = Cli::parse();
    let home = std::path::PathBuf::from(cli.home.clone().unwrap_or_else(default_home));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, home))
}

fn default_home() -> String {
    dirs::home_dir()
        .map(|h| h.join(".cairn").to_string_lossy().into_owned())
        .unwrap_or_else(|| ".cairn".into())
}

async fn run(cli: Cli, home: std::path::PathBuf) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Server {
            data_dir,
            grpc_addr,
            objects_addr,
            dev_insecure,
        } => {
            if dev_insecure {
                tracing::warn!("DEV-INSECURE mode: enrollment codes issued without admin auth");
            }
            cairn_server::run::run(cairn_server::run::ServerConfig {
                data_dir: data_dir.into(),
                grpc_addr,
                objects_addr,
                dev_insecure,
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
        }
        Cmd::Daemon { ctl_addr, ui_addr } => daemon::run(home, ctl_addr, ui_addr).await,
        Cmd::Status { json } => {
            let report = doctor::collect(&home);
            if json {
                println!("{}", serde_json::to_string_pretty(&report.checks.iter()
                    .map(|c| serde_json::json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
                    .collect::<Vec<_>>())?);
            } else {
                for c in &report.checks {
                    println!(
                        "{:3} {:<28} {}",
                        if c.ok { "ok" } else { "!!" },
                        c.name,
                        c.detail
                    );
                }
            }
            Ok(())
        }
        Cmd::Doctor { json } => {
            let report = doctor::collect(&home);
            report.print(json);
            std::process::exit(i32::from(!report.healthy()));
        }
        // Commands that require the sync engine / server land with M2–M5.
        Cmd::Login {
            server,
            code,
            name,
            allow_plaintext_file,
        } => daemon::login(&home, &server, &code, &name, allow_plaintext_file).await,
        Cmd::Logout => {
            daemon::logout(&home);
            Ok(())
        }
        Cmd::Sync { .. }
        | Cmd::Snapshot { .. }
        | Cmd::Pin { .. }
        | Cmd::Unpin { .. }
        | Cmd::Lease { .. }
        | Cmd::Recall { .. }
        | Cmd::GcShadowReport { .. } => {
            anyhow::bail!("this command needs a running daemon: `cairn daemon` (wired through ctl gRPC; see docs/ctl-api.md)")
        }
    }
}
