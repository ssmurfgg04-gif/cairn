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
use axum::response::IntoResponse as _;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tonic::Request;

use cairn_core::clock::{SystemClock, WallClock};
use cairn_store::Store;

use crate::daemon::{CtlPinsSvc, CtlPresenceSvc, CtlRecallSvc, CtlSnapshotsSvc, DaemonState};
use cairn_proto::pb::{
    ctl_pins_server::CtlPins as _, ctl_presence_server::CtlPresence as _,
    ctl_recall_server::CtlRecall as _, ctl_snapshots_server::CtlSnapshots as _,
    CreateSnapshotRequest, ListPinsRequest, ListSnapshotsRequest, PinRequest, RecallStatusRequest,
    RestoreSnapshotRequest, SendPresenceRequest, StartRecallRequest, UnpinRequest,
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
        .route("/api/v1/activity", get(activity))
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
        // round 19: the NLE marker bridge — the same body the CLI exports,
        // served on loopback for the Premiere UXP panel (ADR-0022 follow-up)
        .route("/api/v1/markers", get(markers))
        // round 20 (ADR-0023 §2): live presence — SSE stream + submit +
        // snapshot. Same flag + RBAC gates as the ctl service (delegated,
        // never re-implemented — the set_flag drift lesson).
        .route("/api/v1/live", get(live_sse).post(live_send))
        .route("/api/v1/live/snapshot", get(live_snapshot))
        // round 27 (the "click, don't type" retro): the native folder
        // picker — Attach's first instinct is a CLICK. The daemon runs in
        // the user's interactive session, so the OS dialog shows on their
        // desktop; loopback-only, RBAC-free (picking a folder leaks
        // nothing — attaching it is still guarded).
        .route("/api/v1/pick-folder", get(pick_folder))
        // round 27: file quick-actions (hover row buttons)
        //   open      — reveal in Explorer/Finder/file manager
        //   download  — stream the materialized bytes to the browser
        //   duplicate — local copy beside the original (explicit action,
        //               never triggered by sync)
        .route("/api/v1/file/open", post(file_open))
        .route("/api/v1/file/download", get(file_download))
        .route("/api/v1/file/duplicate", post(file_duplicate))
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
    let mut swarm_summary = Vec::new();
    {
        let map = crate::projects::RUNTIMES.read().await;
        summary_projects = map.len() as u64;
        for rt in map.values() {
            let v = rt.view.read().await;
            if let Some(e) = &v.last_error {
                last_error.get_or_insert_with(|| e.clone());
            }
            drop(v);
            // WAN leg (ADR-0022 §5): per-project NAT metrics — the punch
            // success rate is the number the wan-p2p runbook reads off a
            // VPS box. Missing swarm (no --swarm-signal) stays absent, not
            // zeroed: honest absence over invented zeros.
            if let Some(swarm) = rt.swarm.lock().await.as_ref() {
                let s = swarm.stats();
                swarm_summary.push(json!({
                    "project": project_id_of(rt),
                    "peers": s.peers,
                    "direct_links": s.direct_links,
                    "relay_links": s.relay_links,
                    "stun_resolved": s.stun_resolved,
                    "punch_attempts": s.punch_attempts,
                    "punch_successes": s.punch_successes,
                }));
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
        // per-project swarm/NAT metrics (empty array = no swarm on this daemon)
        "swarm": swarm_summary,
    }))
}

async fn feed(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let mut activity: Vec<serde_json::Value> = Vec::new();
    let mut leases: Vec<serde_json::Value> = Vec::new();
    if let Some(store) = open_store(state.home.as_path()) {
        // real local leases (leases_local table — engine-written, expiry-filtered).
        // A HELD lease is the "project opened" event (ADR-0014: NLEs lock on
        // open), so it joins the activity timeline instead of existing in a
        // separate zone only the settings view can see.
        let now = WallClock.now_millis();
        for (path, token, expires_at) in store.list_leases() {
            leases.push(json!({
                "path": path,
                "token": token,
                "expires_at": expires_at,
                "expired": expires_at <= now,
            }));
            if expires_at > now {
                activity.push(json!({
                    "ts": now,
                    "seq": now,
                    "kind": "lease",
                    "path": path,
                    "project": "",
                }));
            }
        }
        // recent file events (mtime-ordered; the journal's file surface)
        let rows: Vec<cairn_store::FileRow> = store.recent_file_rows(10);
        for f in rows {
            activity.push(json!({
                "seq": f.mtime,
                "ts": f.mtime,
                "path": f.path,
                "project": f.project_id,
                "kind": if f.local_state == "conflict" { "conflict" } else { "upsert" },
                "state": f.local_state,
                "size": f.size,
            }));
        }
        // real pin events (pins table — durable intent, WO6-2)
        for (project, path, pinned_at) in store.recent_pins(6) {
            activity.push(json!({
                "ts": pinned_at,
                "seq": pinned_at,
                "path": path,
                "project": project,
                "kind": "pinned",
            }));
        }
        // newest first, capped: a timeline, not a log viewer
        activity.sort_by(|a, b| {
            b["ts"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["ts"].as_i64().unwrap_or(0))
        });
        activity.truncate(12);
    }
    Json(json!({ "activity": activity, "leases": leases }))
}

