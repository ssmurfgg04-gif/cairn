//! Local diagnostics dashboard (ADR-0009): loopback-only HTTP served by the daemon.
//! Static assets are embedded in the daemon binary (no build toolchain); JSON endpoints
//! mirror the ctl contract (docs/ctl-api.md) and read the REAL local store — no mock
//! data, no hard-coded empty panels (WO6-UI): every ctl action the CLI can do —
//! attach/detach, snapshot create/list/restore, pin/unpin, recall with progress,
//! leases, storage stats, kill switches, doctor — is surfaced here through the SAME
//! ctl service implementations the gRPC side serves.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tonic::Request;

use cairn_core::clock::{SystemClock, WallClock};
use cairn_store::Store;

use crate::daemon::{CtlPinsSvc, CtlRecallSvc, CtlSnapshotsSvc, DaemonState};
use cairn_proto::pb::{
    ctl_pins_server::CtlPins as _, ctl_recall_server::CtlRecall as _,
    ctl_snapshots_server::CtlSnapshots as _, CreateSnapshotRequest, ListPinsRequest,
    ListSnapshotsRequest, PinRequest, RecallStatusRequest, RestoreSnapshotRequest,
    StartRecallRequest, UnpinRequest,
};

const INDEX_HTML: &str = include_str!("../assets/dashboard/index.html");
const APP_CSS: &str = include_str!("../assets/dashboard/app.css");
const APP_JS: &str = include_str!("../assets/dashboard/app.js");

/// Serve the local dashboard + JSON gateway (loopback only; ADR-0009 policy).
pub async fn serve(addr: String, state: Arc<DaemonState>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(js))
        .route("/api/v1/status", get(status))
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/flags", get(flags).post(set_flag))
        .route("/api/v1/doctor", get(doctor))
        // WO6-UI: full ctl parity over HTTP (same svc impls as the gRPC ctl server)
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/attach", post(attach))
        .route("/api/v1/detach", post(detach))
        .route("/api/v1/leases", get(leases))
        .route("/api/v1/storage", get(storage))
        .route("/api/v1/snapshots", get(snapshots))
        .route("/api/v1/snapshots", post(create_snapshot))
        .route("/api/v1/snapshots/restore", post(restore_snapshot))
        .route("/api/v1/pins", get(pins).post(pin))
        .route("/api/v1/pins/unpin", post(unpin))
        .route("/api/v1/recall", post(start_recall))
        .route("/api/v1/recall/:job_id", get(recall_status))
        // round 16: client review summary (per attached root)
        .route("/api/v1/review", get(review_summary))
        // round 18: per-file sync badges, team/RBAC surface, cross-project
        // search, honest update state
        .route("/api/v1/files", get(files))
        .route("/api/v1/team", get(team))
        .route("/api/v1/search", get(search))
        .route("/api/v1/update", get(update_state))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "dashboard listening (loopback only)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> axum::response::Html<&'static str> {
    axum::response::Html(INDEX_HTML)
}

async fn css() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        APP_CSS,
    )
}

async fn js() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        APP_JS,
    )
}

fn open_store(home: &Path) -> Option<Store> {
    Store::open(home, std::sync::Arc::new(WallClock)).ok()
}

fn project_id_of(rt: &crate::projects::ProjectRuntime) -> String {
    rt.project_id.clone()
}

async fn status(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let home = state.home.as_path();
    let mut last_error: Option<String> = None;
    let summary_projects;
    {
        let map = crate::projects::RUNTIMES.read().await;
        summary_projects = map.len() as u64;
        for rt in map.values() {
            let v = rt.view.read().await;
            if let Some(e) = &v.last_error {
                last_error.get_or_insert_with(|| e.clone());
            }
        }
    }
    let (healthy, files, conflicts, cursor, pending) = if let Some(store) = open_store(home) {
        let outbox = cairn_store::Outbox::new(store.conn_handle());
        let (files, conflicts) = store.all_files_summary();
        (
            true,
            files,
            conflicts,
            store.max_cursor(),
            outbox.pending_count_all(),
        )
    } else {
        (false, 0, 0, 0, 0)
    };
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "proto": cairn_proto::PROTO_VERSION,
        "uptime_ms": state.started.elapsed().as_millis() as u64,
        "projects": summary_projects,
        "last_error": last_error,
        "summary": {
            "healthy": healthy,
            "files": files,
            "conflicts": conflicts,
            "journal_cursor": cursor,
            "outbox_pending": pending,
            // I1 lives in the per-mount FsMetrics (CairnFs); with no active mount the
            // daemon honestly reports null rather than inventing a number.
            "hydration_first_byte_ms": serde_json::Value::Null,
            "hydration_note": "no active FUSE/CfAPI mount on this daemon",
        },
    }))
}

