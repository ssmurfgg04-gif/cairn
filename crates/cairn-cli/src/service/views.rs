//! Read-only console views (ADR-0030 service layer): every GET surface of
//! the loopback console, expressed as plain `serde_json::Value` functions
//! over the daemon state. Same shapes the UI already consumes — the HTTP
//! adapter adds nothing but status codes.

use serde_json::json;

use cairn_core::clock::{SystemClock, WallClock};

use crate::daemon::DaemonState;

use super::{display_name_of, file_badge, now_ms_i64, open_store};

/// GET /api/v1/status — daemon health, honest summary numbers, per-project
/// swarm/NAT metrics. Absence stays absent (no invented zeros).
pub async fn status(state: &DaemonState) -> serde_json::Value {
    let home = state.home.as_path();
    let mut last_error: Option<String> = None;
    let summary_projects;
    let mut swarm_summary = Vec::new();
    {
        let runtimes = state.projects.list().await;
        summary_projects = runtimes.len() as u64;
        for rt in &runtimes {
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
                    "project": rt.project_id,
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
    json!({
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
    })
}

/// GET /api/v1/feed — the activity timeline (leases as "project opened"
/// events, recent file rows, pins), newest first, capped.
pub fn feed(state: &DaemonState) -> serde_json::Value {
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
    json!({ "activity": activity, "leases": leases })
}

/// GET /api/v1/activity?tz_offset=&days= — the dashboard chart's data:
/// per-day byte totals for files touched in the window, day boundaries in
/// the CALLER's timezone (JS `getTimezoneOffset` convention) so the chart's
/// weekday labels match the user's clock. Reads the real store — no
/// invented series, no stub shape: an empty project renders an honest
/// empty chart, never a fake curve (round 25).
pub fn activity(state: &DaemonState, days: u32, tz_offset: i64) -> serde_json::Value {
    let now = WallClock.now_millis();
    let cutoff = now.saturating_sub(i64::from(days).saturating_mul(86_400_000));
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
    json!({
        "ok": true,
        "days": days_out,
        "window_days": days,
        "generated_ms": now,
    })
}

/// GET /api/v1/projects — the attached projects with display names, states
/// and per-project progress.
pub async fn projects(state: &DaemonState) -> serde_json::Value {
    let mut out = Vec::new();
    for rt in state.projects.list().await {
        let v = rt.view.read().await;
        let pid = rt.project_id.clone();
        out.push(json!({
            "project_id": pid,
            "display_name": display_name_of(&pid, &rt.workspace),
            "root_path": rt.workspace.to_string_lossy().into_owned(),
            "state": v.state,
            "files_synced": v.files_synced,
            "cursor": v.cursor,
            "pending_outbox": v.pending_outbox,
            "last_error": v.last_error,
        }));
    }
    out.sort_by(|a, b| {
        a["project_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project_id"].as_str().unwrap_or(""))
    });
    json!({ "projects": out })
}

/// GET /api/v1/leases — every local lease with expiry state.
pub fn leases(state: &DaemonState) -> serde_json::Value {
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
    json!({ "leases": out })
}

/// GET /api/v1/storage — CAS stats, files summary, and the REAL per-volume
/// disk meter: the home store's disk AND every attached workspace's disk
/// (a video studio's project drive is rarely the system drive).
pub async fn storage(state: &DaemonState) -> serde_json::Value {
    let key = format!("storage:{}", state.home.display());
    if let Some(v) = cached_view(&key) {
        return v;
    }
    let v = storage_uncached(state).await;
    store_view(&key, &v);
    v
}

async fn storage_uncached(state: &DaemonState) -> serde_json::Value {
    let home = state.home.as_path();
    let Some(store) = open_store(home) else {
        return json!({"ok": false, "error": "store unavailable"});
    };
    let conn = store.conn_handle();
    let (blob_count, blob_bytes, pinned_count, pinned_bytes) =
        match cairn_store::Cas::open(&store.root().join("blobs"), conn) {
            Ok(cas) => cas.blob_stats().unwrap_or((0, 0, 0, 0)),
            Err(_) => (0, 0, 0, 0),
        };
    let disk = cairn_store::eviction::disk_space(store.root()).ok();
    let (files, conflicts) = store.all_files_summary();

    let mut volumes = Vec::new();
    if let Some(d) = &disk {
        volumes.push(json!({
            "label": "store",
            "free_bytes": d.free,
            "total_bytes": d.total,
        }));
    }
    {
        let runtimes = state.projects.list().await;
        let mut seen = std::collections::HashSet::new();
        for rt in &runtimes {
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
    json!({
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
    })
}

/// GET /api/v1/review — the review portal state per attached project:
/// version stack, live links, comment counts. Read-only; minting links is
/// `actions::review_link`.
///
/// ADR-0031 Phase 2: a link tombstoned in the synced records is revoked on
/// EVERY machine — even while this machine's review.json still lists it
/// (the revoke may have happened elsewhere, before this machine's file was
/// touched). Such links are flagged `revoked_remotely` and stop counting as
/// live; the portal refuses them via `RootProvider::link_revocations`.
pub async fn review_summary(state: &DaemonState) -> serde_json::Value {
    if let Some(v) = cached_view("review:") {
        return v;
    }
    let v = review_summary_uncached(state).await;
    store_view("review:", &v);
    v
}

async fn review_summary_uncached(state: &DaemonState) -> serde_json::Value {
    let mut out = Vec::new();
    for rt in state.projects.list().await {
        let root = rt.workspace.clone();
        // ADR-0031 Phase 2 point check per link: tombstoned in the synced
        // records = revoked on EVERY machine (the store may be unavailable —
        // honest absence, the local file still governs)
        let remote_revoked = |store: &Option<cairn_store::Store>, token: &str| {
            store.as_ref().is_some_and(|s| {
                crate::state_records::is_link_revoked_remotely(s, &rt.project_id, token)
            })
        };
        let store = open_store(state.home.as_path());
        let entry = match cairn_review::store::Store::load(&root) {
            Ok(Some(f)) => {
                let now = WallClock.now_millis();
                let comments: u64 = f
                    .versions
                    .iter()
                    .map(|v| {
                        cairn_review::store::Store::load_comments(&root, v.number)
                            .map(|s| s.len() as u64)
                            .unwrap_or(0)
                    })
                    .sum();
                let revoked_flags: Vec<bool> = f
                    .links
                    .iter()
                    .map(|l| remote_revoked(&store, &l.token))
                    .collect();
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
                    "live_links": f.links.iter().zip(&revoked_flags)
                        .filter(|(l, revoked)| !l.is_expired(now) && !**revoked)
                        .count(),
                    "expired_links": f.links.iter().filter(|l| l.is_expired(now)).count(),
                    "revoked_remotely": revoked_flags.iter().filter(|r| **r).count(),
                    // mom-test round: the surface that makes revoke real —
                    // a user cannot kill a link they cannot see.
                    "links": f.links.iter().zip(&revoked_flags).map(|(l, revoked)| json!({
                        "token": l.token,
                        "note": l.note,
                        "role": l.role.as_str(),
                        "expired": l.is_expired(now),
                        "expires_at": l.expires_at,
                        // ADR-0031 Phase 2: tombstoned in the synced records
                        "revoked_remotely": revoked,
                    })).collect::<Vec<_>>(),
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
    out.sort_by(|a, b| {
        a["project_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["project_id"].as_str().unwrap_or(""))
    });
    json!({ "review": out })
}

/// GET /api/v1/markers?project=&version=&format=fcpxml|otio|csv — the NLE
/// marker bridge. The same body the CLI exports (`cairn review
/// export-markers`), so the Premiere UXP panel and the terminal can never
/// disagree. Read-only: comments live in the root's machine-local `.cairn`
/// dir (ADR-0031's honest-scope note); RBAC's write boundary is untouched
/// by a read.
pub async fn markers_export(
    state: &DaemonState,
    project: &str,
    version: u32,
    format: &str,
    visibility: Option<cairn_tl::notes::NoteVisibility>,
) -> super::Export {
    // resolve the root: the named project's runtime, else the first (the
    // panel always names the project; the fallback keeps manual URL fetch
    // on single-project machines working)
    let root: Option<std::path::PathBuf> = if project.is_empty() {
        state
            .projects
            .list()
            .await
            .first()
            .map(|rt| rt.workspace.clone())
    } else {
        state.projects.project_root(project).await
    };
    let Some(root) = root else {
        return super::Export::NotFound(format!("no attached project matches '{project}'"));
    };
    match crate::handoff::markers_payload(&root, version, format, None, visibility) {
        Ok((body, ctype)) => {
            let ext = match format {
                "otio" => "otio",
                "csv" => "csv",
                _ => "fcpxml",
            };
            super::Export::Ready {
                body: body.into_bytes(),
                content_type: ctype.to_string(),
                filename: format!("markers-v{version}.{ext}"),
            }
        }
        Err(e) => super::Export::NotFound(e.to_string()),
    }
}

/// GET /api/v1/files?project=&q= — per-file rows with sync + pin badges
/// (audit #7: "clip1.braw 8.3 MB synced / placeholder / pinned" — a file
/// list, not `ls`). `q` filters cheaply server-side here.
///
/// Media rows also carry `proxy_state` ("none|ready|stale|failed") from the
/// cairn-proxy index (contract freeze §1): CHEAP by design — one index read
/// plus a stat fingerprint per indexed media, NEVER a blake3 rehash of a
/// 50 GB camera master just to draw a table (status_of_fast's fast path,
/// minus its rehash fallback: a fingerprint MISS answers "stale" — the
/// honest "cannot prove current without hashing" — and the status endpoint
/// or `cairn proxy status --verify` settles the doubt).
#[cfg(test)]
pub async fn files(state: &DaemonState, project: &str, needle: &str) -> serde_json::Value {
    files_page_kind(state, project, needle, 0, None, false).await
}

/// True when `path` holds the real bytes locally (not a cloud placeholder,
/// not missing). Windows CfAPI placeholders report their full logical size,
/// so their RECALL/OFFLINE attributes decide; elsewhere a missing file or a
/// short one (bytes not yet fetched) is "not here".
fn is_local_copy(path: &std::path::Path, size: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const OFFLINE: u32 = 0x0000_1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        if meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
            return false;
        }
    }
    meta.len() >= size
}

/// Short-lived cache for read-heavy console views (review #53/#54):
/// storage telemetry (blob stats + disk statvfs) and review summaries are
/// recomputed at most every `VIEW_CACHE_TTL`, however many tabs poll.
/// Mutations that change a cached view call [`invalidate_view`].
pub const VIEW_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(10);
static VIEW_CACHE: std::sync::Mutex<
    Option<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>>,
> = std::sync::Mutex::new(None);

fn cached_view(key: &str) -> Option<serde_json::Value> {
    let g = VIEW_CACHE.lock().ok()?;
    let (at, v) = g.as_ref()?.get(key)?;
    (at.elapsed() < VIEW_CACHE_TTL).then(|| v.clone())
}

fn store_view(key: &str, v: &serde_json::Value) {
    if v["ok"] != json!(true) {
        return; // never cache a failure
    }
    if let Ok(mut g) = VIEW_CACHE.lock() {
        g.get_or_insert_with(Default::default)
            .insert(key.to_owned(), (std::time::Instant::now(), v.clone()));
    }
}

/// Drop a cached view so the next read is fresh (call after mutations).
pub fn invalidate_view(prefix: &str) {
    if let Ok(mut g) = VIEW_CACHE.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|k, _| !k.starts_with(prefix));
        }
    }
}

/// Media the review portal can play (#55: filtered server-side so Review
/// never downloads the whole file list to fill one picker).
pub fn is_review_media(path: &str) -> bool {
    const EXT: [&str; 11] = [
        "mp4", "mov", "m4v", "mkv", "avi", "webm", "mpg", "mpeg", "mxf", "r3d", "braw",
    ];
    path.rsplit_once('.')
        .is_some_and(|(_, e)| EXT.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Hard ceiling on one page (#34): a 200k-file project must never
/// serialize every row into one response or one DOM.
pub const FILES_PAGE_MAX: usize = 5000;

/// GET /api/v1/files with server-side paging (#34). The summary counts
/// cover EVERY matching row; `files` is the sorted window
/// `[offset, offset+limit)`. `total` / `next_offset` let the console say
/// "showing 500 of 12,400" and fetch more on demand instead of silently
/// truncating. `limit = None` keeps the legacy all-rows answer for
/// in-process callers.
#[cfg(test)]
pub async fn files_page(
    state: &DaemonState,
    project: &str,
    needle: &str,
    offset: usize,
    limit: Option<usize>,
) -> serde_json::Value {
    files_page_kind(state, project, needle, offset, limit, false).await
}

/// [`files_page`] with an optional review-media filter (#55).
pub async fn files_page_kind(
    state: &DaemonState,
    project: &str,
    needle: &str,
    offset: usize,
    limit: Option<usize>,
    media_only: bool,
) -> serde_json::Value {
    let needle = needle.to_lowercase();
    let Some(store) = open_store(state.home.as_path()) else {
        return json!({"ok": false, "error": "store unavailable"});
    };
    // proxy index → media_rel → ready|stale|failed (latest entry per media)
    let mut proxy_states: std::collections::HashMap<String, &'static str> = Default::default();
    if !project.is_empty() {
        if let Some(root) = state.projects.project_root(project).await {
            if let Ok(bytes) = std::fs::read(cairn_proxy::pipeline::index_path(&root)) {
                if let Ok(idx) = cairn_proxy::model::ProxyIndex::from_json(&bytes) {
                    let mut latest: std::collections::HashMap<
                        &str,
                        &cairn_proxy::model::ProxyEntry,
                    > = Default::default();
                    for e in idx.proxies.values() {
                        match latest.get(e.media_rel.as_str()) {
                            Some(cur) if cur.generated_at_ms >= e.generated_at_ms => {}
                            _ => {
                                latest.insert(e.media_rel.as_str(), e);
                            }
                        }
                    }
                    for (media_rel, e) in latest {
                        let st = if e.last_error.is_some() {
                            "failed"
                        } else if let (Some(want_len), Some(want_mtime)) =
                            (e.source_len, e.source_mtime_ms)
                        {
                            match cairn_proxy::pipeline::source_fingerprint(&root.join(media_rel)) {
                                Ok((len, mtime)) if len == want_len && mtime == want_mtime => {
                                    "ready"
                                }
                                _ => "stale",
                            }
                        } else {
                            // pre-stat-tracking entry: readiness needs a rehash,
                            // which this view refuses to pay
                            "stale"
                        };
                        proxy_states.insert(media_rel.to_string(), st);
                    }
                }
            }
        }
    }
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
            .list_pins(project)
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    };
    // CONTRACT-DEBT #1: pending semantic-merge offers keyed by the SAME
    // namespace the file rows use — one row -> one "merge available" badge.
    let offers: std::collections::HashSet<String> = if project.is_empty() {
        Default::default()
    } else {
        store
            .list_merge_offers(project)
            .into_iter()
            .map(|o| o.path)
            .collect()
    };
    let merge_offers_n = offers.len() as u64; // decisions waiting, project-wide
    for row in store.list_files(project) {
        if row.mode != "file" {
            continue;
        }
        if !needle.is_empty() && !row.path.to_lowercase().contains(&needle) {
            continue;
        }
        if media_only && !is_review_media(&row.path) {
            continue;
        }
        total_files += 1;
        let merge_available = offers.contains(&row.path);
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
            "merge_available": merge_available,
            // decided per page row below (#placeholder-truth): needs a stat
            "placeholder": false,
            "local_state": row.local_state,
            "proxy_state": proxy_states.get(&row.path).copied().unwrap_or("none"),
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
    summary.insert("merge_offers".into(), json!(merge_offers_n));
    let total = rows_out.len();
    let limit = limit
        .map(|l| l.clamp(1, FILES_PAGE_MAX))
        .unwrap_or(total.max(1));
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let next_offset = (end < total).then_some(end);
    let mut page: Vec<serde_json::Value> = rows_out.drain(start..end).collect();
    // "Is this file actually on this computer?" The old answer was
    // `manifest_hash.is_some()`, which is true for EVERY synced file, so
    // Home told the machine that owns the media that all of it was
    // missing. The truth is on disk; stat only the returned page (bounded
    // by the page size, never the whole project per 2s refresh).
    if let Some(root) = state.projects.project_root(project).await {
        for row in &mut page {
            let rel = row["path"].as_str().unwrap_or_default().to_owned();
            let size = row["size"].as_u64().unwrap_or(0);
            let dirty = row["local_state"] == "dirty";
            row["placeholder"] = json!(!dirty && !is_local_copy(&root.join(&rel), size));
            if let Some(obj) = row.as_object_mut() {
                obj.remove("local_state");
            }
        }
    } else {
        for row in &mut page {
            if let Some(obj) = row.as_object_mut() {
                obj.remove("local_state");
            }
        }
    }
    json!({
        "ok": true,
        "project": project,
        "summary": summary,
        "files": page,
        "total": total,
        "offset": start,
        "limit": limit,
        "next_offset": next_offset,
    })
}

/// GET /api/v1/team — members, the acting device's role, the swarm join
/// code (invite), and the newest audit decisions (audit #5: RBAC was in
/// the CLI only; now the studio roster is a first-class surface).
/// #10: People is project-scoped. `project` names the console's current
/// project; empty keeps the single-project convenience (first attached).
pub async fn team(state: &DaemonState, project: &str) -> serde_json::Value {
    let picked = if project.is_empty() {
        state.projects.first().await
    } else {
        state.projects.view_of(project).await
    };
    let Some((pid, root, files_synced, pending)) = picked else {
        return json!({"ok": true, "projects": []});
    };
    let store = match open_store(state.home.as_path()) {
        Some(s) => s,
        None => return json!({"ok": false, "error": "store unavailable"}),
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
    // ADR-0031 Phase 1 merged read surface: the members file above stays the
    // ENFORCEMENT authority (Phase-1 honesty — machine-local file rules), so
    // these synced copies ADD to the response, never replace it. They show
    // what the roster convergence will look like on every attached machine
    // (and what is still missing): local publishes + everything replayed
    // from peers, straight from the home store's record table.
    let mut synced_members: Vec<serde_json::Value> = store
        .list_state_records(&pid, "member")
        .iter()
        .map(
            |r| match serde_json::from_slice::<serde_json::Value>(&r.payload) {
                Ok(serde_json::Value::Object(mut obj)) => {
                    obj.insert("tombstone".into(), json!(r.tombstone));
                    serde_json::Value::Object(obj)
                }
                _ => json!({
                    "payload": String::from_utf8_lossy(&r.payload),
                    "tombstone": r.tombstone,
                }),
            },
        )
        .collect();
    synced_members.sort_by(|a, b| {
        a["device_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["device_id"].as_str().unwrap_or(""))
    });
    let audit_count = store.list_state_records(&pid, "audit").len();
    json!({
        "ok": true,
        "projects": [{
            "project_id": pid,
            "display_name": display_name_of(&pid, &root),
            "root_path": root.to_string_lossy(),
            "files_synced": files_synced,
            "pending_outbox": pending,
            "my_device": device,
            "my_role": my_role,
            "unlisted_role": crate::members::load(&root)
                .map(|f| f.unlisted_default().as_str().to_string())
                .unwrap_or_else(|_| "editor".into()),
            "members": members,
            "join_code": if join_code.is_empty() { serde_json::Value::Null } else { json!(join_code) },
            "signal": if signal.is_empty() { serde_json::Value::Null } else { json!(signal) },
            "audit": audit,
            "synced_records": {
                "members": synced_members,
                "audit_count": audit_count,
            },
            "now_ms": now_ms_i64(),
        }],
    })
}

/// GET /api/v1/search?q= — substring search across file paths, project
/// ids/display names, review session titles, and audit actions (audit
/// #9: search existed only as a CLI; editors live in the dashboard).
pub async fn search(state: &DaemonState, needle: &str) -> serde_json::Value {
    let needle = needle.to_lowercase();
    if needle.trim().is_empty() {
        return json!({"ok": true, "results": []});
    }
    let mut out = Vec::new();
    // attached projects: (project_id, workspace, display name)
    let attached: Vec<(String, std::path::PathBuf, String)> = state
        .projects
        .list()
        .await
        .iter()
        .map(|rt| {
            (
                rt.project_id.clone(),
                rt.workspace.clone(),
                display_name_of(&rt.project_id, &rt.workspace),
            )
        })
        .collect();
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
        // file paths across ALL attached projects (#82): a fair share per
        // project so one big project cannot crowd out the rest, 40 total
        let per_project = (40 / attached.len().max(1)).max(8);
        let mut file_hits = 0;
        for (pid, _root, _display) in &attached {
            if file_hits >= 40 {
                break;
            }
            let mut this_project = 0;
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
                    this_project += 1;
                    if file_hits >= 40 || this_project >= per_project {
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
    json!({"ok": true, "results": out})
}

/// GET /api/v1/update — honest update state. The daemon never phones
/// home; `cairn update check` (CLI) writes its verdict into the store and
/// this surfaces it: null = never checked, false = checked-current,
/// true = an update is offered. check_failed marks a failed check.
pub fn update_state(state: &DaemonState) -> serde_json::Value {
    let offered = open_store(state.home.as_path())
        .and_then(|s| s.meta_get("update/offered"))
        .map(|v| v == "true")
        .unwrap_or(false);
    let check_failed = open_store(state.home.as_path())
        .and_then(|s| s.meta_get("update/check-failed"))
        .map(|v| v == "true")
        .unwrap_or(false);
    json!({
        "ok": true,
        "current_version": env!("CARGO_PKG_VERSION"),
        "update_offered": offered,
        "check_failed": check_failed,
    })
}

/// GET /api/v1/live/snapshot — the current presence view per project
/// (remote peers from each swarm's last-event-wins map; local events are
/// stream-only). Honest `enabled` field so the UI can show the off state.
pub async fn live_snapshot(state: &DaemonState) -> serde_json::Value {
    let on = {
        state
            .flags
            .read()
            .await
            .iter()
            .any(|(k, v)| k == "live_presence" && v == "true")
    };
    let mut projects_out = Vec::new();
    for rt in state.projects.list().await {
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
                "project": rt.project_id,
                "events": events,
            }));
        }
    }
    json!({ "enabled": on, "projects": projects_out })
}

/// GET /api/v1/doctor — the 5s-fresh cached doctor report (round 27: the
/// dashboard polls this at 15s; the status RPC shares the same cache, so
/// neither starves the ctl thread).
/// Redact anything secret-shaped from text bound for a diagnostics file:
/// long alphanumeric runs that contain a digit (tokens, keys, join codes,
/// link tokens, uuids) and the value after `token`/`secret`/`password`/
/// `code`/`authorization` keys (`k=v`, `k: v`, `"k":"v"`). Paths, words
/// and messages survive.
pub fn redact_secrets(text: &str) -> String {
    const R: &str = "[redacted]";
    const KEYS: [&str; 5] = ["token", "secret", "password", "code", "authorization"];
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    // pass 1: secret-shaped runs
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let secretish = run.len() >= 20 && run.chars().any(|c| c.is_ascii_digit());
        out.push_str(if secretish { R } else { run.as_str() });
        run.clear();
    };
    for c in text.chars() {
        if is_word(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    // pass 2: values after secret keys (ASCII lowering keeps byte offsets)
    let mut res = String::with_capacity(out.len());
    let lower = out.to_ascii_lowercase();
    let mut i = 0;
    'scan: while i < out.len() {
        for key in KEYS {
            if lower[i..].starts_with(key)
                && (i == 0 || !is_word(out[..i].chars().last().unwrap_or(' ')))
            {
                let mut j = i + key.len();
                let rest = &out[j..];
                let skip = rest.len() - rest.trim_start_matches(['"', '\'']).len();
                j += skip;
                let rest = &out[j..];
                if rest.starts_with('=') || rest.starts_with(':') {
                    j += 1;
                    let rest = &out[j..];
                    j += rest.len() - rest.trim_start_matches([' ', '"', '\'']).len();
                    let val_end = out[j..]
                        .find(|c: char| {
                            c == '"' || c == '\'' || c == '&' || c == ',' || c.is_whitespace()
                        })
                        .map_or(out.len(), |e| j + e);
                    if val_end > j {
                        res.push_str(&out[i..j]);
                        res.push_str(R);
                        i = val_end;
                        continue 'scan;
                    }
                }
            }
        }
        let c = out[i..].chars().next().unwrap();
        res.push(c);
        i += c.len_utf8();
    }
    res
}

/// #93 Send diagnostics: one JSON bundle a user can attach to a support
/// message — version, platform, uptime, per-project health, the doctor
/// report, flags, and the tail of `<home>/daemon.log` (the tray's log
/// file, ADR-0029). Everything secret-shaped is redacted; nothing leaves
/// the machine unless the user sends the file themselves.
pub async fn diagnostics(state: &DaemonState) -> serde_json::Value {
    let projects: Vec<serde_json::Value> = state
        .projects
        .list()
        .await
        .into_iter()
        .map(|rt| json!({"project_id": rt.project_id, "root": rt.workspace.display().to_string()}))
        .collect();
    let mut views = Vec::new();
    for p in &projects {
        let pid = p["project_id"].as_str().unwrap_or_default();
        if let Some((_, _, synced, pending)) = state.projects.view_of(pid).await {
            views.push(
                json!({"project_id": pid, "files_synced": synced, "pending_outbox": pending}),
            );
        }
    }
    let log_tail = std::fs::read(state.home.join("daemon.log")).ok().map(|b| {
        let text = String::from_utf8_lossy(&b[b.len().saturating_sub(64 * 1024)..]).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        redact_secrets(&lines[lines.len().saturating_sub(300)..].join("\n"))
    });
    let flags: Vec<serde_json::Value> = state
        .flags
        .read()
        .await
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect();
    json!({
        "ok": true,
        "kind": "cairn-diagnostics",
        "schema": 1,
        "generated_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_s": state.started.elapsed().as_secs(),
        "media_jobs": {
            "running": crate::service::media_jobs().0,
            "waiting": crate::service::media_jobs().1,
            "slots": crate::service::MEDIA_JOB_SLOTS,
        },
        "projects": projects,
        "project_views": views,
        "flags": flags,
        "doctor": doctor(state).await,
        "daemon_log_tail": log_tail,
    })
}

pub async fn doctor(state: &DaemonState) -> serde_json::Value {
    let report = state.cached_doctor().await;
    json!({
        "healthy": report.healthy(),
        "checks": report.checks.iter().map(|c| json!({
            "name": c.name, "ok": c.ok, "detail": c.detail, "latency_ms": c.latency_ms
        })).collect::<Vec<_>>(),
    })
}

/// GET /api/v1/flags — the daemon-side kill-switch mirror.
pub async fn flags(state: &DaemonState) -> serde_json::Value {
    let flags = state.flags.read().await;
    json!({
        "flags": flags.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>(),
    })
}

/// GET /api/v1/state-records?project=&family= — the raw synced record table
/// (ADR-0031 Phase 1 read surface): one family of project-scoped
/// collaboration records exactly as the LOCAL store holds them — local
/// publishes plus everything replayed from peers. Unknown/empty family →
/// an honest empty list (the known set is `cairn_sync::state_records::
/// FAMILIES`); an empty `project` is resolved to the first attached one by
/// the HTTP adapter (the team/markers convention), and a project with no
/// records is simply an empty list — absence stays absent.
pub fn state_records(state: &DaemonState, project: &str, family: &str) -> serde_json::Value {
    let records: Vec<serde_json::Value> = if cairn_sync::state_records::FAMILIES.contains(&family) {
        open_store(state.home.as_path())
            .map(|store| {
                store
                    .list_state_records(project, family)
                    .iter()
                    .map(|r| {
                        json!({
                            "record_id": r.record_id,
                            "key": r.key,
                            "ts_ms": r.ts_ms,
                            "device_id": r.device_id,
                            "tombstone": r.tombstone,
                            "payload": serde_json::from_slice::<serde_json::Value>(&r.payload)
                                .unwrap_or_else(|_| {
                                    serde_json::Value::String(String::from_utf8_lossy(&r.payload).into_owned())
                                }),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    json!({"ok": true, "project": project, "family": family, "records": records})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 6 (CONTRACT-DEBT #1 read surface): the files view must surface
    /// the pending semantic-merge offer as a per-row `merge_available` badge
    /// plus a project-wide `summary.merge_offers` count — the affordance an
    /// editor actually clicks. Rows without an offer stay `false`; the count
    /// is the number of DECISIONS waiting, not the number of rows.
    /// (Contract freeze §1: rows also carry `proxy_state` — "none" here,
    /// since this store has no proxy index.)
    #[tokio::test]
    async fn files_rows_expose_merge_available() {
        let home = tempfile::tempdir().unwrap();
        {
            let store = open_store(home.path()).unwrap();
            for path in ["hero.otio", "broll.otio"] {
                store
                    .put_file(&cairn_store::FileRow {
                        path: path.into(),
                        project_id: "p1".into(),
                        manifest_hash: Some("aa11".into()),
                        size: 10,
                        mode: "file".into(),
                        mtime: 1,
                        local_state: "synced".into(),
                    })
                    .unwrap();
            }
            store
                .upsert_merge_offer(&cairn_store::MergeOfferRow {
                    project_id: "p1".into(),
                    path: "hero.otio".into(),
                    copy_path: "hero (conflict — dev — 2026-10-08).otio".into(),
                    base_manifest: "bb22".into(),
                    theirs_manifest: "cc33".into(),
                    report_json: r#"{"policy":"semantic","outcome":"notes"}"#.into(),
                    created_ms: 1,
                })
                .unwrap();
        }
        // DaemonState fields are all pub; `new` is daemon-private and the view
        // only reads `home` — the rest of the shape is inert defaults.
        let state = DaemonState {
            home: home.path().to_path_buf(),
            started: std::time::Instant::now(),
            flags: tokio::sync::RwLock::new(Vec::new()),
            recall_jobs: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            projects: std::sync::Arc::new(crate::projects::ProjectManager::new()),
            doctor_cache: tokio::sync::RwLock::new(None),
        };

        let out = files(&state, "p1", "").await;
        assert_eq!(out["ok"], json!(true));
        let rows = out["files"].as_array().expect("files array");
        let hero = rows
            .iter()
            .find(|r| r["path"] == json!("hero.otio"))
            .expect("hero row present");
        assert_eq!(hero["merge_available"], json!(true), "offer -> badge");
        assert_eq!(hero["proxy_state"], json!("none"), "no proxy index -> none");
        let broll = rows
            .iter()
            .find(|r| r["path"] == json!("broll.otio"))
            .expect("broll row present");
        assert_eq!(
            broll["merge_available"],
            json!(false),
            "no offer -> no badge"
        );
        assert_eq!(out["summary"]["merge_offers"], json!(1));

        // project scoping: another namespace sees neither rows nor offers
        let other = files(&state, "p2", "").await;
        assert_eq!(other["files"].as_array().unwrap().len(), 0);
        assert_eq!(other["summary"]["merge_offers"], json!(0));
    }

    /// #34: paging returns a sorted window while the summary still counts
    /// every matching row, and `next_offset` walks to the end exactly once.
    #[tokio::test]
    async fn files_are_paged_server_side() {
        let home = tempfile::tempdir().unwrap();
        {
            let store = open_store(home.path()).unwrap();
            for i in 0..25 {
                store
                    .put_file(&cairn_store::FileRow {
                        path: format!("clip{i:02}.mov"),
                        project_id: "p1".into(),
                        manifest_hash: Some("aa11".into()),
                        size: 10,
                        mode: "file".into(),
                        mtime: 1,
                        local_state: "synced".into(),
                    })
                    .unwrap();
            }
        }
        let state = DaemonState {
            home: home.path().to_path_buf(),
            started: std::time::Instant::now(),
            flags: tokio::sync::RwLock::new(Vec::new()),
            recall_jobs: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            projects: std::sync::Arc::new(crate::projects::ProjectManager::new()),
            doctor_cache: tokio::sync::RwLock::new(None),
        };
        let mut seen = Vec::new();
        let mut offset = 0usize;
        let mut pages = 0;
        loop {
            let out = files_page(&state, "p1", "", offset, Some(10)).await;
            assert_eq!(out["total"], json!(25));
            assert_eq!(out["summary"]["files"], json!(25), "summary spans all rows");
            for r in out["files"].as_array().unwrap() {
                seen.push(r["path"].as_str().unwrap().to_owned());
            }
            pages += 1;
            match out["next_offset"].as_u64() {
                Some(n) => offset = n as usize,
                None => break,
            }
        }
        assert_eq!(pages, 3);
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "pages are in path order");
        sorted.dedup();
        assert_eq!(sorted.len(), 25, "no row repeated or skipped");

        // past-the-end offset is an empty page, not an error
        let out = files_page(&state, "p1", "", 999, Some(10)).await;
        assert_eq!(out["files"].as_array().unwrap().len(), 0);
        assert_eq!(out["next_offset"], json!(null));
        // limit is clamped to the ceiling
        let out = files_page(&state, "p1", "", 0, Some(1_000_000)).await;
        assert_eq!(out["limit"], json!(FILES_PAGE_MAX));
    }

    #[test]
    fn diagnostics_redaction_removes_secrets_keeps_context() {
        let input = concat!(
            r#"{"level":"WARN","message":"link minted","token":"abcDEF1234567890xyzQ","path":"cuts/hero.mov"}"#,
            "\nGET /r/7f3a9c2e1b4d5f6a7b8c9d0e1f2a3b4c/session?t=deadbeefcafe0011223344\n",
            "join code: 4821-9931 secret=hunter2 Authorization: Bearer xyz\n",
            "plain message with words only and a short id 12ab\n",
        );
        let out = redact_secrets(input);
        for leaked in [
            "abcDEF1234567890xyzQ",
            "7f3a9c2e1b4d5f6a7b8c9d0e1f2a3b4c",
            "deadbeefcafe0011223344",
            "4821-9931",
            "hunter2",
            "Bearer",
        ] {
            assert!(!out.contains(leaked), "leaked {leaked}: {out}");
        }
        for kept in [
            "link minted",
            "cuts/hero.mov",
            "plain message with words only",
            "12ab",
            "/session",
        ] {
            assert!(out.contains(kept), "lost context {kept}: {out}");
        }
    }

    /// Placeholder truth: a synced file that is really on disk is NOT a
    /// placeholder (the old manifest_hash rule said every synced file was).
    #[test]
    fn local_copy_is_decided_by_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let full = d.path().join("full.mov");
        std::fs::write(&full, vec![1u8; 4096]).unwrap();
        assert!(is_local_copy(&full, 4096), "real bytes on disk = local");
        let short = d.path().join("short.mov");
        std::fs::write(&short, vec![1u8; 10]).unwrap();
        assert!(
            !is_local_copy(&short, 4096),
            "bytes not fetched yet = not here"
        );
        assert!(
            !is_local_copy(&d.path().join("missing.mov"), 1),
            "missing = not here"
        );
    }

    #[test]
    fn review_media_filter_and_view_cache() {
        assert!(is_review_media("cuts/Hero_v3.MOV"));
        assert!(is_review_media("a.braw"));
        assert!(!is_review_media("edit/project.prproj"));
        assert!(!is_review_media("noext"));
        let v = json!({"ok": true, "n": 1});
        store_view("test:a", &v);
        assert_eq!(cached_view("test:a"), Some(v));
        invalidate_view("test:");
        assert_eq!(
            cached_view("test:a"),
            None,
            "invalidated views are recomputed"
        );
        store_view("test:b", &json!({"ok": false}));
        assert_eq!(cached_view("test:b"), None, "failures are never cached");
    }
}
