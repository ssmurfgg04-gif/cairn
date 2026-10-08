//! Proxy-preview actions (ADR-0030 service layer): "big camera files need a
//! fast line — make small preview copies for editing" as a first-class
//! console operation. The pipeline itself is cairn-proxy's (digest →
//! skip-if-current → transcode → index); this module adds the SERVICE
//! policy: attached-project validation, traversal refusal, the media
//! extension gate, RBAC, and a one-transcode-per-media in-flight guard so a
//! double-click can never run two ffmpegs at once.
//!
//! Wire contract (frozen, worklog §API contract freeze 1):
//! * POST /api/v1/proxy/generate {"project","path"} →
//!   200 {"ok":true,"proxy_rel":..,"bytes":N,"state":"ready"}
//!   | 409 {"ok":false,"error":"in_progress"}
//!   | 400 {"ok":false,"error":"..."}
//! * GET /api/v1/proxy/status?project= →
//!   {"proxies":[{"media_rel","proxy_rel","state":"ready|stale|failed",
//!                "bytes","generated_at_ms"}]}

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::json;

use crate::daemon::DaemonState;

type Json = serde_json::Value;

/// One transcode per (project, media path), process-wide. The dashboard is
/// the only writer today, but the guard lives here (not in the adapter) so
/// the tray/panel/CLI surfaces that call the service get the same
/// idempotency. Mutex-guarded HashMap keyed (project, path) → started_at;
/// entries are removed on completion AND on failure, so a crashed encode
/// never wedges the media. Same guard shape as the daemon's recall_jobs
/// (a mutex'd map of in-flight jobs). #42-adjacent: idempotency where it is
/// cheap — here it costs one map entry per running ffmpeg.
static IN_FLIGHT: Lazy<std::sync::Mutex<HashMap<(String, String), std::time::Instant>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// Test/inspection hook: is a generation for this (project, path) running?
#[cfg(test)]
pub(crate) fn is_in_flight(project: &str, path: &str) -> bool {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&(project.to_string(), path.to_string()))
}

/// POST /api/v1/proxy/generate {project, path} — generate (or reuse) the
/// editing proxy for one media file. Returns Ok(json) for 200, Err(json)
/// for 400/409 (the adapter maps "in_progress" → 409, everything else →
/// 400; the service never speaks HTTP status codes — ADR-0030).
pub async fn proxy_generate(state: &Arc<DaemonState>, project: &str, path: &str) -> Json {
    if project.is_empty() || path.is_empty() {
        return json!({"ok": false, "error": "project, path required"});
    }
    let Some(root) = state.projects.project_root(project).await else {
        return json!({"ok": false, "error": "project not attached"});
    };
    let Some(full) = super::safe_join(&root, path) else {
        return json!({"ok": false, "error": "path refused (traversal)"});
    };
    // The SPEC §6 media sniff table — the SAME allowlist the sync pipeline's
    // compression policy uses to classify media (cairn-core `is_media_path`;
    // the review publish/probe path's ffprobe stays the final decodability
    // arbiter). Refusing `.txt`/`.otio`/… here costs one extension compare
    // instead of an ffprobe spawn.
    if !cairn_core::compress::is_media_path(path) {
        return json!({"ok": false, "error": format!(
            "'{}' is not a media file — proxies are for footage/audio only",
            path.rsplit('.').next().unwrap_or(path)
        )});
    }
    if !tokio::fs::metadata(&full)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return json!({"ok": false, "error": "media not materialized on this machine — recall it first"});
    }
    // POLICY BOUNDARY (ADR-0030): generating a proxy WRITES a derived media
    // file into the project (`.cairn/proxy-cache/<digest>.mp4`). In the RBAC
    // matrix that is the WriteFiles class ("write/ingest media + project
    // files") — the same class ingest and `compress` (the other ffmpeg
    // writer) fall under; it is neither a review act (ManageReview governs
    // the review stack/links) nor a move/rename (OrganizeBins).
    if let Err(s) = crate::daemon::rbac_guard(
        state,
        project,
        Some(root.as_path()),
        cairn_core::rbac::Permission::WriteFiles,
        "dash/proxy-generate",
    )
    .await
    {
        return json!({"ok": false, "error": s.message()});
    }
    // In-flight guard: claim (project, path) or answer 409. The claim is
    // released on EVERY path below (success, transcode failure, join
    // failure) — a wedged entry would lock the media out of proxies forever.
    let key = (project.to_string(), path.to_string());
    {
        let mut guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(&key) {
            return json!({"ok": false, "error": "in_progress"});
        }
        guard.insert(key.clone(), std::time::Instant::now());
    }
    let result = run_generation(root, path.to_string()).await;
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    match result {
        Ok(entry) => json!({
            "ok": true,
            "proxy_rel": entry.proxy_rel,
            "bytes": entry.bytes,
            "state": "ready",
        }),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// The blocking work: full-source blake3 digest + index IO + (on a miss)
/// the ffmpeg subprocess. Spawned onto the blocking pool — a 50 GB camera
/// master digests for seconds and an archive encode runs for MINUTES; the
/// async runtime must stay free (same discipline as compress/review #45).
async fn run_generation(
    root: std::path::PathBuf,
    media_rel: String,
) -> Result<cairn_proxy::model::ProxyEntry, String> {
    let profile = cairn_proxy::model::ProxyProfile::default();
    let transcoder = cairn_proxy::transcode::FfmpegTranscoder;
    let now = super::now_ms_i64();
    let joined = tokio::task::spawn_blocking(move || {
        cairn_proxy::pipeline::generate(&root, &media_rel, &profile, &transcoder, now)
    })
    .await;
    match joined {
        Ok(Ok(entry)) => Ok(entry),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("proxy generation task failed: {e}")),
    }
}

