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
///
/// #44/#88 restore safety checkpoint: restore OVERWRITES workspace state,
/// and the moment a user discovers "restore grabbed the wrong commit" is
/// AFTER the restore. So every restore first folds the CURRENT journal head
/// into a commit (the same snapshot-create machinery `create_snapshot`
/// uses) and reports it in the response — the UI can say "Cairn saved the
/// current state first" and one click undoes a wrong restore. A failed
/// checkpoint FAILS the restore: a destructive act without a known-good
/// escape hatch is exactly the accident class #44 describes. Existing
/// response fields (`ok`, `restored_files`, `bytes`) are unchanged.
pub const RESTORE_CHECKPOINT_LABEL: &str = "safety checkpoint before restore";

pub async fn restore_snapshot(
    state: &Arc<DaemonState>,
    project: &str,
    commit_hash: &str,
    target_path: &str,
) -> Result<Json, String> {
    let (checkpoint_version, checkpoint_commit) = restore_safety_checkpoint(state, project).await?;
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
            json!({
                "ok": true,
                "restored_files": r.restored_files,
                "bytes": r.bytes,
                "checkpoint_version": checkpoint_version,
                "checkpoint_commit": checkpoint_commit,
                "checkpoint_label": RESTORE_CHECKPOINT_LABEL,
            })
        })
        .map_err(|s| s.message().to_string())
}

/// Fold the current state into a checkpoint commit and return
/// `(snapshot_seq, commit_hash)`. Reuses the EXACT snapshot-create path
/// (`create_snapshot` → ctl FoldNow); the seq comes from the snapshot list,
/// whose first entry is always the newest commit on `main` — the one this
/// call just folded.
async fn restore_safety_checkpoint(
    state: &Arc<DaemonState>,
    project: &str,
) -> Result<(i64, String), String> {
    let snap = create_snapshot(state, project, RESTORE_CHECKPOINT_LABEL).await?;
    let commit = snap
        .get("commit_hash")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let list = list_snapshots(state, project).await?;
    let version = list
        .get("snapshots")
        .and_then(|s| s.get(0))
        .and_then(|s| s.get("snapshot_seq"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Ok((version, commit))
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
///
/// Review #41: the old loop polled `exists()` then copied — a TOCTOU race
/// (two concurrent duplicates, or a watcher-created "(copy)" between the
/// check and the copy) could OVERWRITE an existing file, and at 100+ copies
/// it silently clobbered `name (copy 100).ext` unconditionally. The
/// destination is now claimed with `create_new(true)` (exclusive create —
/// the kernel arbitrates), looping past 100 without a rename cap (bounded
/// at 10 000 for sanity: every iteration is one O(1) create attempt, and a
/// project with 10 000 same-named copies has bigger problems).
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
    let candidate_name = |n: u32| -> std::path::PathBuf {
        let base = if n == 1 {
            format!("{} (copy)", stem.display())
        } else {
            format!("{} (copy {n})", stem.display())
        };
        if ext.is_empty() {
            std::path::PathBuf::from(base)
        } else {
            std::path::PathBuf::from(format!("{base}.{ext}"))
        }
    };
    // Claim the destination FIRST (exclusive create), then stream the bytes
    // into the handle we already own — the file that appears at `candidate`
    // is ours by construction, never a clobber.
    const DUPLICATE_NAME_CAP: u32 = 10_000;
    let mut n = 0u32;
    let (candidate, mut dest) = loop {
        n += 1;
        if n > DUPLICATE_NAME_CAP {
            return json!({"ok": false, "error": format!(
                "no free duplicate name after {DUPLICATE_NAME_CAP} attempts"
            )});
        }
        let candidate = candidate_name(n);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(f) => break (candidate, f),
            // taken: try the next name
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return json!({"ok": false, "error": format!("copy failed: {e}")});
            }
        }
    };
    let mut src = match tokio::fs::File::open(&full).await {
        Ok(f) => f,
        Err(e) => {
            let _ = tokio::fs::remove_file(&candidate).await; // don't leave an empty claim
            return json!({"ok": false, "error": format!("copy failed: {e}")});
        }
    };
    use tokio::io::AsyncWriteExt as _;
    let written = tokio::io::copy(&mut src, &mut dest).await;
    let flush = dest.flush().await;
    match (written, flush) {
        (Ok(bytes), Ok(())) => {
            let rel = candidate
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| candidate.to_string_lossy().into_owned());
            json!({"ok": true, "path": rel, "bytes": bytes})
        }
        (Err(e), _) | (_, Err(e)) => {
            drop(dest);
            let _ = tokio::fs::remove_file(&candidate).await; // partial copy removed, original untouched
            json!({"ok": false, "error": format!("copy failed: {e}")})
        }
    }
}

