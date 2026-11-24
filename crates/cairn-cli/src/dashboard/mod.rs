//! Local diagnostics dashboard (ADR-0009): loopback-only HTTP served by the daemon.
//! Static assets are embedded in the daemon binary (no build toolchain); JSON endpoints
//! mirror the ctl contract (docs/ctl-api.md) and read the REAL local store — no mock
//! data, no hard-coded empty panels (WO6-UI): every ctl action the CLI can do —
//! attach/detach, snapshot create/list/restore, pin/unpin, recall with progress,
//! leases, storage stats, kill switches, doctor — is surfaced here through the SAME
//! ctl service implementations the gRPC side serves.
//!
//! ROUND 29 (ADR-0030) — this file is now an HTTP ADAPTER, nothing more:
//! router + security gate + thin parse/respond handlers. The behavior —
//! RBAC, store reads, review writes, merges, compression — lives in the
//! `service` module, where the tray, the NLE panel, and tests can reach the
//! same operations without an HTTP client.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::response::IntoResponse as _;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::daemon::DaemonState;
use crate::service;

mod middleware;
use middleware::{file_ticket, idempotency, request_id, security_gate, usage, usage_count};

const INDEX_HTML: &str = include_str!("../../assets/dashboard/index.html");
const APP_CSS: &str = include_str!("../../assets/dashboard/app.css");
const APP_JS: &str = include_str!("../../assets/dashboard/app.js");

/// Per-launch dashboard API token (P0 hardening, mom-test round): the
/// daemon mints one uuid v4 per dashboard start and injects it into the
/// served page (`window.CAIRN_TOKEN`). Every `/api` request must present
/// it — header `x-cairn-token` (fetch), or `?t=` for the two clients that
/// cannot set headers: the presence EventSource and browser download
/// links. Loopback-only binding alone is NOT a boundary (DNS rebinding +
/// drive-by POSTs ride the user's own browser); host, origin, and token
/// gates below make the local API loopback-USER-only.
#[derive(Clone)]
struct DashToken(std::sync::Arc<String>);

