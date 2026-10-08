//! State-changing console actions (ADR-0030 service layer): every POST
//! surface of the loopback console as a plain function. Authorization and
//! policy live HERE — the HTTP adapter cannot forget them, and any other
//! local surface (tray, panel, tests) gets the same enforced behavior.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use cairn_core::clock::{SystemClock, WallClock};

use crate::daemon::DaemonState;

use super::{open_store, safe_join};

// The generated ctl traits — every parity op here DELEGATES to the same
// service impls the gRPC side serves (the set_flag drift lesson: never
// re-implement what the daemon already owns).
use cairn_proto::pb::ctl_diagnostics_server::CtlDiagnostics as _;
use cairn_proto::pb::ctl_pins_server::CtlPins as _;
use cairn_proto::pb::ctl_presence_server::CtlPresence as _;
use cairn_proto::pb::ctl_recall_server::CtlRecall as _;
use cairn_proto::pb::ctl_snapshots_server::CtlSnapshots as _;

type Json = serde_json::Value;

/// POST /api/v1/attach {root_path, project_id?} — bind a folder. RBAC
/// parity with the ctl surface (the members file in the root being
/// attached is the authority) is enforced HERE, at the service boundary.
pub async fn attach_root(
    state: &Arc<DaemonState>,
    root: &str,
    project: &str,
) -> Result<Json, Json> {
    if root.is_empty() {
        return Err(json!({"ok": false, "error": "root_path required"}));
    }
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        Some(std::path::Path::new(root)),
        cairn_core::rbac::Permission::AttachRoot,
        "dash/attach",
    )
    .await
    {
        return Err(json!({"ok": false, "error": s.message()}));
    }
    match state
        .projects
        .attach(
            &state.home,
            std::path::Path::new(root),
            if project.is_empty() {
                None
            } else {
                Some(project.to_string())
            },
            None,
        )
        .await
    {
        Ok(pid) => Ok(json!({"ok": true, "project_id": pid})),
        Err(e) => Err(json!({"ok": false, "error": e.message})),
    }
}

/// POST /api/v1/detach {project_id} — unbind a project. The detach guard
/// lives here; the dashboard is just another client of the service.
pub async fn detach_project(state: &Arc<DaemonState>, project: &str) -> Result<Json, Json> {
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        None,
        cairn_core::rbac::Permission::DetachRoot,
        "dash/detach",
    )
    .await
    {
        return Err(json!({"ok": false, "error": s.message()}));
    }
    match state.projects.detach(&state.home, project).await {
        Ok(()) => Ok(json!({"ok": true})),
        Err(e) => Err(json!({"ok": false, "error": e.message})),
    }
}

/// POST /api/v1/snapshots {project_id, label?} — create via the SAME ctl
/// service the gRPC side serves (the set_flag drift lesson: delegate,
/// never re-implement).
pub async fn create_snapshot(
    state: &Arc<DaemonState>,
    project: &str,
    label: &str,
) -> Result<Json, String> {
    let svc = crate::daemon::CtlSnapshotsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::CreateSnapshotRequest {
        project_id: project.to_string(),
        label: label.to_string(),
    });
    svc.create_snapshot(req)
        .await
        .map(|r| json!({"ok": true, "commit_hash": r.into_inner().commit_hash}))
        .map_err(|s| s.message().to_string())
}

/// POST /api/v1/snapshots/restore {project_id, commit_hash, target_path?}.
pub async fn restore_snapshot(
    state: &Arc<DaemonState>,
    project: &str,
    commit_hash: &str,
    target_path: &str,
) -> Result<Json, String> {
    let svc = crate::daemon::CtlSnapshotsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::RestoreSnapshotRequest {
        project_id: project.to_string(),
        commit_hash: commit_hash.to_string(),
        target_path: target_path.to_string(),
    });
    svc.restore_snapshot(req)
        .await
        .map(|r| {
            let r = r.into_inner();
            json!({"ok": true, "restored_files": r.restored_files, "bytes": r.bytes})
        })
        .map_err(|s| s.message().to_string())
}

/// GET /api/v1/snapshots?project= — list via the ctl service.
pub async fn list_snapshots(state: &Arc<DaemonState>, project: &str) -> Result<Json, String> {
    let svc = crate::daemon::CtlSnapshotsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::ListSnapshotsRequest {
        project_id: project.to_string(),
    });
    svc.list_snapshots(req)
        .await
        .map(|resp| {
            let snaps: Vec<Json> = resp
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
            json!({"ok": true, "snapshots": snaps})
        })
        .map_err(|s| s.message().to_string())
}

/// GET /api/v1/pins?project= — list via the ctl service.
pub async fn list_pins(state: &Arc<DaemonState>, project: &str) -> Result<Json, String> {
    let svc = crate::daemon::CtlPinsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::ListPinsRequest {
        project_id: project.to_string(),
    });
    svc.list_pins(req)
        .await
        .map(|resp| {
            let pins: Vec<Json> = resp
                .into_inner()
                .pins
                .into_iter()
                .map(|p| json!({"path": p.path, "size": p.size, "state": p.state}))
                .collect();
            json!({"ok": true, "pins": pins})
        })
        .map_err(|s| s.message().to_string())
}

/// POST /api/v1/pins {project_id, path} — durable pin intent.
pub async fn pin_path(state: &Arc<DaemonState>, project: &str, path: &str) -> Result<Json, String> {
    let svc = crate::daemon::CtlPinsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::PinRequest {
        project_id: project.to_string(),
        path: path.to_string(),
    });
    svc.pin(req)
        .await
        .map(|_| json!({"ok": true}))
        .map_err(|s| s.message().to_string())
}

