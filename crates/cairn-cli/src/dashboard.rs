//! Local diagnostics dashboard (ADR-0009): loopback-only HTTP served by the daemon.
//! Static assets are embedded in the daemon binary (no build toolchain); JSON endpoints
//! mirror the ctl contract (docs/ctl-api.md) and read the REAL local store — no mock data.
//! Design: taste-skill minimalist profile (warm monochrome, 1px borders, serif/mono
//! contrast, transform/opacity-only motion).

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use cairn_core::clock::WallClock;
use cairn_store::Store;

use crate::daemon::DaemonState;

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

async fn status(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let home = state.home.as_path();
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
        "projects": [],
        "summary": {
            "healthy": healthy,
            "files": files,
            "conflicts": conflicts,
            "journal_cursor": cursor,
            "outbox_pending": pending,
            "hydration_first_byte_ms": serde_json::Value::Null,
        },
    }))
}

async fn feed(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    // honest data source: the local store's durable view; the cross-device journal tail
    // streams through ctl watch once a server is attached (ProjectService wiring, M8+)
    let mut activity: Vec<serde_json::Value> = Vec::new();
    let leases: Vec<serde_json::Value> = Vec::new();
    if let Some(store) = open_store(state.home.as_path()) {
        // file rows (local durable view) — top of journal activity panel
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
        let mut flags = state.flags.write().await;
        if let Some(slot) = flags.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value.clone();
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

// keep imports referenced (Cas surface used by doctor + future panels)
#[allow(unused_imports)]
use cairn_store::Cas as _CasUnused;