/// Serve the local dashboard + JSON gateway (loopback only; ADR-0009 policy).
pub async fn serve(addr: String, state: Arc<DaemonState>) -> anyhow::Result<()> {
    let token = DashToken(std::sync::Arc::new(
        uuid::Uuid::new_v4().simple().to_string(),
    ));
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(js))
        .route("/api/v1/status", get(status))
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/activity", get(activity))
        .route("/api/v1/flags", get(flags).post(set_flag))
        .route("/api/v1/doctor", get(doctor))
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/quit", post(quit))
        .route("/api/v1/storage/free", post(storage_free))
        .route("/api/v1/team/default-role", post(team_default_role))
        .route("/api/v1/usage", get(usage))
        .route("/api/v1/file/ticket", post(file_ticket))
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
        // picker — Attach's first instinct is a CLICK.
        .route("/api/v1/pick-folder", get(pick_folder))
        // round 27: file quick-actions (hover row buttons)
        //   open      — reveal in Explorer/Finder/file manager
        //   download  — stream the materialized bytes to the browser
        //   duplicate — local copy beside the original (explicit action,
        //               never triggered by sync)
        .route("/api/v1/file/open", post(file_open))
        .route("/api/v1/file/download", get(file_download))
        .route("/api/v1/file/duplicate", post(file_duplicate))
        .route("/api/v1/team/regenerate", post(team_regenerate))
        .route("/api/v1/team/join", post(team_join))
        .route("/api/v1/team/code/revoke", post(team_code_revoke))
        .route("/api/v1/review/publish", post(review_publish))
        .route("/api/v1/review/link", post(review_link))
        .route("/api/v1/review/revoke", post(review_revoke))
        // ADR-0031 Phase 1 read surface: the raw synced record table
        // (query: project, family) — the merged view of local publishes +
        // everything replayed from peers.
        .route("/api/v1/state-records", get(state_records))
        // CONTRACT-DEBT #1: the conflict auto-offer — accept lands the merged
        // timeline (one journal entry), decline keeps the conflict copy.
        .route("/api/v1/merge/offer/accept", post(merge_offer_accept))
        .route("/api/v1/merge/offer/decline", post(merge_offer_decline))
        .route("/api/v1/tl-merge", post(tl_merge))
        .route("/api/v1/compress", post(compress))
        // Proxy preview copies (contract freeze §1): make small editing
        // copies of big camera media from the UI. Parse/respond ONLY — the
        // service layer owns validation, RBAC, the in-flight guard, and the
        // transcode; the security gate below covers these routes like the rest.
        .route("/api/v1/proxy/generate", post(proxy_generate))
        .route("/api/v1/proxy/status", get(proxy_status))
        // P0: host + origin + token gates for EVERY route (the layer wraps
        // all routes added above). Order matters: layer before with_state.
        .layer(axum::middleware::from_fn_with_state(
            token.clone(),
            security_gate,
        ))
        // #95: local-only feature counts (never sent anywhere)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            usage_count,
        ))
        // #42: replayed POSTs (double clicks, retries) apply once
        .layer(axum::middleware::from_fn(idempotency))
        // #94: outermost, so even gate denials carry a support reference.
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "dashboard listening (loopback only)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The page itself: inject the per-launch token where app.js can read it
/// (the `%%CAIRN_TOKEN%%` placeholder lives in index.html <head>). Static
/// assets stay token-free so the page loads; every /api call carries it.
///
/// Response hardening (review #62/#63): the page embeds a per-launch
/// secret, so it is `no-store` (a cached page would outlive its token's
/// daemon and never re-mint) and carries a self-hosted CSP — the fonts
/// beacon is gone, so everything resolves from 'self' and the policy
/// blocks any future injected remote host. `frame-ancestors 'none'` keeps
/// other local apps from iframing the console; `nosniff`/`no-referrer`
/// are the cheap belt-and-braces.
async fn index(axum::Extension(token): axum::Extension<DashToken>) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    let body = INDEX_HTML.replace("%%CAIRN_TOKEN%%", &token.0);
    (
        StatusCode::OK,
        [
            // text/html MUST be explicit: with `nosniff` below, a browser
            // renders a missing/`text/plain` type as source text — the live
            // merge-offer dogfood caught exactly that (the hardening round's
            // header array replaced the type-bearing one).
            (
                header::CONTENT_TYPE,
                "text/html; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'self'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response()
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

// ---------------------------------------------------------------------------
// Thin adapters: parse → service → respond. No business logic here.
// ---------------------------------------------------------------------------

async fn status(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::status(&state).await)
}

async fn feed(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::feed(&state))
}

async fn activity(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let days = q
        .get("days")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(7)
        .clamp(1, 31);
    let tz_offset = q
        .get("tz_offset")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(-14 * 60, 14 * 60);
    Json(service::views::activity(&state, days, tz_offset))
}

async fn projects(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::projects(&state).await)
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
    match service::actions::attach_root(&state, &root, &project).await {
        Ok(j) => Json(j),
        Err(j) => Json(j),
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
    match service::actions::detach_project(&state, &project).await {
        Ok(j) => Json(j),
        Err(j) => Json(j),
    }
}

async fn leases(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::leases(&state))
}

async fn storage(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::storage(&state).await)
}

async fn snapshots(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    if project.is_empty() {
        return Json(json!({"ok": false, "error": "?project= required"}));
    }
    match service::actions::list_snapshots(&state, &project).await {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn create_snapshot(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, label?}"}));
    };
    match service::actions::create_snapshot(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["label"].as_str().unwrap_or_default(),
    )
    .await
    {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn restore_snapshot(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, commit_hash}"}));
    };
    match service::actions::restore_snapshot(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["commit_hash"].as_str().unwrap_or_default(),
        v["target_path"].as_str().unwrap_or_default(),
    )
    .await
    {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
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
    match service::actions::list_pins(&state, &project).await {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn pin(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    match service::actions::pin_path(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    )
    .await
    {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn unpin(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    match service::actions::unpin_path(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    )
    .await
    {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn start_recall(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path?}"}));
    };
    match service::actions::start_recall(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    )
    .await
    {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn recall_status(
    State(state): State<Arc<DaemonState>>,
    UrlPath(job_id): UrlPath<String>,
) -> Json<serde_json::Value> {
    match service::actions::recall_status(&state, &job_id).await {
        Ok(j) => Json(j),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn review_summary(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::review_summary(&state).await)
}

async fn markers(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};

    let project = q.get("project").cloned().unwrap_or_default();
    let version: u32 = q.get("version").and_then(|v| v.parse().ok()).unwrap_or(0);
    let format = q.get("format").cloned().unwrap_or_else(|| "fcpxml".into());
    // ADR-0028 §E: the panel exports "what the client gets" by default;
    // ?visibility=all is the studio's own view
    let vis = match q.get("visibility").map(String::as_str) {
        Some("all") => None,
        Some("internal") => Some(cairn_tl::notes::NoteVisibility::Internal),
        _ => Some(cairn_tl::notes::NoteVisibility::Public),
    };
    match service::views::markers_export(&state, &project, version, &format, vis).await {
        service::Export::Ready {
            body,
            content_type,
            filename,
        } => {
            let headers = [
                (header::CONTENT_TYPE, content_type),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ];
            (StatusCode::OK, headers, body).into_response()
        }
        service::Export::NotFound(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

async fn flags(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::flags(&state).await)
}

async fn set_flag(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    let Some(Json(v)) = body else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "body required: {name, value}"})),
        )
            .into_response();
    };
    let name = v["name"].as_str().unwrap_or("").to_string();
    let value = v["value"].as_str().unwrap_or("").to_string();
    match service::actions::set_flag(&state, &name, &value).await {
        Ok(j) => Json(j).into_response(),
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

async fn team_default_role(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, role}"}));
    };
    Json(
        service::actions::team_default_role(
            &state,
            v["project_id"].as_str().unwrap_or_default(),
            v["role"].as_str().unwrap_or_default(),
        )
        .await,
    )
}

async fn storage_free(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::actions::storage_free(&state).await)
}

/// #58 POST /api/v1/quit — stop syncing and exit the daemon (token-gated
/// like every /api route, so only the local user's console or `cairn stop`
/// can reach it). Answers first, exits a beat later so the reply lands.
async fn quit() -> Json<serde_json::Value> {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        crate::daemon::QUIT.notify_one();
    });
    Json(json!({"ok": true, "quitting": true}))
}

async fn diagnostics(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::diagnostics(&state).await)
}