/// GET /api/v1/activity?tz_offset=&days= — the dashboard chart's data:
/// per-day byte totals for files touched in the window, day boundaries in
/// the CALLER's timezone (JS `getTimezoneOffset` convention) so the chart's
/// weekday labels match the user's clock. Reads the real store — no
/// invented series, no stub shape: an empty project renders an honest
/// empty chart, never a fake curve (round 25).
async fn activity(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let days = i64::from(
        q.get("days")
            .and_then(|d| d.parse::<u32>().ok())
            .unwrap_or(7)
            .clamp(1, 31),
    );
    let tz_offset = q
        .get("tz_offset")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(-14 * 60, 14 * 60);
    let now = WallClock.now_millis();
    let cutoff = now.saturating_sub(days.saturating_mul(86_400_000));
    let days_out: Vec<serde_json::Value> = if let Some(store) = open_store(state.home.as_path()) {
        store
                .daily_activity(cutoff, tz_offset)
                .into_iter()
                .map(|(start_ms, bytes, files)| {
                    json!({ "start_ms": start_ms, "bytes": bytes, "files": files })
                })
                .collect()
    } else {
        Vec::new()
    };
    Json(json!({
        "ok": true,
        "days": days_out,
        "window_days": days,
        "generated_ms": now,
    }))
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

    // round 27: the meter is REAL per volume — the home store's disk AND
    // every attached workspace's disk (a video studio's project drive is
    // rarely the system drive; "260/476 GB" with no volume label read as
    // a static mock). The UI labels which volume it is showing.
    let mut volumes = Vec::new();
    if let Some(d) = &disk {
        volumes.push(json!({
            "label": "store",
            "free_bytes": d.free,
            "total_bytes": d.total,
        }));
    }
    {
        let map = crate::projects::RUNTIMES.read().await;
        let mut seen = std::collections::HashSet::new();
        for rt in map.values() {
            if seen.insert(rt.workspace.clone()) {
                if let Ok(d) = cairn_store::eviction::disk_space(&rt.workspace) {
                    volumes.push(json!({
                        "label": rt.project_id,
                        "free_bytes": d.free,
                        "total_bytes": d.total,
                    }));
                }
            }
        }
    }
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
        "volumes": volumes,
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

/// GET /api/v1/markers?project=&version=&format=fcpxml|otio|csv — the NLE
/// marker bridge over the loopback gateway. The same body the CLI exports
/// (`cairn review export-markers`), so the Premiere UXP panel and the
/// terminal can never disagree. Read-only: comments live in the root's
/// machine-local `.cairn` dir (ADR-0022's honest-scope note); RBAC's write
/// boundary (attach/detach/flags) is untouched by a read.
async fn markers(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let project = q.get("project").cloned().unwrap_or_default();
    let version: u32 = q.get("version").and_then(|v| v.parse().ok()).unwrap_or(0);
    let format = q.get("format").cloned().unwrap_or_else(|| "fcpxml".into());

    // resolve the root: the named project's runtime, else the first (the
    // panel always names the project; the fallback keeps manual URL fetch
    // on single-project machines working)
    let root: Option<std::path::PathBuf> = {
        let map = crate::projects::RUNTIMES.read().await;
        map.values()
            .find(|rt| project.is_empty() || rt.project_id == project)
            .map(|rt| rt.workspace.clone())
    };
    let Some(root) = root else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("no attached project matches '{project}'")})),
        )
            .into_response();
    };
    // ADR-0028 §E: the panel exports "what the client gets" by default;
    // ?visibility=all is the studio's own view
    let vis = match q.get("visibility").map(String::as_str) {
        Some("all") => None,
        Some("internal") => Some(cairn_tl::notes::NoteVisibility::Internal),
        _ => Some(cairn_tl::notes::NoteVisibility::Public),
    };
    match crate::handoff::markers_payload(&root, version, &format, None, vis) {
        Ok((body, ctype)) => {
            let ext = match format.as_str() {
                "otio" => "otio",
                "csv" => "csv",
                _ => "fcpxml",
            };
            let headers = [
                (header::CONTENT_TYPE, ctype.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"markers-v{version}.{ext}\""),
                ),
            ];
            (StatusCode::OK, headers, body).into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
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
) -> axum::response::Response {
    // Round 20: DELEGATE to the ctl service (same code the gRPC surface
    // runs) — the local re-implementation drifted: unknown flag / missing
    // body returned {ok:true} on HTTP while gRPC answered NOT_FOUND, and the
    // store mirror ran inside the flags write lock.
    use cairn_proto::pb::ctl_diagnostics_server::CtlDiagnostics as _;
    let svc = crate::daemon::CtlDiagSvc {
        state: Arc::clone(&state),
    };
    let Some(Json(v)) = body else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "body required: {name, value}"})),
        )
            .into_response();
    };
    let name = v["name"].as_str().unwrap_or("").to_string();
    let value = v["value"].as_str().unwrap_or("").to_string();
    match svc
        .set_flag(tonic::Request::new(cairn_proto::pb::SetFlagRequest {
            name,
            value,
        }))
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(st) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": st.message()})),
        )
            .into_response(),
    }
}

