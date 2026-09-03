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
mod win_attach;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cairn",
    about = "Cairn — content-addressed sync & storage for video teams",
    version
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
    /// First-run setup: create the local store (~/.cairn) and report state
    /// (the device identity itself is issued server-side at `cairn login`)
    Init {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
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
    /// Capture a timeline document (stamp identities + sidecar manifest)
    TlCapture {
        /// Timeline file (.otio JSON or .fcpxml)
        path: String,
        /// Canonicalize IN PLACE (default: write <stem>.canonical.otio + sidecar)
        #[arg(long)]
        in_place: bool,
    },
    /// Three-way timeline merge (ADR-0015): base/ours/theirs -> merged + report
    TlMerge {
        /// Base timeline (the common ancestor, content-addressed)
        #[arg(long)]
        base: String,
        /// Our side (the save under the SURVIVING fence — SPEC §8)
        #[arg(long)]
        ours: String,
        /// Their side (the earlier save, rebased)
        #[arg(long)]
        theirs: String,
        /// Report only: do not write the merged document
        #[arg(long)]
        dry_run: bool,
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
        Cmd::TlCapture { path, in_place } => run_tl_capture(&path, in_place),
        Cmd::TlMerge {
            base,
            ours,
            theirs,
            dry_run,
        } => run_tl_merge(&base, &ours, &theirs, dry_run),
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
                                    "last_error": p.last_error,
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
        Cmd::Init { json } => {
            let store =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))?;
            let enrolled = projects::load_identity(&store).is_some();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "home": home.to_string_lossy(),
                        "store": "ok",
                        "enrolled": enrolled,
                    }))?
                );
            } else {
                println!("cairn home: {}", home.display());
                if enrolled {
                    println!("store ready — device enrolled on this machine");
                } else {
                    println!(
                        "store ready — not enrolled yet (device id is issued at `cairn login`)"
                    );
                }
                println!("next: `cairn daemon` then `cairn attach <folder>` (docs/BETA.md)");
            }
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
        Cmd::Sync { .. } => {
            anyhow::bail!("this command needs a running daemon: `cairn daemon` (wired through ctl gRPC; see docs/ctl-api.md)")
        }
        // ---- WO6-3: every ctl command now drives the REAL ctl RPCs ----
        Cmd::Snapshot { cmd } => match cmd {
            snapshot::SnapshotCmd::Create { project, label } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    ctl_label(&label),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .create_snapshot(cairn_proto::pb::CreateSnapshotRequest {
                        project_id: project.clone(),
                        label: label.clone(),
                    })
                    .await?
                    .into_inner();
                println!("snapshot created: {} (project {project})", out.commit_hash);
                println!("note: label is recorded in the next server fold (additive field pending, docs/ctl-api.md)");
                Ok(())
            }
            snapshot::SnapshotCmd::List { project } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    default_ctl(),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .list_snapshots(cairn_proto::pb::ListSnapshotsRequest {
                        project_id: project.clone(),
                    })
                    .await?
                    .into_inner();
                if out.snapshots.is_empty() {
                    println!("no snapshots yet for {project} — create one with `cairn snapshot create --project {project}`");
                }
                for s in out.snapshots {
                    println!(
                        "{}  seq={}  author={}  label={}",
                        s.commit_hash,
                        s.snapshot_seq,
                        if s.author.is_empty() { "-" } else { &s.author },
                        if s.label.is_empty() { "-" } else { &s.label }
                    );
                }
                Ok(())
            }
            snapshot::SnapshotCmd::Restore {
                project,
                commit,
                target,
            } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    default_ctl(),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .restore_snapshot(cairn_proto::pb::RestoreSnapshotRequest {
                        project_id: project.clone(),
                        commit_hash: commit.clone(),
                        target_path: target.clone().unwrap_or_default(),
                    })
                    .await?
                    .into_inner();
                println!(
                    "restored {} files ({} bytes) from {commit} into {}",
                    out.restored_files,
                    out.bytes,
                    target.clone().unwrap_or_else(|| "the workspace".into())
                );
                Ok(())
            }
        },
        Cmd::Pin { project, path } => {
            let mut c = cairn_proto::pb::ctl_pins_client::CtlPinsClient::connect(default_ctl())
                .await
                .map_err(daemon_down)?;
            c.pin(cairn_proto::pb::PinRequest {
                project_id: project.clone(),
                path: path.clone(),
            })
            .await?;
            println!("pinned {path} (chunks recalled + eviction-exempt)");
            Ok(())
        }
        Cmd::Unpin { project, path } => {
            let mut c = cairn_proto::pb::ctl_pins_client::CtlPinsClient::connect(default_ctl())
                .await
                .map_err(daemon_down)?;
            c.unpin(cairn_proto::pb::UnpinRequest {
                project_id: project.clone(),
                path: path.clone(),
            })
            .await?;
            println!("unpinned {path} (evictable again)");
            Ok(())
        }
        Cmd::Lease { project } => {
            // leases are server state; surface via the server's ListLeases through
            // the daemon's server channel — v1 shows local leases (leases_local)
            let store =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))?;
            let rows = store.list_leases();
            let mine: Vec<_> = rows
                .iter()
                .filter(|l| project.is_empty() || l.0.contains(&project))
                .collect();
            if mine.is_empty() {
                println!("no active local leases");
            }
            for (path, token, expires_at) in rows {
                println!("{path}  token={token}  expires_at={expires_at}");
            }
            Ok(())
        }
        Cmd::Recall { project, path } => {
            let mut c = cairn_proto::pb::ctl_recall_client::CtlRecallClient::connect(default_ctl())
                .await
                .map_err(daemon_down)?;
            let job = c
                .start_recall(cairn_proto::pb::StartRecallRequest {
                    project_id: project.clone(),
                    path: path.clone().unwrap_or_default(),
                })
                .await?
                .into_inner();
            println!("recall job {} started", job.job_id);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let st = c
                    .recall_status(cairn_proto::pb::RecallStatusRequest {
                        job_id: job.job_id.clone(),
                    })
                    .await?
                    .into_inner();
                println!(
                    "  state={} progress={:.0}% bytes_done={} bytes_total={}",
                    st.state,
                    st.progress * 100.0,
                    st.bytes_done,
                    st.bytes_total
                );
                if st.state == "completed" || st.state == "failed" {
                    break;
                }
            }
            Ok(())
        }
        Cmd::GcShadowReport { .. } => {
            anyhow::bail!("gc-shadow report runs against the storage server (server-side RPC; ADR'd in docs/ctl-api.md — not silently missing)")
        }
    }
}