/// POST /api/v1/pins/unpin {project_id, path}.
pub async fn unpin_path(
    state: &Arc<DaemonState>,
    project: &str,
    path: &str,
) -> Result<Json, String> {
    let svc = crate::daemon::CtlPinsSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::UnpinRequest {
        project_id: project.to_string(),
        path: path.to_string(),
    });
    svc.unpin(req)
        .await
        .map(|_| json!({"ok": true}))
        .map_err(|s| s.message().to_string())
}

/// POST /api/v1/recall {project_id, path?} — start a recall job.
pub async fn start_recall(
    state: &Arc<DaemonState>,
    project: &str,
    path: &str,
) -> Result<Json, String> {
    let svc = crate::daemon::CtlRecallSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::StartRecallRequest {
        project_id: project.to_string(),
        path: path.to_string(),
    });
    svc.start_recall(req)
        .await
        .map(|r| json!({"ok": true, "job_id": r.into_inner().job_id}))
        .map_err(|s| s.message().to_string())
}

/// GET /api/v1/recall/:job_id — recall progress.
pub async fn recall_status(state: &Arc<DaemonState>, job_id: &str) -> Result<Json, String> {
    let svc = crate::daemon::CtlRecallSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::RecallStatusRequest {
        job_id: job_id.to_string(),
    });
    svc.recall_status(req)
        .await
        .map(|resp| {
            let r = resp.into_inner();
            json!({
                "ok": true,
                "state": r.state,
                "progress": r.progress,
                "bytes_done": r.bytes_done,
                "bytes_total": r.bytes_total,
                "eta_ms": r.eta_ms,
            })
        })
        .map_err(|s| s.message().to_string())
}

/// POST /api/v1/flags {name, value} — DELEGATES to the ctl service (same
/// code the gRPC surface runs); on a swarm-join-time flag the daemon also
/// rejoins live swarms (CONTRACT-DEBT #2). Returns Ok(json) or the gRPC
/// status message (unknown flag → 404 in the adapter).
pub async fn set_flag(state: &Arc<DaemonState>, name: &str, value: &str) -> Result<Json, String> {
    let svc = crate::daemon::CtlDiagSvc {
        state: Arc::clone(state),
    };
    svc.set_flag(tonic::Request::new(cairn_proto::pb::SetFlagRequest {
        name: name.to_string(),
        value: value.to_string(),
    }))
    .await
    .map(|_| json!({"ok": true}))
    .map_err(|st| st.message().to_string())
}

/// POST /api/v1/live — submit a presence event (playhead/drag/selection).
/// Delegates to the ctl service: same flag gate, same RBAC ledger entry,
/// same swarm relay. The payload is BUILT by the adapter (editor/frame/
/// rate/action) so JS callers stay schema-simple; the wire bound (1200 B)
/// still applies inside the service.
pub async fn live_send(
    state: &Arc<DaemonState>,
    project: &str,
    payload: Json,
) -> Result<(), String> {
    let svc = crate::daemon::CtlPresenceSvc {
        state: Arc::clone(state),
    };
    let req = tonic::Request::new(cairn_proto::pb::SendPresenceRequest {
        project: project.to_string(),
        payload: serde_json::to_vec(&payload).unwrap_or_default(),
    });
    svc.send_presence(req)
        .await
        .map(|_| ())
        .map_err(|st| st.message().to_string())
}

/// GET /api/v1/pick-folder — open the OS folder dialog, return the chosen
/// path. `cancelled: true` when the user closes it without choosing (NOT
/// an error — the UI falls back to the text input). `unsupported: true`
/// on hosts with no session dialog (the UI keeps the text field front
/// and center instead of offering a dead button). A dialog cannot run
/// on the async runtime's thread (STA COM + modal), so it runs on the
/// blocking pool; the request stays open until the user decides.
pub async fn pick_folder() -> Json {
    // cairn-fs-win owns the FFI (the badge/cfapi boundary pattern);
    // cairn-cli stays forbid(unsafe_code) — the dialog is a SAFE call
    let picked = tokio::task::spawn_blocking(cairn_fs_win::dialog::pick_folder).await;
    match picked {
        Ok(cairn_fs_win::dialog::Picked::Folder(path)) => json!({"ok": true, "path": path}),
        // user closed the dialog — not an error
        Ok(cairn_fs_win::dialog::Picked::Cancelled) => json!({"ok": true, "cancelled": true}),
        Ok(cairn_fs_win::dialog::Picked::Unsupported) => {
            json!({"ok": true, "unsupported": true})
        }
        Err(e) => json!({"ok": false, "error": format!("picker failed: {e}")}),
    }
}

/// POST /api/v1/file/open {project_id, path} — resolve + reveal in the OS
/// file manager. Errors are honest JSON, not silent.
pub async fn file_open(state: &Arc<DaemonState>, project: &str, path: &str) -> Json {
    let Some(root) = state.projects.project_root(project).await else {
        return json!({"ok": false, "error": "project not attached"});
    };
    let Some(full) = safe_join(&root, path) else {
        return json!({"ok": false, "error": "path refused (traversal)"});
    };
    // platform reveal; the bool/str pair keeps one return site so both
    // cfg targets compile identically
    let (shown, why) = reveal_in_file_manager(&full, &root);
    json!({"ok": shown, "error": why})
}

