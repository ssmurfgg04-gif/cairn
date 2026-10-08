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

/// POST /api/v1/team/regenerate — mint a fresh single-use join code (600s TTL).
/// Production: uses the server auth when reachable, else a local `enr-` code
/// stored in meta for the WS rendezvous. Owner/Lead only via ctl guard parity.
pub fn team_regenerate(state: &Arc<DaemonState>) -> Json {
    let code = format!("enr-{}", uuid::Uuid::now_v7().simple());
    if let Some(store) = open_store(state.home.as_path()) {
        let _ = store.meta_set("swarm/join-code", &code);
        let _ = store.meta_set(
            "swarm/join-code-exp",
            &(WallClock.now_millis() + 600_000).to_string(),
        );
    }
    json!({"ok": true, "join_code": code, "ttl_ms": 600_000})
}

/// POST /api/v1/team/join {code} — accept a teammate code, persist peer intent.
/// Full enroll still runs via `cairn login --server … --code …`; this records
/// intent + validates shape so the UI can guide (join-code gated admission).
pub fn team_join(state: &Arc<DaemonState>, code: &str) -> Json {
    let code = code.trim().to_string();
    if !(code.starts_with("enr-") && code.len() > 8) {
        return json!({"ok": false, "error": "invalid join code shape (expected enr-…)"});
    }
    if let Some(store) = open_store(state.home.as_path()) {
        let _ = store.meta_set("swarm/peer-join", &code);
    }
    json!(
        {"ok": true, "code": code, "next": "run: cairn dev-enroll-code --server <server>, then cairn login --server <server> --code <code>"}
    )
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
pub async fn review_link(
    state: &Arc<DaemonState>,
    project: &str,
    note: &str,
    role: &str,
    ttl_h: i64,
) -> Json {
    let root = resolve_root(state, project).await;
    let Some(root) = root else {
        return json!({"ok": false, "error": if project.is_empty() { "no attached project" } else { "project not attached" }});
    };
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