async fn doctor(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::doctor(&state).await)
}

async fn files(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    let needle = q.get("q").cloned().unwrap_or_default();
    let offset = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|v| v.parse().ok());
    let media_only = q.get("kind").is_some_and(|k| k == "media");
    Json(
        service::views::files_page_kind(&state, &project, &needle, offset, limit, media_only).await,
    )
}

async fn state_records(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut project = q.get("project").cloned().unwrap_or_default();
    // An unnamed project reads the first attached one (the team/markers
    // convention — single-project machines should not have to pass ids);
    // nothing attached keeps the empty id, and the view answers honestly.
    if project.is_empty() {
        if let Some((pid, _, _, _)) = state.projects.first().await {
            project = pid;
        }
    }
    let family = q.get("family").cloned().unwrap_or_default();
    Json(service::views::state_records(&state, &project, &family))
}

async fn team(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    Json(service::views::team(&state, &project).await)
}

async fn search(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let needle = q.get("q").cloned().unwrap_or_default();
    Json(service::views::search(&state, &needle).await)
}

async fn update_state(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::update_state(&state))
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
    match service::actions::live_send(&state, &body.project, payload).await {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            axum::http::StatusCode::PRECONDITION_FAILED,
            axum::Json(json!({ "ok": false, "error": e })),
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
    let rx = state.projects.subscribe_presence();
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

async fn live_snapshot(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(service::views::live_snapshot(&state).await)
}

// ---------------------------------------------------------------------------
// round 27 — "click, don't type": the native folder picker + file
// quick-actions (policy in the service; the OS-side effects too).
// ---------------------------------------------------------------------------

async fn pick_folder() -> Json<serde_json::Value> {
    Json(service::actions::pick_folder().await)
}

async fn file_open(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    Json(
        service::actions::file_open(
            &state,
            v["project_id"].as_str().unwrap_or_default(),
            v["path"].as_str().unwrap_or_default(),
        )
        .await,
    )
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
    let (full, name, len) = match service::actions::resolve_download(&state, &project, &path).await
    {
        service::Download::Ready { full, name, len } => (full, name, len),
        service::Download::ProjectNotAttached => {
            return err(StatusCode::NOT_FOUND, "project not attached")
        }
        service::Download::TraversalRefused => {
            return err(StatusCode::BAD_REQUEST, "path refused (traversal)")
        }
        service::Download::NotMaterialized => {
            return err(
                StatusCode::CONFLICT,
                "file is not materialized on this machine — recall it first, then download",
            )
        }
        service::Download::NotAFile => return err(StatusCode::BAD_REQUEST, "not a file"),
    };
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
    if let Ok(cl) = HeaderValue::from_str(&len.to_string()) {
        headers.insert(header::CONTENT_LENGTH, cl);
    }
    resp
}

async fn file_duplicate(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    Json(
        service::actions::file_duplicate(
            &state,
            v["project_id"].as_str().unwrap_or_default(),
            v["path"].as_str().unwrap_or_default(),
        )
        .await,
    )
}

/// POST /api/v1/merge/offer/accept {project_id, path} — thin adapter over
/// `actions::merge_offer_accept` (CONTRACT-DEBT #1). All behavior — the
/// engine resolution, the merge, the copy removal — lives in the service.
async fn merge_offer_accept(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    Json(
        service::actions::merge_offer_accept(
            &state,
            v["project_id"].as_str().unwrap_or_default(),
            v["path"].as_str().unwrap_or_default(),
        )
        .await,
    )
}

/// POST /api/v1/merge/offer/decline {project_id, path} — thin adapter over
/// `actions::merge_offer_decline`.
async fn merge_offer_decline(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, path}"}));
    };
    Json(service::actions::merge_offer_decline(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    ))
}

