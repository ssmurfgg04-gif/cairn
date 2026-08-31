//! Cairn CLI + local daemon entry point.
//!
//! Daemon architecture (SPEC §11): single process; localhost gRPC ctl on 127.0.0.1:17777
//! (token-authenticated); local diagnostics dashboard on 127.0.0.1:17778 (ADR-0009). The ctl
//! contract is frozen in docs/ctl-api.md — breaking changes are bugs.

#![forbid(unsafe_code)]

mod daemon;
mod dashboard;
mod doctor;
mod projects;

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
        /// CA cert (PEM) for TLS servers with self-signed certs
        #[arg(long)]
        ca: Option<String>,
    },
    /// Remove the stored device token (revokes nothing server-side)
    Logout,
    /// Attach a folder as a project root (scan → chunk → upload → sync loop)
    Attach {
        /// Folder to attach (becomes the project root)
        path: String,
        /// Project id (default: slug of the folder name)
        #[arg(long)]
        project: Option<String>,
        /// Server addr override (host:port; default: the one stored at login)
        #[arg(long)]
        server: Option<String>,
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
    /// Detach a project root (local files are NOT touched)
    Detach {
        /// Project id
        #[arg(long)]
        project: String,
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
    /// List attached projects (live ctl view)
    Projects {
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
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
    /// (hidden, dev) Issue an enrollment code against a dev-insecure server
    #[command(hide = true)]
    DevEnrollCode {
        /// Server address (host:port)
        #[arg(long)]
        server: String,
        /// Tenant id
        #[arg(long, default_value = "t1")]
        tenant: String,
        /// Email for the code
        #[arg(long, default_value = "editor@studio.tv")]
        email: String,
    },
    /// (hidden) Count FastCDC chunks for a file (acceptance harness helper)
    #[command(hide = true)]
    ChunkCount { path: String },
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
        /// TLS server cert (PEM) for the gRPC endpoint — enables TLS on 7443
        #[arg(long)]
        tls_cert: Option<String>,
        /// TLS server key (PEM)
        #[arg(long)]
        tls_key: Option<String>,
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
    // tonic's TLS pulls rustls with both providers feature-unified; pick ring explicitly
    // (workspace-standard, THIRD_PARTY.md) before any TLS-capable code runs
    let _ = rustls::crypto::ring::default_provider().install_default();
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
            tls_cert,
            tls_key,
        } => {
            if dev_insecure {
                tracing::warn!("DEV-INSECURE mode: enrollment codes issued without admin auth");
            }
            cairn_server::run::run(cairn_server::run::ServerConfig {
                data_dir: data_dir.into(),
                grpc_addr,
                objects_addr,
                dev_insecure,
                tls_cert: tls_cert.map(std::path::PathBuf::from),
                tls_key: tls_key.map(std::path::PathBuf::from),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
        }
        Cmd::Daemon { ctl_addr, ui_addr } => daemon::run(home, ctl_addr, ui_addr).await,
        Cmd::Status { json } => {
            // live daemon view first (projects + files_synced); doctor fallback offline.
            // ctl endpoint comes from the home store (daemon persists it at boot), so
            // multi-daemon machines poll THEIR daemon, not a hardcoded port.
            let ctl =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))
                    .ok()
                    .and_then(|s| s.meta_get("ctl/addr"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "http://127.0.0.1:17777".into());
            if let Ok(mut c) =
                cairn_proto::pb::ctl_status_client::CtlStatusClient::connect(ctl).await
            {
                if let Ok(out) = c.status(cairn_proto::pb::StatusRequest {}).await {
                    let s = out.into_inner();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "version": s.version,
                                "server_reachable": s.server_reachable,
                                "projects": s.projects.iter().map(|p| serde_json::json!({
                                    "project_id": p.project_id,
                                    "root_path": p.root_path,
                                    "state": p.state,
                                    "files_synced": p.files_synced,
                                    "cursor": p.cursor,
                                    "pending_outbox": p.pending_outbox,
                                })).collect::<Vec<_>>(),
                            }))?
                        );
                    } else {
                        println!("daemon {}", s.version);
                        println!("server {}", s.server_reachable);
                        for p in &s.projects {
                            println!(
                                "   {:<24} {:<10} files={:<6} cursor={:<8} outbox={:<4} {}",
                                p.project_id,
                                p.state,
                                p.files_synced,
                                p.cursor,
                                p.pending_outbox,
                                p.root_path
                            );
                        }
                        if s.projects.is_empty() {
                            println!("   (no attached projects — `cairn attach <path>`)");
                        }
                    }
                    return Ok(());
                }
            }
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
        Cmd::DevEnrollCode {
            server,
            tenant,
            email,
        } => {
            let mut auth =
                cairn_proto::pb::auth_client::AuthClient::connect(format!("http://{server}"))
                    .await
                    .map_err(|e| anyhow::anyhow!("cannot reach server {server}: {e}"))?;
            let out = auth
                .enroll_code(cairn_proto::pb::EnrollCodeRequest {
                    tenant_id: tenant,
                    email,
                    scopes: "sync".into(),
                })
                .await?
                .into_inner();
            println!("{}", out.code);
            Ok(())
        }
        Cmd::ChunkCount { path } => {
            let bytes = std::fs::read(&path)?;
            let sh = cairn_core::chunker::StreamHash::compute(&bytes);
            println!("{}", sh.chunk_hashes.len());
            Ok(())
        }
        // Commands that require the sync engine / server land with M2–M5.
        Cmd::Login {
            server,
            code,
            name,
            allow_plaintext_file,
            ca,
        } => {
            let ca_pem = match &ca {
                Some(p) => Some(
                    std::fs::read_to_string(p)
                        .map_err(|e| anyhow::anyhow!("cannot read CA pem {p}: {e}"))?,
                ),
                None => None,
            };
            daemon::login_full(&home, &server, &code, &name, allow_plaintext_file, ca_pem).await
        }
        Cmd::Logout => {
            daemon::logout(&home);
            Ok(())
        }
        Cmd::Attach {
            path,
            project,
            server,
            ctl,
        } => ctl_attach(&ctl, &path, project.as_deref(), server.as_deref()).await,
        Cmd::Detach { project, ctl } => {
            let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl)
                .await
                .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
            c.detach_root(cairn_proto::pb::DetachRootRequest {
                project_id: project.clone(),
            })
            .await?;
            println!("detached {project}");
            Ok(())
        }
        Cmd::Projects { ctl } => {
            let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl)
                .await
                .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
            let out = c
                .list_projects(cairn_proto::pb::ListProjectsCtlRequest {})
                .await?
                .into_inner();
            for p in out.projects {
                println!("{:<24} {:<10} {}", p.project_id, p.state, p.root_path);
            }
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

async fn ctl_attach(
    ctl: &str,
    path: &str,
    project: Option<&str>,
    server: Option<&str>,
) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path)
        .map_err(|e| anyhow::anyhow!("cannot open {path}: {e}"))?
        .to_string_lossy()
        .into_owned();
    let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl.to_string())
        .await
        .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
    let out = c
        .attach_root(cairn_proto::pb::AttachRootRequest {
            root_path: root,
            server_addr: server.unwrap_or("").to_string(),
            project_id: project.unwrap_or("").to_string(),
        })
        .await?
        .into_inner();
    println!("attached {} as project `{}`", path, out.project_id);
    Ok(())
}