/// POST /api/v1/team/regenerate {project_id?} — mint a fresh single-use join
/// code (600s TTL). Production: uses the server auth when reachable, else a
/// local `enr-` code stored in meta for the WS rendezvous.
/// POLICY BOUNDARY (ADR-0030): invite generation is a ManageMembers act. The
/// ctl path always enforced it; the dashboard path does too now — the
/// "service action = direct mutation" drift the review flagged is closed
/// (every dashboard mutation funnels through rbac_guard like ctl does).
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
        _ => {
            // review #46: the probe shells out to ffprobe (a blocking
            // subprocess over possibly gigabyte media) — publish runs on the
            // async runtime, so the probe goes to the blocking pool.
            // Behavior identical: join failure degrades to the same honest
            // "could not probe" answer.
            let probe_path = full.clone();
            match tokio::task::spawn_blocking(move || crate::review::probe_media(&probe_path)).await
            {
                Ok(Some(probed)) => (probed.2, probed.0, probed.1),
                _ => {
                    return json!({"ok": false, "error": "could not probe this media (ffprobe missing or file unreadable) - install ffmpeg, or publish with explicit frames/fps"})
                }
            }
        }
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
pub async fn tl_merge(base: &str, ours: &str, theirs: &str, semantic: bool) -> Json {
    if base.is_empty() || ours.is_empty() || theirs.is_empty() {
        return json!(
            {"ok": false, "error": "base_otio, ours_otio, theirs_otio required (OTIO JSON strings)"}
        );
    }
    // review #47: the tmp writes + three parses + the C0-C10 merge are pure
    // blocking CPU over potentially large timeline JSON — off the async
    // runtime. Response contract unchanged.
    let base = base.to_string();
    let ours = ours.to_string();
    let theirs = theirs.to_string();
    let joined =
        tokio::task::spawn_blocking(move || tl_merge_blocking(&base, &ours, &theirs, semantic))
            .await;
    match joined {
        Ok(j) => j,
        Err(e) => json!({"ok": false, "error": format!("merge task failed: {e}")}),
    }
}