async fn doctor(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    // cached (round 27): the dashboard polls this at 15s; the status RPC
    // shares the same 5s-fresh cache, so neither starves the ctl thread
    let report = state.cached_doctor().await;
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

// ---------- live presence (ADR-0023 §2) ----------

#[derive(serde::Deserialize)]
struct LiveSendBody {
    project: String,
    editor: String,
    frame: i64,
    #[serde(default = "default_rate")]
    rate: i64,
    #[serde(default = "default_action")]
    action: String,
}
fn default_rate() -> i64 {
    24
}
fn default_action() -> String {
    "playhead".into()
}

/// POST /api/v1/live — submit a presence event (playhead/drag/selection).
/// Delegates to the ctl service: same flag gate, same RBAC ledger entry,
/// same swarm relay. The payload is BUILT here (editor/frame/rate/action)
/// so JS callers stay schema-simple; the wire bound (1200 B) still applies.
async fn live_send(
    State(state): State<Arc<DaemonState>>,
    axum::Json(body): axum::Json<LiveSendBody>,
) -> axum::response::Response {
    let payload = serde_json::json!({
        "editor": body.editor,
        "frame": body.frame,
        "rate": body.rate,
        "action": body.action,
    });
    let svc = CtlPresenceSvc {
        state: Arc::clone(&state),
    };
    let req = tonic::Request::new(SendPresenceRequest {
        project: body.project,
        payload: serde_json::to_vec(&payload).unwrap_or_default(),
    });
    match svc.send_presence(req).await {
        Ok(_) => axum::Json(json!({ "ok": true })).into_response(),
        Err(st) => (
            axum::http::StatusCode::PRECONDITION_FAILED,
            axum::Json(json!({ "ok": false, "error": st.message() })),
        )
            .into_response(),
    }
}

/// GET /api/v1/live — SSE stream of presence events (dashboard live view).
/// 403-shape JSON error (not an error EVENT) when the flag is off — the
/// client shows the honest "presence off" chip instead of a dead stream.
async fn live_sse(State(state): State<Arc<DaemonState>>) -> axum::response::Response {
    let on = {
        state
            .flags
            .read()
            .await
            .iter()
            .any(|(k, v)| k == "live_presence" && v == "true")
    };
    if !on {
        return (
            axum::http::StatusCode::PRECONDITION_FAILED,
            axum::Json(json!({
                "ok": false,
                "error": "live presence is OFF on this device (flag live_presence)",
            })),
        )
            .into_response();
    }
    use tokio_stream::StreamExt as _;
    let rx = crate::projects::PRESENCE_TX.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => {
            let data = ev.to_json().to_string();
            Some(Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default().data(data),
            ))
        }
        Err(_) => None, // Lagged — presence is a signal, not a log
    });
    use axum::response::sse::{KeepAlive, Sse};
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/v1/live/snapshot — the current presence view per project
/// (remote peers from each swarm's last-event-wins map; local events are
/// stream-only). Honest `enabled` field so the UI can show the off state.
async fn live_snapshot(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let on = {
        state
            .flags
            .read()
            .await
            .iter()
            .any(|(k, v)| k == "live_presence" && v == "true")
    };
    let mut projects_out = Vec::new();
    {
        let map = crate::projects::RUNTIMES.read().await;
        for rt in map.values() {
            if let Some(swarm) = rt.swarm.lock().await.as_ref() {
                let events: Vec<serde_json::Value> = swarm
                    .presence_snapshot()
                    .into_iter()
                    .map(|ev| {
                        json!({
                            "from": ev.from,
                            "payload": String::from_utf8_lossy(&ev.payload),
                        })
                    })
                    .collect();
                projects_out.push(json!({
                    "project": project_id_of(rt),
                    "events": events,
                }));
            }
        }
    }
    Json(json!({ "enabled": on, "projects": projects_out }))
}