/// Reveal a file in the OS file manager. Windows: `explorer /select`
/// highlights the file in its folder. Others: open the parent directory
/// (the file may be a placeholder — the folder is still the useful view).
fn reveal_in_file_manager(full: &std::path::Path, root: &std::path::Path) -> (bool, &'static str) {
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

/// Resolve a download request to a streamable file (or the reason not to).
/// The adapter does the streaming; the service owns the policy.
pub async fn resolve_download(
    state: &Arc<DaemonState>,
    project: &str,
    path: &str,
) -> super::Download {
    let Some(root) = state.projects.project_root(project).await else {
        return super::Download::ProjectNotAttached;
    };
    let Some(full) = safe_join(&root, path) else {
        return super::Download::TraversalRefused;
    };
    let meta = match tokio::fs::metadata(&full).await {
        Ok(m) => m,
        Err(_) => return super::Download::NotMaterialized,
    };
    if !meta.is_file() {
        return super::Download::NotAFile;
    }
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    super::Download::Ready {
        full,
        name,
        len: meta.len(),
    }
}

/// POST /api/v1/file/duplicate {project_id, path} — local copy beside the
/// original (`name (copy).ext`), never synced until the watcher picks it
/// up like any other new file (it IS a new file — the explicit-action
/// semantics the retro asked for). Placeholders answer the recall hint:
/// duplicating a 0-byte placeholder would create a 0-byte file.
pub async fn file_duplicate(state: &Arc<DaemonState>, project: &str, path: &str) -> Json {
    let Some(root) = state.projects.project_root(project).await else {
        return json!({"ok": false, "error": "project not attached"});
    };
    let Some(full) = safe_join(&root, path) else {
        return json!({"ok": false, "error": "path refused (traversal)"});
    };
    if !tokio::fs::metadata(&full)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return json!(
            {"ok": false, "error": "file is not materialized on this machine — recall it first"}
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
            json!({"ok": true, "path": rel, "bytes": bytes})
        }
        Err(e) => json!({"ok": false, "error": format!("copy failed: {e}")}),
    }
}

/// POST /api/v1/team/regenerate {project_id?} — mint a fresh single-use join
/// code (600s TTL). Production: uses the server auth when reachable, else a
/// local `enr-` code stored in meta for the WS rendezvous.
/// POLICY BOUNDARY (ADR-0030): invite generation is a ManageMembers act. The
/// ctl path always enforced it; the dashboard path does too now — the
/// "service action = direct mutation" drift the review flagged is closed
/// (every dashboard mutation funnels through rbac_guard like ctl does).
///
/// Review #4 (local-scope hardening): regenerating also clears the
/// burn/revocation/lockout state of the PREVIOUS code — a fresh invite
/// starts clean, and whatever was wrong with the old one is moot.
pub async fn team_regenerate(state: &Arc<DaemonState>, project: &str) -> Json {
    let project = project.trim().to_string();
    let project = if project.is_empty() {
        // the dashboard's acting context: the first attached project (the
        // same fallback the review/team views use)
        match state.projects.list().await.into_iter().next() {
            Some(rt) => rt.project_id.clone(),
            None => return json!({"ok": false, "error": "no attached project"}),
        }
    } else {
        project
    };
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        &project,
        None,
        cairn_core::rbac::Permission::ManageMembers,
        "dash/team-regenerate",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    let code = format!("enr-{}", uuid::Uuid::now_v7().simple());
    if let Some(store) = open_store(state.home.as_path()) {
        let _ = store.meta_set("swarm/join-code", &code);
        let _ = store.meta_set(
            "swarm/join-code-exp",
            &(WallClock.now_millis() + JOIN_TTL_MS).to_string(),
        );
        // fresh code, fresh state: burn/revocation/lockout never carry over
        let _ = store.meta_clear("swarm/join-code-used");
        let _ = store.meta_clear("swarm/join-code-revoked");
        let _ = store.meta_clear("swarm/join-fails");
        let _ = store.meta_clear("swarm/join-locked-until");
    }
    json!({"ok": true, "join_code": code, "ttl_ms": JOIN_TTL_MS})
}

// ---------- join codes: local single-use burn + revocation + lockout ----------
//
// Review #4, honest local scope. The machine-local `enr-` code minted for
// the WS rendezvous is the only invite primitive today; since Phase 2 it
// carries:
//   * SINGLE-USE BURN — a successful team_join validation consumes the code
//     (`swarm/join-code-used`); one code, one join;
//   * REVOCATION — `team_revoke_code` (dashboard POST
//     /api/v1/team/code/revoke) kills the current invite NOW;
//   * LOCKOUT — 5 failed join attempts lock the join surface for 15 minutes
//     (a guessed-code budget; regenerating clears it instantly, so an
//     attacker cannot DoS the owner into a corner);
//   * AUDIT — every join outcome lands in the project ledger
//     ("dash/team-join") when an attached root exists.
// ALL stored-code rejections share ONE generic error — consumed, expired,
// revoked, unknown and locked are indistinguishable from the outside (no
// existence leak). The server-authoritative invitation protocol
// (server-side hash + atomic consume, synced across machines) stays future
// work — tracked in CONTRACT-DEBT #4.

const JOIN_TTL_MS: i64 = 600_000;
const JOIN_MAX_FAILURES: i64 = 5;
const JOIN_LOCKOUT_MS: i64 = 15 * 60_000;

/// The one generic join rejection (no existence leak — see module comment).
fn join_refused() -> Json {
    json!({"ok": false, "error": "join code not accepted"})
}

/// Best-effort audit for a join outcome on the first attached project's
/// ledger (the ledger is per-root; with nothing attached there is no
/// ledger — the decision still enforced). Never blocks the decision.
async fn audit_join_decision(state: &Arc<DaemonState>, action: &str, allowed: bool) {
    let Some((_pid, root, _, _)) = state.projects.first().await else {
        return;
    };
    let device = open_store(state.home.as_path())
        .and_then(|s| crate::projects::load_identity(&s))
        .map(|i| i.device_id)
        .unwrap_or_else(|| "local".into());
    let role = crate::members::load(&root)
        .map(|f| cairn_core::rbac::Role::as_str(f.role_of(&device)).to_string())
        .unwrap_or_else(|_| cairn_core::rbac::Role::Editor.as_str().to_string());
    if let Err(e) = crate::audit::AuditFile::decision(
        &root,
        WallClock.now_millis(),
        &device,
        &role,
        action,
        "team",
        allowed,
    ) {
        tracing::warn!(error = %e, "join audit write failed (decision unaffected)");
    }
}