fn default_ctl() -> String {
    "http://127.0.0.1:17777".to_string()
}

fn ctl_label(_label: &str) -> String {
    default_ctl()
}

fn daemon_down(e: tonic::transport::Error) -> anyhow::Error {
    anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}")
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

// ---------------------------------------------------------------------------
// ADR-0015 timeline capture + merge (cairn-tl): pure library, CLI edges only.
// Exit-code contract (tl-merge): 0 clean · 1 merged-with-notes · 2 conflicts
// (human escalation — merged file still written, conflicting edits withheld)
// · 3 refused (C10 — no output touched).
// ---------------------------------------------------------------------------

fn run_tl_capture(path: &str, in_place: bool) -> anyhow::Result<()> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let looks_xml = raw.trim_start().starts_with("<?xml") && raw.contains("<fcpxml");
    let is_fcpxml = path.ends_with(".fcpxml") || looks_xml;
    let (mut timeline, sidecar, out_path) = if is_fcpxml {
        let (major, minor) = fcpxml_version(&raw);
        let tl = cairn_tl::fcpxml::parse_fcpxml(&raw)
            .map_err(|e| anyhow::anyhow!("fcpxml ingest: {e}"))?;
        let sc = cairn_tl::sidecar::Sidecar::for_fcpxml(major, minor, raw.as_bytes());
        let out = canonical_out_path(path);
        (tl, sc, out)
    } else {
        let tl =
            cairn_tl::parse::parse_otio(&raw).map_err(|e| anyhow::anyhow!("otio parse: {e}"))?;
        (
            tl,
            cairn_tl::sidecar::Sidecar::for_otio(raw.as_bytes()),
            canonical_out_path(path),
        )
    };
    // stamp identities (idempotent — already-stamped documents pass through)
    cairn_tl::model::stamp_all(&mut timeline);
    let canonical = cairn_tl::canon::serialize_file(&timeline)
        .map_err(|e| anyhow::anyhow!("canonical serialize: {e}"))?;
    let target = if in_place && !is_fcpxml {
        path.to_string()
    } else {
        out_path
    };
    std::fs::write(&target, &canonical)
        .map_err(|e| anyhow::anyhow!("cannot write {target}: {e}"))?;
    let sidecar_path = format!("{target}.cairn-timeline");
    std::fs::write(&sidecar_path, sidecar.to_json())
        .map_err(|e| anyhow::anyhow!("cannot write {sidecar_path}: {e}"))?;
    println!("captured: {target} (+ {sidecar_path})");
    println!("elements stamped: {}", timeline.tracks.count());
    Ok(())
}

fn canonical_out_path(path: &str) -> String {
    let stem = path
        .strip_suffix(".otio")
        .unwrap_or_else(|| path.strip_suffix(".fcpxml").unwrap_or(path));
    format!("{stem}.canonical.otio")
}