/// The synchronous merge body (runs on the blocking pool; see tl_merge).
fn tl_merge_blocking(base: &str, ours: &str, theirs: &str, semantic: bool) -> Json {
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
    // review #45: the encode is a LONG blocking subprocess (an archive H265
    // run can outlive the request by minutes) — it must never occupy an
    // async-runtime worker or the whole console stalls mid-encode. Response
    // contract unchanged.
    let encode_full = out_full.clone();
    let joined = tokio::task::spawn_blocking(move || {
        std::process::Command::new("ffmpeg")
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
            .arg(&encode_full)
            .output()
    })
    .await;
    let status = joined.unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
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
    use cairn_core::clock::WallClock;
    use cairn_store::state::LocalState;
    use cairn_store::{Cas, FileRow, HeaderCache, Outbox, Store};
    use std::sync::Arc;

    fn tmp() -> std::path::PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    /// DaemonState with inert defaults (all fields pub; `new` is
    /// daemon-private — same construction the views tests use).
    fn daemon_state(home: std::path::PathBuf) -> DaemonState {
        DaemonState {
            home,
            started: std::time::Instant::now(),
            flags: tokio::sync::RwLock::new(Vec::new()),
            recall_jobs: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            projects: std::sync::Arc::new(crate::projects::ProjectManager::new()),
            doctor_cache: tokio::sync::RwLock::new(None),
        }
    }

    fn enrolled_state(home: &std::path::Path, server_url: &str, device: &str) -> Arc<DaemonState> {
        let store = Store::open(home, Arc::new(WallClock)).unwrap();
        crate::projects::save_identity(
            &store,
            &crate::projects::Identity {
                server_url: server_url.to_string(),
                token: "test-token".into(),
                device_id: device.into(),
                tenant_id: "t-test".into(),
                tls_ca: None,
            },
        )
        .unwrap();
        Arc::new(daemon_state(home.to_path_buf()))
    }

    // ------------------------------------------------------------------
    // #41: file_duplicate never clobbers — exclusive create, past 100
    // ------------------------------------------------------------------

    /// The review #41 cap case: with 150 "(copy N)" siblings already on
    /// disk, duplicate still lands a FRESH name (the old loop capped at 100
    /// and then overwrote "name (copy 100).ext" unconditionally). The
    /// content of every pre-existing copy must survive untouched.
    #[tokio::test]
    async fn file_duplicate_survives_150_existing_copies_without_clobber() {
        let home = tmp();
        let state = enrolled_state(&home, "http://127.0.0.1:1", "dev-dup");
        let root = tmp();
        let src = root.join("clip.braw");
        std::fs::write(&src, b"ORIGINAL-BYTES").unwrap();
        // every candidate name 1..=150 is taken ("(copy)" + "(copy 2..150)"):
        // the first free name is (copy 151) — beyond the old 100 cap that
        // clobbered unconditionally
        std::fs::write(root.join("clip (copy).braw"), b"KEEP-1").unwrap();
        for n in 2..=150 {
            std::fs::write(
                root.join(format!("clip (copy {n}).braw")),
                format!("KEEP-{n}"),
            )
            .unwrap();
        }
        state
            .projects
            .attach(&state.home, &root, Some("p-dup".into()), None)
            .await
            .unwrap();

        let r = file_duplicate(&state, "p-dup", "clip.braw").await;
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["path"], "clip (copy 151).braw");
        assert_eq!(r["bytes"], b"ORIGINAL-BYTES".len() as u64);
        // the copy is byte-exact
        assert_eq!(
            std::fs::read(root.join("clip (copy 151).braw")).unwrap(),
            b"ORIGINAL-BYTES"
        );
        // and NOT ONE pre-existing copy was touched (the old code clobbered #100)
        assert_eq!(
            std::fs::read(root.join("clip (copy).braw")).unwrap(),
            b"KEEP-1"
        );
        for n in 2..=150 {
            assert_eq!(
                std::fs::read(root.join(format!("clip (copy {n}).braw"))).unwrap(),
                format!("KEEP-{n}").into_bytes(),
                "copy {n} must survive untouched"
            );
        }
        // original intact too
        assert_eq!(std::fs::read(&src).unwrap(), b"ORIGINAL-BYTES");
    }

    /// Baseline collision names stay the familiar " (copy)" / " (copy 2)".
    #[tokio::test]
    async fn file_duplicate_names_and_race_safety_basics() {
        let home = tmp();
        let state = enrolled_state(&home, "http://127.0.0.1:1", "dev-dup");
        let root = tmp();
        std::fs::write(root.join("clip.braw"), b"AA").unwrap();
        state
            .projects
            .attach(&state.home, &root, Some("p-dup2".into()), None)
            .await
            .unwrap();
        let r1 = file_duplicate(&state, "p-dup2", "clip.braw").await;
        assert_eq!(r1["path"], "clip (copy).braw");
        let r2 = file_duplicate(&state, "p-dup2", "clip.braw").await;
        assert_eq!(r2["path"], "clip (copy 2).braw");
        // extensionless files keep the no-suffix shape
        std::fs::write(root.join("README"), b"RR").unwrap();
        let r3 = file_duplicate(&state, "p-dup2", "README").await;
        assert_eq!(r3["path"], "README (copy)");
    }

    // ------------------------------------------------------------------
    // #44/#88: restore creates a safety checkpoint first
    // ------------------------------------------------------------------

    /// Boot the REAL server stack (gRPC + objects HTTP, ephemeral loopback
    /// ports) — the same shape crates/cairn-server/tests/cold_fetch.rs uses.
    /// The accept loop feeds `serve_with_incoming` via a channel so the
    /// test needs no extra tokio-stream features.
    async fn spin_server(dir: &std::path::Path) -> (Arc<cairn_server::ServerState>, String) {
        let obj_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let obj_port = obj_listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{obj_port}/");
        let db = cairn_server::db::open(&dir.join("meta.db")).await.unwrap();
        cairn_server::db::migrate(&db).await.unwrap();
        let auth = cairn_server::auth::Authenticator::load_or_create(
            &dir.join("keys"),
            Arc::new(WallClock),
        )
        .unwrap();
        let store = Arc::new(
            cairn_server::storage::LocalFsStore::open(
                &dir.join("objects"),
                b"test-object-key",
                &base,
            )
            .unwrap(),
        );
        let state = Arc::new(cairn_server::ServerState {
            db,
            auth,
            store: Arc::clone(&store) as Arc<dyn cairn_server::storage::ObjectStore>,
            bloom: tokio::sync::RwLock::new(cairn_core::bloom::Bloom::empty()),
            clock: Arc::new(WallClock) as Arc<dyn cairn_core::clock::SystemClock>,
            dev_insecure: true,
        });
        state.migrate().await.unwrap();

        let router = store.router();
        tokio::spawn(async move { axum::serve(obj_listener, router).await });

        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_port = grpc_listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::channel::<tokio::net::TcpStream>(64);
        tokio::spawn(async move {
            while let Ok((sock, _)) = grpc_listener.accept().await {
                if tx.send(sock).await.is_err() {
                    break;
                }
            }
        });
        let serve_state = Arc::clone(&state);
        tokio::spawn(async move {
            use futures::StreamExt;
            let incoming =
                tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::io::Error>);
            let _ = tonic::transport::Server::builder()
                .add_service(cairn_proto::pb::journal_server::JournalServer::new(
                    cairn_server::services::JournalSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::lease_server::LeaseServer::new(
                    cairn_server::services::LeaseSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::upload_server::UploadServer::new(
                    cairn_server::services::UploadSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::download_server::DownloadServer::new(
                    cairn_server::services::DownloadSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::auth_server::AuthServer::new(
                    cairn_server::services::AuthSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::project_server::ProjectServer::new(
                    cairn_server::services::ProjectSvc {
                        state: serve_state.clone(),
                    },
                ))
                .add_service(cairn_proto::pb::snapshot_server::SnapshotServer::new(
                    cairn_server::services::SnapshotSvc { state: serve_state },
                ))
                .serve_with_incoming(incoming)
                .await;
        });
        (state, format!("http://127.0.0.1:{grpc_port}"))
    }

    /// The full restore story against the real server: a real file is
    /// pushed by a real engine, folded into commit1; restore(commit1) must
    /// FIRST fold a checkpoint commit (reported as checkpoint_version /
    /// checkpoint_commit) and then materialize the file. The response keeps
    /// its historical fields (ok / restored_files / bytes) intact.
    #[tokio::test]
    async fn restore_creates_a_safety_checkpoint_of_the_current_state_first() {
        let server_dir = tempfile::tempdir().unwrap();
        let (server, grpc_url) = spin_server(server_dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t-test',0)")
            .execute(&server.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT OR IGNORE INTO projects(tenant_id, project_id, created_at) VALUES('t-test','p-restore',0)",
        )
        .execute(&server.db)
        .await
        .unwrap();
        let code = server
            .auth
            .enroll_code("t-test", "restore@test.tv", "sync", 600_000)
            .await;
        let (token, identity) = server
            .auth
            .enroll(&server.db, &code, "pk-restore", "restore-probe")
            .await
            .unwrap();
        assert_eq!(identity.tenant_id, "t-test");

        // daemon home with the REAL identity (server reachable)
        let home = tmp();
        {
            let store = Store::open(&home, Arc::new(WallClock)).unwrap();
            crate::projects::save_identity(
                &store,
                &crate::projects::Identity {
                    server_url: grpc_url.clone(),
                    token,
                    device_id: identity.device_id.clone(),
                    tenant_id: identity.tenant_id.clone(),
                    tls_ca: None,
                },
            )
            .unwrap();
        }
        let state = Arc::new(daemon_state(home.clone()));

        // project root + workspace binding + one real file pushed by a real engine
        let root = tmp();
        std::fs::create_dir_all(root.join("media")).unwrap();
        std::fs::write(root.join("media/notes.txt"), b"restore-me").unwrap();
        // restore is Owner-only in the RBAC matrix — the enrolled device must
        // BE the owner of this root for the ctl path to accept the restore
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        let mut members = cairn_core::rbac::MemberFile::default();
        members.upsert(
            &identity.device_id,
            "restore probe",
            cairn_core::rbac::Role::Owner,
            "self",
            1,
        );
        std::fs::write(
            crate::members::members_path(&root),
            members.to_json().unwrap(),
        )
        .unwrap();
        let store = Store::open(&home, Arc::new(WallClock)).unwrap();
        cairn_sync::workspace::set_workspace_ns(&store, "p-restore", &root).unwrap();
        let conn = store.conn_handle();
        let cas = Cas::open(&home.join("blobs"), conn.clone()).unwrap();
        let plane = cairn_sync::plane_grpc::GrpcPlane::connect(
            &grpc_url,
            &store.meta_get("auth/token").unwrap(),
            "t-test",
            None,
        )
        .await
        .unwrap();
        let engine = cairn_sync::Engine {
            tenant_id: "t-test".into(),
            project_id: "p-restore".into(),
            device_id: identity.device_id.clone(),
            local_ns: "p-restore".into(),
            author_id: identity.device_id.clone(),
            store: store.clone(),
            cas,
            outbox: Outbox::new(conn.clone()),
            headers: HeaderCache::new(conn),
            plane: Arc::new(plane),
            dicts: cairn_core::compress::DictRegistry::new(),
            gate: cairn_sync::Gate::new(),
        };
        let meta = std::fs::metadata(root.join("media/notes.txt")).unwrap();
        engine
            .store
            .put_file(&FileRow {
                path: "media/notes.txt".into(),
                project_id: "p-restore".into(),
                manifest_hash: None,
                size: meta.len(),
                mode: "file".into(),
                mtime: cairn_sync::scan::mtime_millis(&meta),
                local_state: LocalState::Dirty.as_str().into(),
            })
            .unwrap();
        engine.sync_pass().await.expect("push to real server");
        // fold the pushed state into commit1 (the restore target). NOTE: the
        // ctl restore path is Owner-only; the members file above (written
        // BEFORE attach) makes the enrolled device the root owner.
        let target = create_snapshot(&state, "p-restore", "restore target")
            .await
            .unwrap();
        let commit1 = target["commit_hash"].as_str().unwrap().to_string();
        // workspace drifts AFTER the commit — the restore must bring it back
        std::fs::write(root.join("media/notes.txt"), b"DRIFTED-BEYOND-RECOGNITION").unwrap();

        // attach AFTER the engine pass: the service action resolves the
        // project through the ProjectManager (real attach, real server)
        state
            .projects
            .attach(&state.home, &root, Some("p-restore".into()), None)
            .await
            .unwrap();

        let r = restore_snapshot(&state, "p-restore", &commit1, "")
            .await
            .unwrap();
        assert_eq!(r["ok"], true, "{r}");
        // historical contract fields unchanged
        assert_eq!(r["restored_files"], 1);
        assert_eq!(r["bytes"], b"restore-me".len() as u64);
        // #44/#88: the checkpoint of the CURRENT state, reported for the UI
        assert!(r["checkpoint_version"].as_i64().unwrap() >= 1);
        let checkpoint = r["checkpoint_commit"].as_str().unwrap();
        assert!(
            checkpoint.len() == 64,
            "commit hash is 64 hex: {checkpoint}"
        );
        assert_ne!(
            checkpoint, commit1,
            "checkpoint is a NEW commit, not the restore target"
        );
        assert_eq!(r["checkpoint_label"], RESTORE_CHECKPOINT_LABEL);
        // and the checkpoint is a REAL commit in the snapshot list
        let snaps = list_snapshots(&state, "p-restore").await.unwrap();
        let hashes: Vec<&str> = snaps["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["commit_hash"].as_str().unwrap())
            .collect();
        assert!(hashes.contains(&checkpoint));
        // the workspace is back at the committed bytes
        assert_eq!(
            std::fs::read(root.join("media/notes.txt")).unwrap(),
            b"restore-me"
        );
    }

    /// A failed checkpoint must FAIL the restore (never destroy state
    /// without the escape hatch): no server behind the identity → the
    /// fold_now errors → restore_snapshot returns Err before touching disk.
    #[tokio::test]
    async fn restore_is_refused_when_the_checkpoint_cannot_be_created() {
        let home = tmp();
        let state = enrolled_state(&home, "http://127.0.0.1:1", "dev-offline");
        let r = restore_snapshot(&state, "p-any", "deadbeef", "").await;
        assert!(
            r.is_err(),
            "no checkpoint possible offline → restore refused"
        );
        let msg = r.err().unwrap();
        assert!(
            !msg.is_empty(),
            "the error names the checkpoint failure, not a silent restore"
        );
    }
}
