//! Local diagnostics dashboard (ADR-0009): loopback-only HTTP served by the daemon.
//! The full UI ships with the UI phase; this module is the axum host + minimal JSON endpoints
//! the dashboard consumes. Endpoints mirror the ctl contract (docs/ctl-api.md).

use std::sync::Arc;

use axum::routing::get;
use axum::Json;
use serde_json::json;

use crate::daemon::DaemonState;

/// Serve the local dashboard + JSON gateway (loopback only; ADR-0009 policy).
pub async fn serve(addr: String, state: Arc<DaemonState>) -> anyhow::Result<()> {
    let app = axum::Router::new()
        .route("/", get(index))
        .route("/api/v1/status", get(status))
        .route("/api/v1/flags", get(flags))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "dashboard listening (loopback only)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> &'static str {
    "cairn daemon — dashboard assets load here (UI phase; ADR-0009)\n"
}

async fn status(axum::extract::State(state): axum::extract::State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "proto": cairn_proto::PROTO_VERSION,
        "uptime_ms": state.started.elapsed().as_millis() as u64,
        "projects": [],
    }))
}

async fn flags(axum::extract::State(state): axum::extract::State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let flags = state.flags.read().await;
    Json(json!({
        "flags": flags.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>(),
    }))
}
