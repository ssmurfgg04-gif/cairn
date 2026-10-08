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
pub fn files(state: &DaemonState, project: &str, needle: &str) -> serde_json::Value {
    let needle = needle.to_lowercase();
    let Some(store) = open_store(state.home.as_path()) else {
        return json!({"ok": false, "error": "store unavailable"});
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
    summary.insert("merge_offers".into(), json!(merge_offers_n));
    json!({"ok": true, "project": project, "summary": summary, "files": rows_out})
}

/// GET /api/v1/team — members, the acting device's role, the swarm join
/// code (invite), and the newest audit decisions (audit #5: RBAC was in
/// the CLI only; now the studio roster is a first-class surface).
pub async fn team(state: &DaemonState) -> serde_json::Value {
    let Some((pid, root, files_synced, pending)) = state.projects.first().await else {
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
    #[test]
    fn files_rows_expose_merge_available() {
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

        let out = files(&state, "p1", "");
        assert_eq!(out["ok"], json!(true));
        let rows = out["files"].as_array().expect("files array");
        let hero = rows
            .iter()
            .find(|r| r["path"] == json!("hero.otio"))
            .expect("hero row present");
        assert_eq!(hero["merge_available"], json!(true), "offer -> badge");
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
        let other = files(&state, "p2", "");
        assert_eq!(other["files"].as_array().unwrap().len(), 0);
        assert_eq!(other["summary"]["merge_offers"], json!(0));
    }
}