/// GET /api/v1/proxy/status?project= — every indexed proxy with its state.
/// Read path mirrors the CLI's `cairn proxy list`: index file →
/// ProxyIndex::from_json, stat-fast state per entry (verify=false — NO
/// rehash of camera masters just to draw a table). An empty/missing index
/// is an honest empty list, not an error.
pub async fn proxy_status(state: &Arc<DaemonState>, project: &str) -> Json {
    let root = if project.is_empty() {
        state.projects.first().await.map(|(_, r, _, _)| r)
    } else {
        state.projects.project_root(project).await
    };
    let Some(root) = root else {
        return json!({"proxies": []});
    };
    let idx = std::fs::read(cairn_proxy::pipeline::index_path(&root))
        .ok()
        .and_then(|b| cairn_proxy::model::ProxyIndex::from_json(&b).ok())
        .unwrap_or_default();
    let mut rows: Vec<Json> = idx
        .proxies
        .values()
        .map(|e| {
            // state semantics per cairn-proxy status_of/status_of_fast: a
            // recorded failure is Failed; otherwise the LATEST entry for the
            // media decides — matching digest = ready, superseded/unknown =
            // stale (the safe answer: "the proxy may not match the media").
            let state_str = if e.last_error.is_some() {
                "failed"
            } else {
                match cairn_proxy::pipeline::status_of_fast(&root, &e.media_rel, false) {
                    Ok(Some((latest, st))) if latest.source_digest == e.source_digest => match st {
                        cairn_proxy::model::ProxyStatus::Ready => "ready",
                        cairn_proxy::model::ProxyStatus::Stale => "stale",
                        cairn_proxy::model::ProxyStatus::Failed => "failed",
                    },
                    // superseded by a newer generation, or media vanished:
                    // never claim ready without proof
                    _ => "stale",
                }
            };
            json!({
                "media_rel": e.media_rel,
                "proxy_rel": e.proxy_rel,
                "state": state_str,
                "bytes": e.bytes,
                "generated_at_ms": e.generated_at_ms,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let media = a["media_rel"].as_str().unwrap_or("").cmp(b["media_rel"].as_str().unwrap_or(""));
        let ts = a["generated_at_ms"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&b["generated_at_ms"].as_i64().unwrap_or(0));
        media.then(ts)
    });
    json!({"proxies": rows})
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use cairn_store::Store;

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

    /// A daemon state whose home store carries an identity (attach needs
    /// one) pointed at an unreachable dev server — attach is best-effort
    /// server-side, the local binding is what these tests exercise.
    fn enrolled_state(home: &std::path::Path) -> Arc<DaemonState> {
        let store = Store::open(home, Arc::new(WallClock)).unwrap();
        crate::projects::save_identity(
            &store,
            &crate::projects::Identity {
                server_url: "http://127.0.0.1:1".into(),
                token: "test-token".into(),
                device_id: "dev-test".into(),
                tenant_id: "t-test".into(),
                tls_ca: None,
            },
        )
        .unwrap();
        Arc::new(daemon_state(home.to_path_buf()))
    }

    async fn attached(state: &Arc<DaemonState>, root: &std::path::Path, pid: &str) {
        state
            .projects
            .attach(&state.home, root, Some(pid.to_string()), None)
            .await
            .unwrap();
    }

    fn ffmpeg_present() -> bool {
        use cairn_proxy::transcode::Transcoder as _;
        cairn_proxy::transcode::FfmpegTranscoder.available()
    }

    #[tokio::test]
    async fn proxy_generate_refuses_bad_requests_before_any_work() {
        let home = tmp();
        let state = enrolled_state(&home);
        // not attached
        let r = proxy_generate(&state, "nope", "cuts/v1.mov").await;
        assert_eq!(r["error"], "project not attached");
        // missing args
        let r = proxy_generate(&state, "", "cuts/v1.mov").await;
        assert_eq!(r["error"], "project, path required");
        // traversal refused
        let root = tmp();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v1.mov"), b"media").unwrap();
        attached(&state, &root, "p-bad").await;
        let r = proxy_generate(&state, "p-bad", "../escape.mov").await;
        assert_eq!(r["error"], "path refused (traversal)");
        // non-media extension refused BEFORE the spawn gate
        std::fs::write(root.join("notes.txt"), b"hello").unwrap();
        let r = proxy_generate(&state, "p-bad", "notes.txt").await;
        assert!(r["error"].as_str().unwrap().contains("not a media file"));
        // media but not materialized on disk
        let r = proxy_generate(&state, "p-bad", "cuts/ghost.mov").await;
        assert!(r["error"].as_str().unwrap().contains("not materialized"));
    }

    #[tokio::test]
    async fn proxy_generate_produces_indexed_proxy_and_status_agrees() {
        if !ffmpeg_present() {
            eprintln!("skipping: no ffmpeg in this environment");
            return;
        }
        let home = tmp();
        let state = enrolled_state(&home);
        let root = tmp();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        // a REAL 1-second clip so the ffmpeg leg actually transcodes
        let src = root.join("cuts/v1.mov");
        assert!(std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi",
                "-i", "testsrc=size=320x180:rate=24", "-t", "1",
                "-c:v", "libx264", "-pix_fmt", "yuv420p",
            ])
            .arg(&src)
            .output()
            .unwrap()
            .status
            .success());

        attached(&state, &root, "p-proxy-e2e").await;
        // files() reads the SYNC store, not the filesystem — give the media
        // a row (as a real sync pass would) so the view can badge it
        {
            let store = Store::open(&home, Arc::new(WallClock)).unwrap();
            let meta = std::fs::metadata(&src).unwrap();
            store
                .put_file(&cairn_store::FileRow {
                    path: "cuts/v1.mov".into(),
                    project_id: "p-proxy-e2e".into(),
                    manifest_hash: Some("aa11".into()),
                    size: meta.len(),
                    mode: "file".into(),
                    mtime: cairn_sync::scan::mtime_millis(&meta),
                    local_state: "synced".into(),
                })
                .unwrap();
        }
        let r = proxy_generate(&state, "p-proxy-e2e", "cuts/v1.mov").await;
        assert_eq!(r["ok"], true, "generate failed: {r}");
        assert_eq!(r["state"], "ready");
        let proxy_rel = r["proxy_rel"].as_str().unwrap();
        assert!(proxy_rel.starts_with(".cairn/proxy-cache/"));
        assert!(root.join(proxy_rel).is_file(), "proxy file written");
        assert!(r["bytes"].as_u64().unwrap() > 0);

        // status lists it as ready
        let st = proxy_status(&state, "p-proxy-e2e").await;
        let rows = st["proxies"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["media_rel"], "cuts/v1.mov");
        assert_eq!(rows[0]["state"], "ready");
        assert_eq!(rows[0]["bytes"], r["bytes"]);
        assert!(rows[0]["generated_at_ms"].as_i64().unwrap() > 0);

        // the files view sees it too (proxy_state merged into the row)
        let files = crate::service::views::files(&state, "p-proxy-e2e", "").await;
        let row = files["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["path"] == "cuts/v1.mov")
            .expect("media row present");
        assert_eq!(row["proxy_state"], "ready");
    }

    #[tokio::test]
    async fn in_flight_guard_answers_409_and_releases() {
        let key = ("p-guard".to_string(), "cuts/v1.mov".to_string());
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), std::time::Instant::now());
        assert!(is_in_flight("p-guard", "cuts/v1.mov"));
        let home = tmp();
        let state = enrolled_state(&home);
        let root = tmp();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v1.mov"), b"media-bytes").unwrap();
        attached(&state, &root, "p-guard").await;
        // the guard fires BEFORE any transcode work (no ffmpeg needed here)
        let r = proxy_generate(&state, "p-guard", "cuts/v1.mov").await;
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "in_progress");
        // release: the entry is gone, a later call would proceed
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        assert!(!is_in_flight("p-guard", "cuts/v1.mov"));
    }

    #[tokio::test]
    async fn proxy_status_is_honest_when_empty_or_detached() {
        let home = tmp();
        let state = enrolled_state(&home);
        let st = proxy_status(&state, "never-attached").await;
        assert_eq!(st["proxies"].as_array().unwrap().len(), 0);
        // attached but no proxies generated yet
        let root = tmp();
        attached(&state, &root, "p-empty").await;
        let st = proxy_status(&state, "p-empty").await;
        assert_eq!(st["proxies"].as_array().unwrap().len(), 0);
        // a corrupt index answers as empty (fail-open read, like the CLI list)
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        std::fs::write(
            root.join(".cairn/proxies.json"),
            b"{ corrupt",
        )
        .unwrap();
        let st = proxy_status(&state, "p-empty").await;
        assert_eq!(st["proxies"].as_array().unwrap().len(), 0);
    }
}