fn fcpxml_version(raw: &str) -> (u32, u32) {
    // <fcpxml version="1.11"> — major 1, minor 11 (NOT 1.1: version parse bug
    // class from the pre-release, caught by the round-11 style gates)
    if let Some(pos) = raw.find("<fcpxml") {
        let head = &raw[pos..(pos + 80).min(raw.len())];
        if let Some(vpos) = head.find("version=\"") {
            let rest = &head[vpos + 9..];
            if let Some(end) = rest.find('"') {
                let v = &rest[..end];
                if let Some((ma, mi)) = v.split_once('.') {
                    return (ma.parse().unwrap_or(1), mi.parse().unwrap_or(0));
                }
            }
        }
    }
    (1, 0)
}

fn load_timeline_sidecar(
    path: &str,
) -> anyhow::Result<(
    cairn_tl::model::Timeline,
    Option<cairn_tl::sidecar::Sidecar>,
)> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let sc = std::fs::read_to_string(format!("{path}.cairn-timeline"))
        .ok()
        .and_then(|t| cairn_tl::sidecar::Sidecar::parse(&t).ok());
    if path.ends_with(".fcpxml") {
        let tl = cairn_tl::fcpxml::parse_fcpxml(&raw)
            .map_err(|e| anyhow::anyhow!("fcpxml ingest {path}: {e}"))?;
        Ok((tl, sc))
    } else {
        let tl = cairn_tl::parse::parse_otio(&raw)
            .map_err(|e| anyhow::anyhow!("otio parse {path}: {e}"))?;
        Ok((tl, sc))
    }
}

fn run_tl_merge(base: &str, ours: &str, theirs: &str, dry_run: bool) -> anyhow::Result<()> {
    // The exit-code contract: 3 means REFUSED — structural mismatch or any
    // input that cannot be parsed. Parsing failures are refusals (C10), not
    // runtime errors: no partial state, both inputs untouched.
    let refuse = |msg: String| -> ! {
        eprintln!("REFUSED: {msg}");
        eprintln!("no output written — both inputs remain untouched for the human");
        std::process::exit(3);
    };
    let (base_tl, base_sc) = load_timeline_sidecar(base).unwrap_or_else(|e| refuse(e.to_string()));
    let (ours_tl, ours_sc) = load_timeline_sidecar(ours).unwrap_or_else(|e| refuse(e.to_string()));
    let (theirs_tl, theirs_sc) =
        load_timeline_sidecar(theirs).unwrap_or_else(|e| refuse(e.to_string()));

    // sidecar version gate (C10) — only when sidecars exist (un-stamped docs
    // merge on their own bytes; the gate is the capture-substrate contract)
    if let (Some(b), Some(o), Some(t)) = (&base_sc, &ours_sc, &theirs_sc) {
        if let Err(e) = cairn_tl::sidecar::check_mergeable(b, o, t) {
            refuse(e.to_string());
        }
    }

    match cairn_tl::merge::merge(&base_tl, &ours_tl, &theirs_tl) {
        Ok((merged, report)) => {
            let code = match report.outcome {
                cairn_tl::merge::Outcome::Clean => 0,
                cairn_tl::merge::Outcome::Notes => 1,
                cairn_tl::merge::Outcome::Conflicts => 2,
            };
            if dry_run {
                println!(
                    "dry-run: {}",
                    serde_json::to_string_pretty(&report.to_json())?
                );
            } else {
                // .name.merged.otio next to ours (ADR §2.5: NEW file, never
                // in-place — the conflict-copy machinery stays the backstop)
                let out = format!("{}.merged.otio", ours.strip_suffix(".otio").unwrap_or(ours));
                let bytes = cairn_tl::canon::serialize_file(&merged)
                    .map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
                std::fs::write(&out, bytes)
                    .map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
                // .cairn-timeline/reports/<seq>.json relative to the ours
                // document's directory (ADR §2.5); seq = existing count + 1
                let dir = std::path::Path::new(ours)
                    .parent()
                    .unwrap_or(std::path::Path::new("."));
                let reports_dir = dir.join(".cairn-timeline").join("reports");
                std::fs::create_dir_all(&reports_dir).ok();
                let seq = std::fs::read_dir(&reports_dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).count())
                    .unwrap_or(0)
                    + 1;
                let report_path = reports_dir.join(format!("{seq}.json"));
                std::fs::write(
                    &report_path,
                    serde_json::to_string_pretty(&report.to_json())?,
                )
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", report_path.display()))?;
                println!("merged: {out}");
                println!("report: {}", report_path.display());
            }
            println!(
                "outcome: {:?} (applied={}, withheld={}, deduped={})",
                report.outcome, report.stats.applied, report.stats.withheld, report.stats.deduped
            );
            for v in &report.verdicts {
                println!(
                    "  C{:<2} {:<7} ours=[{}] theirs=[{:?}] {}",
                    v.class,
                    format!("{:?}", v.verdict),
                    v.ours,
                    v.theirs,
                    v.note
                );
            }
            std::process::exit(code);
        }
        Err(refusal) => {
            eprintln!("REFUSED: {}", refusal.0);
            eprintln!("no output written — both inputs remain untouched for the human");
            std::process::exit(3);
        }
    }
}