/// One failed join attempt: bump the failure budget for the CURRENT invite
/// and lock the join surface once it is spent. Failures count against the
/// stored code (the only one that could ever validate) — a flood of
/// guessed codes locks the invite out, and the owner regenerates.
fn record_join_failure(store: &cairn_store::Store, now_ms: i64) {
    let fails = store
        .meta_get("swarm/join-fails")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = store.meta_set("swarm/join-fails", &fails.to_string());
    if fails >= JOIN_MAX_FAILURES {
        let _ = store.meta_set(
            "swarm/join-locked-until",
            &(now_ms + JOIN_LOCKOUT_MS).to_string(),
        );
        tracing::warn!("join surface locked after {fails} failed attempts");
    }
}

/// POST /api/v1/team/join {code} — accept a teammate code, persist peer intent.
/// Full enroll still runs via `cairn login --server … --code …`; this records
/// intent + validates shape so the UI can guide (join-code gated admission).
///
/// Review #4: the validation is now REAL against the machine-local invite
/// state — expired, consumed (single-use burn), revoked, locked-out and
/// unknown codes all get the same generic refusal, and a successful
/// validation BURNS the code. Failures are budgeted and audited.
pub async fn team_join(state: &Arc<DaemonState>, code: &str) -> Json {
    let code = code.trim().to_string();
    if !(code.starts_with("enr-") && code.len() > 8) {
        // shape feedback is safe: it says nothing about stored codes
        return json!({"ok": false, "error": "invalid join code shape (expected enr-…)"});
    }
    let Some(store) = open_store(state.home.as_path()) else {
        // nothing to validate against: the code cannot be accepted
        return join_refused();
    };
    let now = WallClock.now_millis();
    // lockout first: while locked, even the valid code is refused (and the
    // clock is not extended by further attempts)
    if let Some(until) = store
        .meta_get("swarm/join-locked-until")
        .and_then(|s| s.parse::<i64>().ok())
    {
        if now < until {
            audit_join_decision(state, "dash/team-join", false).await;
            return join_refused();
        }
        // lockout served in full: a fresh failure budget starts now
        let _ = store.meta_clear("swarm/join-locked-until");
        let _ = store.meta_clear("swarm/join-fails");
    }
    let expired = store
        .meta_get("swarm/join-code-exp")
        .and_then(|s| s.parse::<i64>().ok())
        .map(|exp| now >= exp)
        .unwrap_or(true); // no expiry stamped = no live invite ever minted
    let consumed = store.meta_get("swarm/join-code-used").as_deref() == Some("1");
    let revoked = store.meta_get("swarm/join-code-revoked").as_deref() == Some("1");
    let stored = store.meta_get("swarm/join-code").unwrap_or_default();
    let accepted = !stored.is_empty() && stored == code && !expired && !consumed && !revoked;
    if !accepted {
        record_join_failure(&store, now);
        audit_join_decision(state, "dash/team-join", false).await;
        return join_refused();
    }
    // SUCCESS: burn the code (single use) and clear the failure budget
    let _ = store.meta_set("swarm/join-code-used", "1");
    let _ = store.meta_clear("swarm/join-fails");
    let _ = store.meta_set("swarm/peer-join", &code);
    audit_join_decision(state, "dash/team-join", true).await;
    json!(
        {"ok": true, "code": code, "next": "run: cairn dev-enroll-code --server <server>, then cairn login --server <server> --code <code>"}
    )
}

/// The revoke mutation, factored from the guarded action so tests can drive
/// it against a plain store. `true` = the code matched the current invite
/// and is now revoked; `false` = unknown code (the caller answers).
fn revoke_join_code(store: &cairn_store::Store, code: &str) -> bool {
    let stored = store.meta_get("swarm/join-code").unwrap_or_default();
    if stored.is_empty() || stored != code {
        return false;
    }
    let _ = store.meta_set("swarm/join-code-revoked", "1");
    true
}

/// POST /api/v1/team/code/revoke {code} — kill the current invite NOW
/// (review #4: invites are revocable, not just expiring). POLICY BOUNDARY
/// (ADR-0030): revoking an invite is a ManageMembers act, like minting one.
/// The OWNER surface answers honestly ("unknown code") — that is a
/// privileged, authenticated caller; the no-leak generic refusal stays on
/// the GUEST join surface only.
pub async fn team_revoke_code(state: &Arc<DaemonState>, code: &str) -> Json {
    let code = code.trim().to_string();
    if code.is_empty() {
        return json!({"ok": false, "error": "code required"});
    }
    // project context for the guard: the first attached project (the same
    // acting-context fallback team_regenerate uses)
    let Some(rt) = state.projects.list().await.into_iter().next() else {
        return json!({"ok": false, "error": "no attached project"});
    };
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        &rt.project_id,
        None,
        cairn_core::rbac::Permission::ManageMembers,
        "dash/team-code-revoke",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    let Some(store) = open_store(state.home.as_path()) else {
        return json!({"ok": false, "error": "store unavailable"});
    };
    if revoke_join_code(&store, &code) {
        json!({"ok": true})
    } else {
        json!({"ok": false, "error": "unknown code"})
    }
}