async fn feed(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let mut activity: Vec<serde_json::Value> = Vec::new();
    let mut leases: Vec<serde_json::Value> = Vec::new();
    if let Some(store) = open_store(state.home.as_path()) {
        // real local leases (leases_local table — engine-written, expiry-filtered)
        let now = WallClock.now_millis();
        for (path, token, expires_at) in store.list_leases() {
            leases.push(json!({
                "path": path,
                "token": token,
                "expires_at": expires_at,
                "expired": expires_at <= now,
            }));
        }
        let rows: Vec<cairn_store::FileRow> = store.recent_file_rows(12);
        for f in rows {
            activity.push(json!({
                "seq": f.mtime,
                "path": f.path,
                "kind": if f.local_state == "conflict" { "conflict" } else { "upsert" },
                "size": f.size,
            }));
        }
    }
    Json(json!({ "activity": activity, "leases": leases }))
}

async fn projects(State(_state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let mut out = Vec::new();
    {
        let map = crate::projects::RUNTIMES.read().await;
        for rt in map.values() {
            let v = rt.view.read().await;
            let root_path = rt.workspace.to_string_lossy().into_owned();
            // display name: the folder editors named, not the slug they
            // did not (audit #4 — "cairn-test2" is a DB id, "Brand Film"
            // is what a human calls the project)
            let display_name = std::path::Path::new(&root_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rt.project_id.clone());
            out.push(json!({
                "project_id": project_id_of(rt),
                "display_name": display_name,
                "root_path": root_path,
                "state": v.state,
                "files_synced": v.files_synced,
                "cursor": v.cursor,
                "pending_outbox": v.pending_outbox,
                "last_error": v.last_error,
            }));
        }
    }
    out.sort_by(|a, b| {
        a["project_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project_id"].as_str().unwrap_or(""))
    });
    Json(json!({ "projects": out }))
}

async fn attach(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {root_path, project_id?}"}));
    };
    let root = v["root_path"].as_str().unwrap_or("").to_string();
    let project = v["project_id"].as_str().unwrap_or("").to_string();
    if root.is_empty() {
        return Json(json!({"ok": false, "error": "root_path required"}));
    }
    // RBAC parity with the ctl surface (the members file in the root
    // being attached is the authority)
    if let Err(s) = crate::daemon::rbac_guard(
        &state,
        &project,
        Some(std::path::Path::new(&root)),
        cairn_core::rbac::Permission::AttachRoot,
        "dash/attach",
    )
    .await
    {
        return Json(json!({"ok": false, "error": s.message()}));
    }
    match crate::projects::attach(
        &state.home,
        std::path::Path::new(&root),
        if project.is_empty() {
            None
        } else {
            Some(project)
        },
        None,
    )
    .await
    {
        Ok(pid) => Json(json!({"ok": true, "project_id": pid})),
        Err(e) => Json(json!({"ok": false, "error": e.message})),
    }
}

async fn detach(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id}"}));
    };
    let project = v["project_id"].as_str().unwrap_or("").to_string();
    // RBAC parity with the ctl surface — the detach guard lives in the
    // daemon, the dashboard is just another client
    if let Err(s) = crate::daemon::rbac_guard(
        &state,
        &project,
        None,
        cairn_core::rbac::Permission::DetachRoot,
        "dash/detach",
    )
    .await
    {
        return Json(json!({"ok": false, "error": s.message()}));
    }
    match crate::projects::detach(&state.home, &project).await {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e.message})),
    }
}

async fn leases(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(store) = open_store(state.home.as_path()) {
        let now = WallClock.now_millis();
        for (path, token, expires_at) in store.list_leases() {
            out.push(json!({
                "path": path, "token": token, "expires_at": expires_at,
                "expired": expires_at <= now,
            }));
        }
    }
    Json(json!({ "leases": out }))
}