// ---------------------------------------------------------------------------
// round 27 — "click, don't type": the native folder picker + file
// quick-actions. The install retro's sharpest finding: a user's first
// instinct on the attach scene is to CLICK a button, not to paste a
// path. The daemon serves loopback-only; the OS dialog belongs to the
// user's interactive session (installer/tray both start it there).
// ---------------------------------------------------------------------------

/// GET /api/v1/pick-folder — open the OS folder dialog, return the chosen
/// path. `cancelled: true` when the user closes it without choosing (NOT
/// an error — the UI falls back to the text input). `unsupported: true`
/// on hosts with no session dialog (the UI keeps the text field front
/// and center instead of offering a dead button). A dialog cannot run
/// on the async runtime's thread (STA COM + modal), so it runs on the
/// blocking pool; the request stays open until the user decides.
async fn pick_folder() -> Json<serde_json::Value> {
    // cairn-fs-win owns the FFI (the badge/cfapi boundary pattern);
    // cairn-cli stays forbid(unsafe_code) — the dialog is a SAFE call
    let picked = tokio::task::spawn_blocking(cairn_fs_win::dialog::pick_folder).await;
    match picked {
        Ok(cairn_fs_win::dialog::Picked::Folder(path)) => Json(json!({"ok": true, "path": path})),
        // user closed the dialog — not an error
        Ok(cairn_fs_win::dialog::Picked::Cancelled) => Json(json!({"ok": true, "cancelled": true})),
        Ok(cairn_fs_win::dialog::Picked::Unsupported) => {
            Json(json!({"ok": true, "unsupported": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": format!("picker failed: {e}")})),
    }
}

/// Resolve a project-relative path inside the project's attached root,
/// refusing traversal (`..`, absolute paths, drive letters, UNC). The
/// quick-actions must never become an arbitrary-file-read primitive.
fn safe_join(root: &Path, rel: &str) -> Option<std::path::PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    // components like "..", reserved device names, drive-letter colons
    for comp in rel_path.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            std::path::Component::CurDir => {}
            _ => return None, // ParentDir, Prefix (C:), RootDir, UNC
        }
    }
    let joined = root.join(rel_path);
    // belt and braces: canonicalize (when it exists) and re-check prefix
    if let Ok(canon) = joined.canonicalize() {
        let root_canon = root.canonicalize().ok()?;
        if !canon.starts_with(&root_canon) {
            return None;
        }
        Some(canon)
    } else {
        Some(joined)
    }
}

/// The live root of a project id (first runtime with that id).
async fn project_root_path(_state: &DaemonState, project_id: &str) -> Option<std::path::PathBuf> {
    let map = crate::projects::RUNTIMES.read().await;
    map.values()
        .find(|rt| rt.project_id == project_id)
        .map(|rt| rt.workspace.clone())
}

/// POST /api/v1/file/open {project_id, path} — reveal the file in the OS
/// file manager (Explorer /select on Windows, xdg-open the parent dir
/// elsewhere). Errors are honest JSON, not silent.
async fn file_open(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    let project = v["project_id"].as_str().unwrap_or("").to_string();
    let path = v["path"].as_str().unwrap_or("").to_string();
    let Some(root) = project_root_path(&state, &project).await else {
        return Json(json!({"ok": false, "error": "project not attached"}));
    };
    let Some(full) = safe_join(&root, &path) else {
        return Json(json!({"ok": false, "error": "path refused (traversal)"}));
    };
    // platform reveal; the bool/str pair keeps one return site so both
    // cfg targets compile identically
    let (shown, why) = reveal_in_file_manager(&full, &root);
    Json(json!({"ok": shown, "error": why}))
}

/// Reveal a file in the OS file manager. Windows: `explorer /select`
/// highlights the file in its folder. Others: open the parent directory
/// (the file may be a placeholder — the folder is still the useful view).
fn reveal_in_file_manager(full: &Path, root: &Path) -> (bool, &'static str) {
    #[cfg(windows)]
    {
        let _ = root; // reveal-by-select needs only the file itself
        let ok = std::process::Command::new("explorer.exe")
            .arg(format!("/select,\"{}\"", full.display()))
            .spawn()
            .is_ok();
        (ok, if ok { "" } else { "explorer failed to start" })
    }
    #[cfg(not(windows))]
    {
        // xdg-open the parent directory (the file may be a placeholder —
        // opening the folder is still the useful view)
        let dir = full.parent().unwrap_or(root).to_path_buf();
        let ok = std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .is_ok();
        (ok, if ok { "" } else { "xdg-open failed" })
    }
}

/// GET /api/v1/file/download?project=..&path=.. — stream the LOCAL
/// materialized bytes to the browser as an attachment. A placeholder (not
/// materialized) answers 409 with the recall hint: downloading 50 GB of
/// BRAW through the browser is the user's explicit choice, but it
/// requires the bytes to be here first.
async fn file_download(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::body::{Body, Bytes};
    use axum::http::{header, HeaderValue, StatusCode};

    let project = q.get("project").cloned().unwrap_or_default();
    let path = q.get("path").cloned().unwrap_or_default();
    let err = |code: StatusCode, msg: &str| {
        (code, Json(json!({"ok": false, "error": msg}))).into_response()
    };
    let Some(root) = project_root_path(&state, &project).await else {
        return err(StatusCode::NOT_FOUND, "project not attached");
    };
    let Some(full) = safe_join(&root, &path) else {
        return err(StatusCode::BAD_REQUEST, "path refused (traversal)");
    };
    let meta = match tokio::fs::metadata(&full).await {
        Ok(m) => m,
        Err(_) => {
            return err(
                StatusCode::CONFLICT,
                "file is not materialized on this machine — recall it first, then download",
            )
        }
    };
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "not a file");
    }
    let f = match tokio::fs::File::open(&full).await {
        Ok(f) => f,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "open failed"),
    };
    // stream in 256 KiB chunks — memory stays flat for 50 GB BRAW
    let stream = futures::stream::unfold(f, |mut f| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
            Ok(0) => None,
            Ok(n) => Some((Ok(Bytes::from(buf[..n].to_vec())), f)),
            Err(e) => Some((Err(std::io::Error::other(e)), f)),
        }
    });
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut resp = axum::response::Response::new(Body::from_stream(stream));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(cd) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        name.replace('"', "")
    )) {
        headers.insert(header::CONTENT_DISPOSITION, cd);
    }
    if let Ok(cl) = HeaderValue::from_str(&meta.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, cl);
    }
    resp
}