async fn team_regenerate(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    // optional {project_id} — empty falls back to the first attached project
    let project = body
        .and_then(|Json(v)| v["project_id"].as_str().map(str::to_string))
        .unwrap_or_default();
    Json(service::actions::team_regenerate(&state, &project).await)
}

async fn team_join(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {code}"}));
    };
    Json(service::actions::team_join(&state, v["code"].as_str().unwrap_or("")).await)
}

/// POST /api/v1/team/code/revoke {code} — kill the current join code NOW
/// (review #4; the service action enforces ManageMembers).
async fn team_code_revoke(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {code}"}));
    };
    Json(service::actions::team_revoke_code(&state, v["code"].as_str().unwrap_or_default()).await)
}

async fn review_publish(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, media}"}));
    };
    let r = service::actions::review_publish(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["media"].as_str().unwrap_or_default(),
        v["title"].as_str().unwrap_or_default(),
        v["frames"].as_u64(),
        v["fps"].as_str(),
    )
    .await;
    service::views::invalidate_view("review:"); // #54: fresh after the mutation
    Json(r)
}

async fn review_link(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let v = body.map(|Json(j)| j).unwrap_or(json!({}));
    let r = service::actions::review_link(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["note"].as_str().unwrap_or("Client"),
        v["role"].as_str().unwrap_or("commenter"),
        // contract freeze #2: optional ttl_hours, default 720 (=30
        // days); the service action clamps to 1..=8760
        v["ttl_hours"].as_i64().unwrap_or(720),
    )
    .await;
    service::views::invalidate_view("review:"); // #54: fresh after the mutation
    Json(r)
}

async fn review_revoke(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {token}"}));
    };
    let r = service::actions::review_revoke(
        &state,
        v["project_id"].as_str().unwrap_or_default(),
        v["token"].as_str().unwrap_or_default(),
    )
    .await;
    service::views::invalidate_view("review:"); // #54: fresh after the mutation
    Json(r)
}

async fn tl_merge(body: Option<Json<serde_json::Value>>) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(
            json!({"ok": false, "error": "body required: {base_otio, ours_otio, theirs_otio}"}),
        );
    };
    Json(
        service::actions::tl_merge(
            v["base_otio"].as_str().unwrap_or_default(),
            v["ours_otio"].as_str().unwrap_or_default(),
            v["theirs_otio"].as_str().unwrap_or_default(),
            v["semantic"].as_bool().unwrap_or(false),
        )
        .await,
    )
}

async fn compress(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project_id, media, preset?}"}));
    };
    Json(
        service::actions::compress(
            &state,
            v["project_id"].as_str().unwrap_or_default(),
            v["media"].as_str().unwrap_or_default(),
            v["preset"].as_str().unwrap_or("proxy540"),
            v["via"].as_str(),
        )
        .await,
    )
}

/// POST /api/v1/proxy/generate {project, path} — generate an editing proxy.
/// The service answers Ok/Err JSON; the adapter maps ONLY status codes:
/// "in_progress" → 409 (a second click while one encode runs), everything
/// else → 400 (caller-fixable input/refusal). Never a 500 from business logic.
async fn proxy_generate(
    State(state): State<Arc<DaemonState>>,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let Some(Json(v)) = body else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "body required: {project, path}"})),
        )
            .into_response();
    };
    let j = service::proxy::proxy_generate(
        &state,
        v["project"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    )
    .await;
    if j["ok"] == serde_json::Value::Bool(true) {
        return Json(j).into_response();
    }
    let code = if j["error"] == "in_progress" {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (code, Json(j)).into_response()
}

/// GET /api/v1/proxy/status?project= — the proxy index as rows.
async fn proxy_status(
    State(state): State<Arc<DaemonState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let project = q.get("project").cloned().unwrap_or_default();
    Json(service::proxy::proxy_status(&state, &project).await)
}