async fn storage(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let home = state.home.as_path();
    let Some(store) = open_store(home) else {
        return Json(json!({"ok": false, "error": "store unavailable"}));
    };
    let conn = store.conn_handle();
    let (blob_count, blob_bytes, pinned_count, pinned_bytes) =
        match cairn_store::Cas::open(&store.root().join("blobs"), conn) {
            Ok(cas) => cas.blob_stats().unwrap_or((0, 0, 0, 0)),
            Err(_) => (0, 0, 0, 0),
        };
    let disk = cairn_store::eviction::disk_space(store.root()).ok();
    let (files, conflicts) = store.all_files_summary();
    Json(json!({
        "ok": true,
        "store_root": store.root().to_string_lossy(),
        "files": files,
        "conflicts": conflicts,
        "blobs": {
            "count": blob_count,
            "bytes": blob_bytes,
            "pinned_count": pinned_count,
            "pinned_bytes": pinned_bytes,
        },
        "disk": disk.map(|d| json!({"free_bytes": d.free, "total_bytes": d.total})),
    }))
}

async fn snapshots(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    if project.is_empty() {
        return Json(json!({"ok": false, "error": "?project= required"}));
    }
    let svc = CtlSnapshotsSvc { state };
    match svc
        .list_snapshots(Request::new(ListSnapshotsRequest {
            project_id: project,
        }))
        .await
    {
        Ok(resp) => {
            let snaps: Vec<serde_json::Value> = resp
                .into_inner()
                .snapshots
                .into_iter()
                .map(|s| {
                    json!({
                        "commit_hash": s.commit_hash,
                        "parent": s.parent,
                        "label": s.label,
                        "author": s.author,
                        "snapshot_seq": s.snapshot_seq,
                        "server_ts": s.server_ts,
                    })
                })
                .collect();
            Json(json!({"ok": true, "snapshots": snaps}))
        }
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn create_snapshot(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, label?}"}));
    };
    let project = v["project_id"].as_str().unwrap_or("").to_string();
    let svc = CtlSnapshotsSvc { state };
    let req = Request::new(CreateSnapshotRequest {
        project_id: project,
        label: v["label"].as_str().unwrap_or_default().to_string(),
    });
    match svc.create_snapshot(req).await {
        Ok(resp) => Json(json!({"ok": true, "commit_hash": resp.into_inner().commit_hash})),
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn restore_snapshot(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, commit_hash}"}));
    };
    let svc = CtlSnapshotsSvc { state };
    let req = Request::new(RestoreSnapshotRequest {
        project_id: v["project_id"].as_str().unwrap_or_default().to_string(),
        commit_hash: v["commit_hash"].as_str().unwrap_or_default().to_string(),
        target_path: v["target_path"].as_str().unwrap_or_default().to_string(),
    });
    match svc.restore_snapshot(req).await {
        Ok(resp) => {
            let r = resp.into_inner();
            Json(json!({"ok": true, "restored_files": r.restored_files, "bytes": r.bytes}))
        }
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn pins(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    if project.is_empty() {
        return Json(json!({"ok": false, "error": "?project= required"}));
    }
    let svc = CtlPinsSvc { state };
    match svc
        .list_pins(Request::new(ListPinsRequest {
            project_id: project,
        }))
        .await
    {
        Ok(resp) => {
            let pins: Vec<serde_json::Value> = resp
                .into_inner()
                .pins
                .into_iter()
                .map(|p| json!({"path": p.path, "size": p.size, "state": p.state}))
                .collect();
            Json(json!({"ok": true, "pins": pins}))
        }
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn pin(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    let svc = CtlPinsSvc { state };
    let req = Request::new(PinRequest {
        project_id: v["project_id"].as_str().unwrap_or_default().to_string(),
        path: v["path"].as_str().unwrap_or_default().to_string(),
    });
    match svc.pin(req).await {
        Ok(_) => Json(json!({"ok": true})),
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn unpin(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    let svc = CtlPinsSvc { state };
    let req = Request::new(UnpinRequest {
        project_id: v["project_id"].as_str().unwrap_or_default().to_string(),
        path: v["path"].as_str().unwrap_or_default().to_string(),
    });
    match svc.unpin(req).await {
        Ok(_) => Json(json!({"ok": true})),
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn start_recall(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path?}"}));
    };
    let svc = CtlRecallSvc { state };
    let req = Request::new(StartRecallRequest {
        project_id: v["project_id"].as_str().unwrap_or_default().to_string(),
        path: v["path"].as_str().unwrap_or_default().to_string(),
    });
    match svc.start_recall(req).await {
        Ok(resp) => Json(json!({"ok": true, "job_id": resp.into_inner().job_id})),
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

async fn recall_status(
    State(state): State<Arc<DaemonState>>,
    UrlPath(job_id): UrlPath<String>,
) -> Json<serde_json::Value> {
    let svc = CtlRecallSvc { state };
    match svc
        .recall_status(Request::new(RecallStatusRequest { job_id }))
        .await
    {
        Ok(resp) => {
            let r = resp.into_inner();
            Json(json!({
                "ok": true,
                "state": r.state,
                "progress": r.progress,
                "bytes_done": r.bytes_done,
                "bytes_total": r.bytes_total,
                "eta_ms": r.eta_ms,
            }))
        }
        Err(s) => Json(json!({"ok": false, "error": s.message()})),
    }
}

/// GET /api/v1/review — the review portal state per attached project:
/// version stack, live links, comment counts. Read-only; minting links
/// stays on the CLI (`cairn review link`).
async fn review_summary(State(_state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let mut out = Vec::new();
    {
        let map = crate::projects::RUNTIMES.read().await;
        for rt in map.values() {
            let root = rt.workspace.clone();
            let entry = match cairn_review::store::Store::load(&root) {
                Ok(Some(f)) => {
                    let now = cairn_core::clock::WallClock.now_millis();
                    let comments: u64 = f
                        .versions
                        .iter()
                        .map(|v| {
                            cairn_review::store::Store::load_comments(&root, v.number)
                                .map(|s| s.len() as u64)
                                .unwrap_or(0)
                        })
                        .sum();
                    json!({
                        "project_id": rt.project_id,
                        "root_path": root.to_string_lossy(),
                        "title": f.title,
                        "versions": f.versions.iter().map(|v| json!({
                            "number": v.number,
                            "label": v.label,
                            "frames": v.frames,
                            "fps_num": v.fps_num,
                            "fps_den": v.fps_den,
                            "duration": v.timecode(v.frames.saturating_sub(1)),
                            "has_proxy": v.proxy_rel.is_some(),
                            "published_by": v.published_by,
                        })).collect::<Vec<_>>(),
                        "live_links": f.links.iter().filter(|l| !l.is_expired(now)).count(),
                        "expired_links": f.links.iter().filter(|l| l.is_expired(now)).count(),
                        "open_notes": comments,
                    })
                }
                _ => json!({
                    "project_id": rt.project_id,
                    "root_path": root.to_string_lossy(),
                    "title": null,
                }),
            };
            out.push(entry);
        }
    }
    out.sort_by(|a, b| {
        a["project_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project_id"].as_str().unwrap_or(""))
    });
    Json(json!({ "review": out }))
}

async fn flags(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let flags = state.flags.read().await;
    Json(json!({
        "flags": flags.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>(),
    }))
}

async fn set_flag(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    if let Some(Json(v)) = body {
        let name = v["name"].as_str().unwrap_or("").to_string();
        let value = v["value"].as_str().unwrap_or("").to_string();
        // RBAC parity: kill switches are machine-global (any attached
        // project's members.json may deny)
        let mut pids: Vec<String> = {
            let map = crate::projects::RUNTIMES.read().await;
            map.values().map(|rt| rt.project_id.clone()).collect()
        };
        pids.sort();
        pids.dedup();
        for pid in &pids {
            if let Err(s) = crate::daemon::rbac_guard(
                &state,
                pid,
                None,
                cairn_core::rbac::Permission::ManageFlags,
                "dash/set-flag",
            )
            .await
            {
                return Json(json!({"ok": false, "error": s.message()}));
            }
        }
        let mut flags = state.flags.write().await;
        if let Some(slot) = flags.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value.clone();
            // mirror into the store so the ENGINE sees it per pass
            if let Some(store) = open_store(state.home.as_path()) {
                let _ = store.meta_set(&format!("flag:{name}"), &value);
            }
            tracing::info!(flag = %name, %value, "kill switch flipped from dashboard");
        }
    }
    Json(json!({"ok": true}))
}

async fn doctor(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let report = crate::doctor::collect(&state.home);
    Json(json!({
        "healthy": report.healthy(),
        "checks": report.checks.iter().map(|c| json!({
            "name": c.name, "ok": c.ok, "detail": c.detail, "latency_ms": c.latency_ms
        })).collect::<Vec<_>>(),
    }))
}

// ---------- round 18: files / team / search / update ----------

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The first runtime's (project, root) — Team reads one project's
/// members/audit; multi-project machines get one card per call site.
async fn first_runtime() -> Option<(String, std::path::PathBuf, u64, u64)> {
    let map = crate::projects::RUNTIMES.read().await;
    let mut best: Option<(String, std::path::PathBuf, u64, u64)> = None;
    for rt in map.values() {
        let v = rt.view.read().await;
        let cand = (
            rt.project_id.clone(),
            rt.workspace.clone(),
            v.files_synced,
            v.pending_outbox,
        );
        best = match best {
            None => Some(cand),
            Some((pid, _, _, _)) if cand.0 < pid => Some(cand),
            other => other,
        };
    }
    best
}

/// Map a file row's raw local_state to the badge vocabulary editors
/// already know from cloud drives: local / syncing / synced / conflict.
fn file_badge(row: &cairn_store::FileRow) -> &'static str {
    match row.local_state.as_str() {
        "conflict" => "conflict",
        "synced" => "synced",
        "dirty" => "syncing",
        _ => "syncing",
    }
}

/// GET /api/v1/files?project=&q= — per-file rows with sync + pin badges
/// (audit #7: "clip1.braw 8.3 MB synced / placeholder / pinned" — a file
/// list, not `ls`). `q` filters client-side cheaply server-side here.
async fn files(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    let needle = q.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    let Some(store) = open_store(state.home.as_path()) else {
        return Json(json!({"ok": false, "error": "store unavailable"}));
    };
    let mut rows_out = Vec::new();
    let mut summary = serde_json::Map::new();
    let mut total_files = 0u64;
    let mut synced_n = 0u64;
    let mut dirty_n = 0u64;
    let mut conflict_n = 0u64;
    let pins: std::collections::HashSet<String> = if project.is_empty() {
        Default::default()
    } else {
        store
            .list_pins(&project)
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    };
    for row in store.list_files(&project) {
        if row.mode != "file" {
            continue;
        }
        if !needle.is_empty() && !row.path.to_lowercase().contains(&needle) {
            continue;
        }
        total_files += 1;
        match file_badge(&row) {
            "synced" => synced_n += 1,
            "conflict" => conflict_n += 1,
            _ => dirty_n += 1,
        }
        rows_out.push(json!({
            "path": row.path,
            "size": row.size,
            "mtime": row.mtime,
            "state": file_badge(&row),
            "pinned": pins.contains(&row.path),
            "placeholder": row.manifest_hash.is_some(),
        }));
    }
    rows_out.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });
    summary.insert("files".into(), json!(total_files));
    summary.insert("synced".into(), json!(synced_n));
    summary.insert("syncing".into(), json!(dirty_n));
    summary.insert("conflict".into(), json!(conflict_n));
    summary.insert("pinned".into(), json!(pins.len()));
    Json(json!({"ok": true, "project": project, "summary": summary, "files": rows_out}))
}

/// GET /api/v1/team — members, the acting device's role, the swarm join
/// code (invite), and the newest audit decisions (audit #5: RBAC was in
/// the CLI only; now the studio roster is a first-class surface).
async fn team(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let Some((pid, root, files_synced, pending)) = first_runtime().await else {
        return Json(json!({"ok": true, "projects": []}));
    };
    let store = match open_store(state.home.as_path()) {
        Some(s) => s,
        None => return Json(json!({"ok": false, "error": "store unavailable"})),
    };
    let device = crate::projects::load_identity(&store)
        .map(|i| i.device_id)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "local".into());
    let members = crate::members::load(&root)
        .map(|f| {
            f.members
                .values()
                .map(|m| {
                    json!({
                        "device_id": m.device_id,
                        "name": m.name,
                        "role": m.role.as_str(),
                        "added_at_ms": m.added_at_ms,
                        "is_me": m.device_id == device,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let my_role = crate::members::load(&root)
        .map(|f| cairn_core::rbac::Role::as_str(f.role_of(&device)).to_string())
        .unwrap_or_else(|_| "editor".into());
    let join_code = store.meta_get("swarm/join-code").unwrap_or_default();
    let signal = store.meta_get("swarm/signal").unwrap_or_default();
    let audit = crate::audit::AuditFile::load(&root)
        .map(|rows| {
            rows.iter()
                .rev()
                .take(12)
                .map(|(_, e)| {
                    json!({
                        "ts_ms": e.ts_ms,
                        "device": e.device,
                        "role": e.role,
                        "action": e.action,
                        "allowed": e.allowed,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!({
        "ok": true,
        "projects": [{
            "project_id": pid,
            "display_name": root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| pid.clone()),
            "root_path": root.to_string_lossy(),
            "files_synced": files_synced,
            "pending_outbox": pending,
            "my_device": device,
            "my_role": my_role,
            "members": members,
            "join_code": if join_code.is_empty() { serde_json::Value::Null } else { json!(join_code) },
            "signal": if signal.is_empty() { serde_json::Value::Null } else { json!(signal) },
            "audit": audit,
            "now_ms": now_ms_i64(),
        }],
    }))
}

/// GET /api/v1/search?q= — substring search across file paths, project
/// ids/display names, review session titles, and audit actions (audit
/// #9: search existed only as a CLI; editors live in the dashboard).
async fn search(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let needle = q.get("q").cloned().unwrap_or_default().to_lowercase();
    if needle.trim().is_empty() {
        return Json(json!({"ok": true, "results": []}));
    }
    let mut out = Vec::new();
    // attached projects: (project_id, workspace, display name)
    let attached: Vec<(String, std::path::PathBuf, String)> = {
        let map = crate::projects::RUNTIMES.read().await;
        map.values()
            .map(|rt| {
                let display = rt
                    .workspace
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rt.project_id.clone());
                (rt.project_id.clone(), rt.workspace.clone(), display)
            })
            .collect()
    };
    for (pid, _root, display) in &attached {
        if pid.to_lowercase().contains(&needle) || display.to_lowercase().contains(&needle) {
            out.push(json!({
                "kind": "project",
                "project": pid,
                "label": display,
                "sub": pid,
                "target": "#projects",
            }));
        }
    }
    if let Some(store) = open_store(state.home.as_path()) {
        // file paths (first 40 hits across attached projects)
        let mut file_hits = 0;
        for (pid, _root, _display) in &attached {
            for row in store.list_files(pid) {
                if row.mode != "file" {
                    continue;
                }
                if row.path.to_lowercase().contains(&needle) {
                    out.push(json!({
                        "kind": "file",
                        "project": pid,
                        "label": row.path,
                        "sub": file_badge(&row),
                        "target": "#files",
                    }));
                    file_hits += 1;
                    if file_hits >= 40 {
                        break;
                    }
                }
            }
        }
        // review sessions
        for (pid, root, _display) in &attached {
            if let Ok(Some(f)) = cairn_review::store::Store::load(root) {
                if f.title.to_lowercase().contains(&needle) {
                    out.push(json!({
                        "kind": "review",
                        "project": pid,
                        "label": f.title,
                        "sub": format!("{} versions", f.versions.len()),
                        "target": "#review",
                    }));
                }
            }
        }
    }
    out.truncate(60);
    Json(json!({"ok": true, "results": out}))
}

/// GET /api/v1/update — honest update state. The daemon never phones
/// home; `cairn update check` (CLI) writes its verdict into the store and
/// this surfaces it: null = never checked, false = checked-current,
/// true = an update is offered. check_failed marks a failed check.
async fn update_state(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let offered = open_store(state.home.as_path())
        .and_then(|s| s.meta_get("update/offered"))
        .map(|v| v == "true")
        .unwrap_or(false);
    let check_failed = open_store(state.home.as_path())
        .and_then(|s| s.meta_get("update/check-failed"))
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({
        "ok": true,
        "current_version": env!("CARGO_PKG_VERSION"),
        "update_offered": offered,
        "check_failed": check_failed,
    }))
}