/// POST /api/v1/file/duplicate {project_id, path} — local copy beside the
/// original (`name (copy).ext`), never synced until the watcher picks it
/// up like any other new file (it IS a new file — the explicit-action
/// semantics the retro asked for). Placeholders answer the recall hint:
/// duplicating a 0-byte placeholder would create a 0-byte file.
async fn file_duplicate(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    let project = v["project_id"].as_str().unwrap_or("").to_string();
    let path = v["path"].as_str().unwrap_or("").to_string();
    let Some(root) = project_root_path(&state, &project).await else {
        return Json(json!({"ok": false, "error": "project not attached"}));
    };
    let Some(full) = safe_join(&root, &path) else {
        return Json(json!({"ok": false, "error": "path refused (traversal)"}));
    };
    if !tokio::fs::metadata(&full)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return Json(
            json!({"ok": false, "error": "file is not materialized on this machine — recall it first"}),
        );
    }
    // `clip.braw` -> `clip (copy).braw`; `README` -> `README (copy)`
    let stem = full.with_extension("");
    let ext = full
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = stem;
    let mut n = 1;
    let mut candidate = {
        let base = format!("{} (copy)", dest.display());
        if ext.is_empty() {
            std::path::PathBuf::from(base)
        } else {
            std::path::PathBuf::from(format!("{base}.{ext}"))
        }
    };
    while tokio::fs::metadata(&candidate).await.is_ok() && n < 100 {
        n += 1;
        let base = format!("{} (copy {})", dest.display(), n);
        candidate = if ext.is_empty() {
            std::path::PathBuf::from(base)
        } else {
            std::path::PathBuf::from(format!("{base}.{ext}"))
        };
    }
    match tokio::fs::copy(&full, &candidate).await {
        Ok(bytes) => {
            let rel = candidate
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| candidate.to_string_lossy().into_owned());
            Json(json!({"ok": true, "path": rel, "bytes": bytes}))
        }
        Err(e) => Json(json!({"ok": false, "error": format!("copy failed: {e}")})),
    }
}