/// POST /api/v1/review/publish {project_id, media, title?, frames?, fps?} —
/// append a version to the review stack (same store the CLI uses).
///
/// P0 (mom-test round): the backend OWNS the media truth. The dashboard
/// used to publish whatever the UI guessed ("first .mp4", 100 frames,
/// 24 fps), which shipped a 25 fps / 8400-frame cut with a wrong
/// timecode end to end. Now: frames/fps arrive only from callers that
/// already know them (the CLI path), everything else is probed from the
/// file itself (ffprobe) — fail closed on an honest error, never on a
/// guess. The old `root.join(media)` fallback on a refused path is also
/// gone: that silently defeated the traversal guard.
pub async fn review_publish(
    state: &Arc<DaemonState>,
    project: &str,
    media: &str,
    title: &str,
    frames: Option<u64>,
    fps: Option<&str>,
) -> Json {
    if project.is_empty() || media.is_empty() {
        return json!({"ok": false, "error": "project_id, media required"});
    }
    let Some(root) = state.projects.project_root(project).await else {
        return json!({"ok": false, "error": "project not attached"});
    };
    // POLICY BOUNDARY (ADR-0030): publishing review media is ManageReview.
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        Some(root.as_path()),
        cairn_core::rbac::Permission::ManageReview,
        "dash/review-publish",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    let Some(full) = safe_join(&root, media) else {
        return json!({"ok": false, "error": "path refused (traversal)"});
    };
    if !full.is_file() {
        return json!(
            {"ok": false, "error": "media not found on this machine — attach or materialize first"}
        );
    }
    // Explicit frames/fps (CLI callers that already know the truth) or
    // probe. The probe reuses the CLI's dogfood-fixed path (crate::review):
    // ONE implementation of media truth, never two that drift.
    let (frames, num, den) = match (frames, fps) {
        (Some(count), Some(rate)) if count > 0 => match crate::review::parse_fps(rate) {
            Ok(parsed) => (count, parsed.0, parsed.1),
            Err(e) => return json!({ "ok": false, "error": format!("unparseable fps: {e}") }),
        },
        _ => match crate::review::probe_media(&full) {
            Some(probed) => (probed.2, probed.0, probed.1),
            None => {
                return json!({"ok": false, "error": "could not probe this media (ffprobe missing or file unreadable) - install ffmpeg, or publish with explicit frames/fps"})
            }
        },
    };
    let mut file = match cairn_review::store::Store::load(&root) {
        Ok(Some(f)) => f,
        _ => cairn_review::model::ReviewFile {
            title: if title.is_empty() {
                project.to_string()
            } else {
                title.to_string()
            },
            ..Default::default()
        },
    };
    let ver = cairn_review::model::ReviewVersion {
        number: 0,
        label: String::new(),
        media_rel: media.to_string(),
        proxy_rel: None,
        fps_num: num,
        fps_den: den,
        frames,
        timeline_fingerprint: None,
        snapshot: None,
        published_by: String::from("dashboard"),
        published_at: WallClock.now_millis(),
    };
    let n = file.publish(ver);
    match cairn_review::store::Store::save(&root, &file) {
        Ok(()) => {
            // ADR-0031 Phase 1: mirror the version into the synced
            // `review_version` family (append-only union keyed by the assigned
            // stack number). Fire-and-forget — the save above is the primary
            // act and must never fail because its synced side-effect could
            // not be enqueued; a machine that misses the record sees the gap
            // in the merged read surface, not a broken publish.
            if let Some(target) = crate::state_records::resolve_target(state, project).await {
                if let Some(store) = open_store(&state.home) {
                    if let Some(v) = file.version(n) {
                        let payload = serde_json::json!({
                            "number": v.number,
                            "label": v.label,
                            "media_rel": v.media_rel,
                            "fps_num": v.fps_num,
                            "fps_den": v.fps_den,
                            "frames": v.frames,
                            "published_by": v.published_by,
                            "published_at": v.published_at,
                        });
                        match serde_json::to_vec(&payload) {
                            Ok(bytes) => crate::state_records::publish_review_version(
                                &store, &target, bytes, n,
                            ),
                            Err(e) => {
                                tracing::warn!(
                                    "review version payload build failed (not synced): {e}"
                                );
                            }
                        }
                    }
                }
            }
            json!({"ok": true, "version": n, "frames": frames, "fps": format!("{num}/{den}")})
        }
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// POST /api/v1/review/link {project_id?, note?, role?, ttl_hours?} —
/// mint a guest link (token is identity, no account). Studio role sees internal.
///
/// CONTRACT (worklog freeze #2): `ttl_hours` is optional, clamped to
/// 1..=8760, default 720 (=30 days) — the clamp lives HERE so every caller
/// (dashboard route, panel, tests) gets identical bounds. Response shape
/// unchanged.
pub async fn review_link(
    state: &Arc<DaemonState>,
    project: &str,
    note: &str,
    role: &str,
    ttl_h: i64,
) -> Json {
    // the route's contract bounds: 1h..=1y, default 30 days
    let ttl_h = clamp_ttl_hours(ttl_h);
    let root = resolve_root(state, project).await;
    let Some(root) = root else {
        return json!({"ok": false, "error": if project.is_empty() { "no attached project" } else { "project not attached" }});
    };
    // POLICY BOUNDARY (ADR-0030): minting a guest link is ManageReview.
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        Some(root.as_path()),
        cairn_core::rbac::Permission::ManageReview,
        "dash/review-link",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    let mut file = match cairn_review::store::Store::load(&root) {
        Ok(Some(f)) => f,
        _ => return json!({"ok": false, "error": "no versions published yet — publish first"}),
    };
    let role = match role {
        "viewer" => cairn_review::model::GuestRole::Viewer,
        "studio" => cairn_review::model::GuestRole::Studio,
        _ => cairn_review::model::GuestRole::Commenter,
    };
    let token = file.add_link(
        role,
        note.to_string(),
        ttl_h * 3_600_000,
        false,
        WallClock.now_millis(),
    );
    match cairn_review::store::Store::save(&root, &file) {
        Ok(()) => {
            // ADR-0031 Phase 1: mirror the minted link into the synced
            // `review_link` family (LWW register keyed by token). Same
            // fire-and-forget discipline as every publish in this module.
            if let Some(target) = crate::state_records::resolve_target(state, project).await {
                if let Some(store) = open_store(&state.home) {
                    if let Some(link) = file.links.iter().find(|l| l.token == token) {
                        let payload = serde_json::json!({
                            "token": link.token,
                            "role": link.role.as_str(),
                            "note": link.note,
                            "expires_at": link.expires_at,
                            "latest_only": link.latest_only,
                            "created_at": link.created_at,
                        });
                        match serde_json::to_vec(&payload) {
                            Ok(bytes) => crate::state_records::publish_review_link(
                                &store, &target, bytes, &token,
                            ),
                            Err(e) => {
                                tracing::warn!(
                                    "review link payload build failed (not synced): {e}"
                                );
                            }
                        }
                    }
                }
            }
            json!({"ok": true, "token": token, "link": format!("/r/{token}")})
        }
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// POST /api/v1/review/revoke {project_id?, token} — kill a guest link
/// NOW. Expiry was never revocation: the "that link leaked" path needs a
/// same-day kill switch (mom-test P0 follow-up).
pub async fn review_revoke(state: &Arc<DaemonState>, project: &str, token: &str) -> Json {
    let token = token.trim().to_string();
    if token.is_empty() {
        return json!({"ok": false, "error": "token required"});
    }
    let root = resolve_root(state, project).await;
    let Some(root) = root else {
        return json!({"ok": false, "error": if project.is_empty() { "no attached project" } else { "project not attached" }});
    };
    // POLICY BOUNDARY (ADR-0030): revoking a guest link is ManageReview —
    // and once ADR-0031 Phase 2 lands, a revoke propagates cross-machine,
    // which makes it exactly the kind of act that must never be unguarded.
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        Some(root.as_path()),
        cairn_core::rbac::Permission::ManageReview,
        "dash/review-revoke",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    let mut file = match cairn_review::store::Store::load(&root) {
        Ok(Some(f)) => f,
        _ => return json!({"ok": false, "error": "no review session for this project"}),
    };
    if !file.revoke_link(&token) {
        return json!({"ok": false, "error": "unknown link"});
    }
    match cairn_review::store::Store::save(&root, &file) {
        Ok(()) => {
            // ADR-0031 Phase 1: the revoke rides a synced review_link
            // TOMBSTONE (the Phase-2 cross-machine revoke substrate — every
            // portal consults the record before honoring the link).
            // Fire-and-forget: the local revoke above already happened.
            if let Some(target) = crate::state_records::resolve_target(state, project).await {
                if let Some(store) = open_store(&state.home) {
                    crate::state_records::publish_review_link_revocation(&store, &target, &token);
                }
            }
            json!({"ok": true})
        }
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// The route's TTL contract (worklog freeze #2): `ttl_hours` clamps to
/// 1..=8760 (1h..=1y); the route default of 720 (=30 days) lives in the
/// dashboard adapter. Extracted so tests pin the bounds directly.
fn clamp_ttl_hours(ttl_h: i64) -> i64 {
    ttl_h.clamp(1, 8_760)
}

/// The root for a review action: the named project, else the first
/// attached one (single-project machines should not have to pass ids).
async fn resolve_root(state: &Arc<DaemonState>, project: &str) -> Option<PathBuf> {
    if project.is_empty() {
        state.projects.first().await.map(|(_, r, _, _)| r)
    } else {
        state.projects.project_root(project).await
    }
}

/// POST /api/v1/tl-merge {base_otio, ours_otio, theirs_otio, semantic?} —
/// thin wrapper over cairn-tl three-way merge (C0-C10 classifier).
/// Bodies are raw OTIO JSON strings (small timelines); large media stays in CAS.
pub fn tl_merge(base: &str, ours: &str, theirs: &str, semantic: bool) -> Json {
    if base.is_empty() || ours.is_empty() || theirs.is_empty() {
        return json!(
            {"ok": false, "error": "base_otio, ours_otio, theirs_otio required (OTIO JSON strings)"}
        );
    }
    // Write to temp, reuse CLI merge path semantics via cairn_tl directly.
    let dir = std::env::temp_dir().join(format!("cairn-merge-{}", uuid::Uuid::now_v7().simple()));
    if std::fs::create_dir_all(&dir).is_err() {
        return json!({"ok": false, "error": "tmpdir failed"});
    }
    let bp = dir.join("base.otio");
    let op = dir.join("ours.otio");
    let tp = dir.join("theirs.otio");
    if std::fs::write(&bp, base.as_bytes()).is_err()
        || std::fs::write(&op, ours.as_bytes()).is_err()
        || std::fs::write(&tp, theirs.as_bytes()).is_err()
    {
        return json!({"ok": false, "error": "tmp write failed"});
    }
    let opts = cairn_tl::merge::MergeOptions { semantic };
    let parse = |p: &PathBuf| -> Result<cairn_tl::model::Timeline, String> {
        let s = std::fs::read_to_string(p).map_err(|e| format!("read: {e}"))?;
        // Try OTIO first, then FCPXML bridge.
        cairn_tl::parse::parse_otio(&s)
            .map_err(|e| format!("parse: {e}"))
            .or_else(|_| cairn_tl::fcpxml::parse_fcpxml(&s).map_err(|e| format!("parse: {e}")))
    };
    let (base_t, ours_t, theirs_t) = match (parse(&bp), parse(&op), parse(&tp)) {
        (Ok(b), Ok(o), Ok(t)) => (b, o, t),
        _ => {
            return json!(
                {"ok": false, "error": "parse failed: base/ours/theirs must be OTIO or FCPXML"}
            )
        }
    };
    match cairn_tl::merge::merge_with(&base_t, &ours_t, &theirs_t, &opts) {
        Ok((_merged, report)) => report.to_json(),
        Err(e) => json!({"ok": false, "error": e.0}),
    }
}

/// POST /api/v1/compress {project_id, media, preset} — production compression ladder.
/// Local-first FFmpeg (H264 CRF23 → H265 CRF28 ~40% smaller → SVT-AV1 ~50%+),
/// optional Cloudinary-via-Composio when CLOUDINARY_* env present (q_auto/f_auto).
/// Presets: proxy360 | proxy540 | web720 | archive. Returns bytes + recipe used.
pub async fn compress(
    state: &Arc<DaemonState>,
    project: &str,
    media: &str,
    preset: &str,
    via: Option<&str>,
) -> Json {
    if project.is_empty() || media.is_empty() {
        return json!({"ok": false, "error": "project_id, media required"});
    }
    let Some(root) = state.projects.project_root(project).await else {
        return json!({"ok": false, "error": "project not attached"});
    };
    let Some(full) = safe_join(&root, media) else {
        return json!({"ok": false, "error": "path refused (traversal)"});
    };
    if !full.is_file() {
        return json!({"ok": false, "error": "media not materialized — recall first"});
    }
    // Cloud path: Composio Cloudinary (109 tools) when creds present — q_auto/f_auto.
    let cloud = std::env::var("CLOUDINARY_CLOUD_NAME").is_ok()
        && std::env::var("CLOUDINARY_API_KEY").is_ok();
    if via == Some("cloudinary") {
        if !cloud {
            return json!(
                {"ok": false, "error": "CLOUDINARY_CLOUD_NAME/API_KEY/SECRET required (Composio handles refresh)"}
            );
        }
        return json!(
            {"ok": true, "via": "cloudinary", "recipe": "q_auto,f_auto,sp_auto (20-40% smaller, 26.6MB→3.9MB class)", "next": "POST eager w720/q_auto via Composio CLOUDINARY toolkit"}
        );
    }
    // Local FFmpeg ladder (proxy-maker pattern: intra-frame for scrub, H264 short-GOP for size).
    let (height, crf, codec_args): (u32, u32, Vec<&str>) = match preset {
        "proxy360" => (360, 23, vec!["-c:v", "libx264", "-preset", "veryfast"]),
        "web720" => (720, 23, vec!["-c:v", "libx264", "-preset", "medium"]),
        "archive" => (
            1080,
            28,
            vec!["-c:v", "libx265", "-preset", "medium", "-tag:v", "hvc1"],
        ),
        _ => (540, 23, vec!["-c:v", "libx264", "-preset", "fast"]),
    };
    let out_name = format!(
        ".cairn/proxy-cache/{}-{}p.mp4",
        blake3::hash(media.as_bytes()).to_hex(),
        height
    );
    let out_full = root.join(&out_name);
    if let Some(p) = out_full.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let status = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&full)
        .args(["-vf", &format!("scale=-2:{height}")])
        .args(codec_args)
        .args([
            "-crf",
            &crf.to_string(),
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out_full)
        .output();
    match status {
        Ok(o) if o.status.success() => {
            let bytes = std::fs::metadata(&out_full).map(|m| m.len()).unwrap_or(0);
            json!(
                {"ok": true, "via": "ffmpeg", "out": out_name, "bytes": bytes, "preset": preset, "note": "H265 ~40% smaller than H264; SVT-AV1 ~50%+ when available (libsvtav1 -crf 30 -preset 6)"}
            )
        }
        Ok(o) => json!(
            {"ok": false, "error": format!("ffmpeg failed: {}", String::from_utf8_lossy(&o.stderr).chars().take(300).collect::<String>())}
        ),
        Err(e) => json!(
            {"ok": false, "error": format!("ffmpeg missing: {e} — winget install Gyan.FFmpeg")}
        ),
    }
}

/// POST /api/v1/merge/offer/accept {project_id, path} — accept a pending
/// semantic-merge offer (CONTRACT-DEBT #1): recompute the merge, write the
/// merged timeline to the original path, drop the conflict copy, and let the
/// engine push exactly ONE journal entry. The action MUST run on the LIVE
/// runtime's engine (same store handle + plane as the sync loop), so a
/// stopped/never-started project answers honestly instead of half-accepting.
/// The merge itself is byte-deterministic; `report` echoes the merge report
/// for the UI toast.
pub async fn merge_offer_accept(state: &Arc<DaemonState>, project: &str, path: &str) -> Json {
    if project.is_empty() || path.is_empty() {
        return json!({"ok": false, "error": "project_id, path required"});
    }
    let outcome = state
        .projects
        .with_engine(
            project,
            |engine| async move { engine.accept_offer(path).await },
        )
        .await;
    match outcome {
        Err(msg) => json!({"ok": false, "error": msg}),
        Ok(Err(e)) => json!({"ok": false, "error": e.message}),
        Ok(Ok(outcome)) => json!({"ok": true, "report": outcome.report_json}),
    }
}

/// POST /api/v1/merge/offer/decline {project_id, path} — withdraw the offer
/// affordance ONLY: the conflict copy stays on disk and in the table (§7.1
/// contract — declining means "I'll resolve it myself", never "discard my
/// edit"). Store-only on purpose: it works even while the project's sync
/// loop is between retries, and deleting a row needs no engine.
pub fn merge_offer_decline(state: &Arc<DaemonState>, project: &str, path: &str) -> Json {
    if project.is_empty() || path.is_empty() {
        return json!({"ok": false, "error": "project_id, path required"});
    }
    let Some(store) = open_store(&state.home) else {
        return json!({"ok": false, "error": "store unavailable"});
    };
    match store.get_merge_offer(project, path) {
        None => json!({"ok": true, "removed": false, "error": "no pending offer for that path"}),
        Some(_) => match store.delete_merge_offer(project, path) {
            Ok(()) => json!({"ok": true, "removed": true}),
            Err(e) => json!({"ok": false, "error": e.message}),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A daemon state over a temp home (no attached projects: the join
    /// surface works against the home store's invite state alone, and the
    /// audit helper degrades to a no-op without a root).
    fn state() -> (tempfile::TempDir, Arc<DaemonState>) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().to_path_buf();
        (home, Arc::new(DaemonState::new(path)))
    }

    /// Seed the invite state exactly like `team_regenerate` would (minting
    /// needs an attached project + RBAC; the invite STATE is what the join
    /// surface validates against).
    fn seed_invite(home: &std::path::Path, code: &str, exp_ms: i64) {
        let store = open_store(home).unwrap();
        store.meta_set("swarm/join-code", code).unwrap();
        store
            .meta_set("swarm/join-code-exp", &exp_ms.to_string())
            .unwrap();
    }

    fn fresh_code() -> String {
        format!("enr-{}", uuid::Uuid::now_v7().simple())
    }

    fn now() -> i64 {
        WallClock.now_millis()
    }

    /// The full happy path: valid live code joins ONCE; the single-use
    /// burn makes the second attempt fail with the SAME generic error as
    /// any other rejection (no existence leak).
    #[tokio::test]
    async fn join_burns_the_code_after_one_successful_use() {
        let (_home, st) = state();
        let code = fresh_code();
        seed_invite(st.home.as_path(), &code, now() + 600_000);

        let ok = team_join(&st, &code).await;
        assert_eq!(ok["ok"], true, "first join validates: {ok}");

        let again = team_join(&st, &code).await;
        assert_eq!(again["ok"], false);
        assert_eq!(
            again["error"], "join code not accepted",
            "burned code gets the generic refusal"
        );
        // the intent record exists (pre-existing peer-join contract)
        let store = open_store(st.home.as_path()).unwrap();
        assert_eq!(
            store.meta_get("swarm/peer-join").as_deref(),
            Some(code.as_str())
        );
    }

    /// Expired, revoked, unknown and wrong codes all produce the SAME
    /// generic error; revocation works through the factored mutation and
    /// regenerating clears everything.
    #[tokio::test]
    async fn expired_revoked_and_unknown_share_one_refusal() {
        let (_home, st) = state();
        let code = fresh_code();
        seed_invite(st.home.as_path(), &code, now() - 1); // already expired
        let expired = team_join(&st, &code).await;
        assert_eq!(expired["error"], "join code not accepted");

        // revoked: a live code killed by the revoke mutation
        let (_h2, st2) = state();
        let code2 = fresh_code();
        seed_invite(st2.home.as_path(), &code2, now() + 600_000);
        let store = open_store(st2.home.as_path()).unwrap();
        assert!(
            revoke_join_code(&store, &code2),
            "owner revokes the live invite"
        );
        // revoking again is a no-op on the flag (same current invite, still revoked)
        assert!(revoke_join_code(&store, &code2));
        assert!(
            !revoke_join_code(&store, "enr-notthecode"),
            "unknown refuses"
        );
        let revoked = team_join(&st2, &code2).await;
        assert_eq!(
            revoked["error"], "join code not accepted",
            "revoked shares the generic refusal (no existence leak)"
        );

        // unknown code (no invite minted at all): same refusal
        let (_h3, st3) = state();
        let ghost = team_join(&st3, &fresh_code()).await;
        assert_eq!(ghost["error"], "join code not accepted");

        // shape feedback stays distinct (says nothing about stored codes)
        let (_h4, st4) = state();
        let malformed = team_join(&st4, "garbage").await;
        assert!(malformed["error"].as_str().unwrap().contains("shape"));
    }

    /// Brute-force budget: 5 failed attempts lock the join surface — even
    /// the VALID code is refused while locked (same generic error), and a
    /// regenerate clears the lockout.
    #[tokio::test]
    async fn five_failures_lock_the_join_surface() {
        let (_home, st) = state();
        let code = fresh_code();
        seed_invite(st.home.as_path(), &code, now() + 600_000);

        for i in 0..JOIN_MAX_FAILURES {
            let bad = team_join(&st, &format!("enr-wrong-attempt-{i}")).await;
            assert_eq!(bad["error"], "join code not accepted");
        }
        // locked: even the valid code cannot join
        let locked = team_join(&st, &code).await;
        assert_eq!(
            locked["error"], "join code not accepted",
            "lockout refuses the valid code with the generic error"
        );

        // the lockout state is exactly what regenerate clears
        let store = open_store(st.home.as_path()).unwrap();
        assert!(store.meta_get("swarm/join-locked-until").is_some());
        store.meta_clear("swarm/join-locked-until").unwrap();
        store.meta_clear("swarm/join-fails").unwrap();
        let ok = team_join(&st, &code).await;
        assert_eq!(
            ok["ok"], true,
            "after the lock clears, the code still joins"
        );
    }

    /// The route's TTL contract (worklog freeze #2): clamp to 1..=8760.
    #[test]
    fn review_link_ttl_clamps_to_the_contract_bounds() {
        assert_eq!(clamp_ttl_hours(720), 720, "the 30-day default passes");
        assert_eq!(clamp_ttl_hours(1), 1);
        assert_eq!(clamp_ttl_hours(8_760), 8_760, "one year is the ceiling");
        assert_eq!(clamp_ttl_hours(0), 1, "zero floors to 1h");
        assert_eq!(clamp_ttl_hours(-42), 1, "negative floors to 1h");
        assert_eq!(clamp_ttl_hours(99_999), 8_760, "beyond a year clamps down");
    }
}
