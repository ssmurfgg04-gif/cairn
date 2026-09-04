//! The guest-facing review portal HTTP surface (ADR-0020): token-gated
//! routes only — no dashboard state, no store internals, nothing but what
//! a client with a valid link may see.
//!
//! Binding policy differs from the ADR-0009 loopback dashboard on
//! purpose: this listener is meant for clients on the LAN/VPN (or a
//! port-forward), so it is OFF by default and enabled explicitly
//! (`cairn daemon --review 0.0.0.0:17778`). Every route resolves the
//! token first and fails closed; the only identity is the link token
//! itself (122 CSPRNG bits) plus its role and expiry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::Stream;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use cairn_tl::notes::NoteStatus;

use crate::model::{GuestLink, ReviewFile};
use crate::store::Store;

const REVIEW_HTML: &str = include_str!("../assets/review.html");
const REVIEW_CSS: &str = include_str!("../assets/review.css");
const REVIEW_JS: &str = include_str!("../assets/review.js");

/// Media chunks per range response (8 MiB — browsers issue follow-up
/// range fetches; a capped 206 keeps memory flat for any file size).
const CHUNK: u64 = 8 * 1024 * 1024;

/// A live reviewer signal (ephemeral by design — never persisted, never
/// synced: presence is "who is watching right now", not state).
#[derive(Clone, Debug)]
pub struct PresenceEntry {
    pub reviewer: String,
    pub version: u32,
    pub frame: u64,
    pub seen_at_ms: i64,
}

/// Where the portal finds project roots. The daemon implements this over
/// its attached runtimes; tests over tempdirs.
#[async_trait::async_trait]
pub trait RootProvider: Send + Sync {
    /// (project_id, workspace root) for every attached project.
    async fn roots(&self) -> Vec<(String, PathBuf)>;
    /// Wall clock millis (overridable in tests).
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Shared portal state: root provider + in-memory presence keyed by
/// (token, reviewer).
#[derive(Clone)]
pub struct Portal {
    provider: Arc<dyn RootProvider>,
    presence: Arc<Mutex<HashMap<(String, String), PresenceEntry>>>,
}
impl Portal {
    pub fn new(provider: Arc<dyn RootProvider>) -> Portal {
        Portal {
            provider,
            presence: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve a token to (root, session, link), failing closed on
    /// unknown/expired tokens.
    async fn resolve(&self, token: &str) -> Option<(PathBuf, ReviewFile, GuestLink)> {
        let now = self.provider.now_ms();
        for (_pid, root) in self.provider.roots().await {
            if let Ok(Some(f)) = Store::load(&root) {
                if let Some(l) = GuestLink::resolve(&f.links, token, now) {
                    let link = l.clone(); // end the borrow before moving f
                    return Some((root, f, link));
                }
            }
        }
        None
    }

    /// Presence for one token, freshest first, stale entries dropped
    /// (90 s without a heartbeat).
    fn presence(&self, token: &str) -> Vec<PresenceEntry> {
        let now = self.provider.now_ms();
        let mut guard = self.presence.lock().expect("presence lock");
        guard.retain(|(t, _), e| t != token || now.saturating_sub(e.seen_at_ms) < 90_000);
        guard
            .iter()
            .filter(|((t, _), _)| t == token)
            .map(|(_, e)| e.clone())
            .collect()
    }
}

/// Serve the portal on `addr` (run forever).
pub async fn serve(addr: String, portal: Portal) -> std::io::Result<()> {
    let app = router(portal);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "review portal listening (token-gated routes only)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The portal router (also mounted by tests directly).
pub fn router(portal: Portal) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/review.css", get(css))
        .route("/assets/review.js", get(js))
        .route("/r/:token", get(player))
        .route("/r/:token/api/session", get(session))
        .route("/r/:token/api/comment", post(comment))
        .route("/r/:token/api/resolve", post(resolve))
        .route("/r/:token/api/presence", post(presence))
        .route("/r/:token/media/:version", get(media))
        .with_state(portal)
}

async fn index() -> Html<&'static str> {
    Html(REVIEW_HTML)
}

async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        REVIEW_CSS,
    )
}

async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        REVIEW_JS,
    )
}

/// The player page (static shell; JS bootstraps from the session API).
async fn player(UrlPath(_token): UrlPath<String>) -> Html<&'static str> {
    Html(REVIEW_HTML)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({
            "ok": false,
            "error": msg,
        })),
    )
        .into_response()
}

/// GET /r/:token/api/session — everything the player needs in one call.
async fn session(State(p): State<Portal>, UrlPath(token): UrlPath<String>) -> Response {
    let Some((root, file, link)) = p.resolve(&token).await else {
        return err(StatusCode::NOT_FOUND, "link not found or expired");
    };
    let versions: Vec<serde_json::Value> = file
        .versions_for(&link)
        .iter()
        .map(|v| {
            // honest proxy state: a promised proxy that is not on disk yet
            // (still generating, pruned) falls back to full-res serving —
            // the player shows a chip instead of a dead screen
            let proxy_ready = v
                .proxy_rel
                .as_ref()
                .map(|pr| root.join(pr.trim_start_matches(['/', '\\'])).is_file())
                .unwrap_or(false);
            json!({
                "number": v.number,
                "label": v.label,
                "frames": v.frames,
                "fps_num": v.fps_num,
                "fps_den": v.fps_den,
                "tc_rate": v.tc_rate(),
                "media": v.media_rel,
                "proxy": v.proxy_rel,
                "has_proxy": v.proxy_rel.is_some(),
                "proxy_ready": proxy_ready,
                "timeline_fingerprint": v.timeline_fingerprint,
                "published_by": v.published_by,
                "published_at": v.published_at,
            })
        })
        .collect();
    // comments across visible versions
    let mut comments: Vec<serde_json::Value> = Vec::new();
    for v in file.versions_for(&link) {
        if let Ok(set) = Store::load_comments(&root, v.number) {
            for n in set.notes.values() {
                comments.push(json!({
                    "id": n.id,
                    "version": v.number,
                    "frame": n.anchor.frame,
                    "tc": v.timecode(n.anchor.frame.max(0) as u64),
                    "author": n.author,
                    "body": n.body,
                    "status": n.status.as_str(),
                    "created_ms": n.created_ms,
                }));
            }
        }
    }
    let presence: Vec<serde_json::Value> = p
        .presence(&token)
        .into_iter()
        .map(|e| {
            json!({
                "reviewer": e.reviewer,
                "version": e.version,
                "frame": e.frame,
                "seen_at_ms": e.seen_at_ms,
            })
        })
        .collect();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": true,
            "title": file.title,
            "role": link.role.as_str(),
            "note": link.note,
            "latest_only": link.latest_only,
            "versions": versions,
            "comments": comments,
            "presence": presence,
        })),
    )
        .into_response()
}

/// POST /r/:token/api/presence — heartbeat.
async fn presence(
    State(p): State<Portal>,
    UrlPath(token): UrlPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let Some(Json(v)) = body else {
        return err(
            StatusCode::BAD_REQUEST,
            "body required: {reviewer, version, frame}",
        );
    };
    let Some(()) = p.resolve(&token).await.map(|_| ()) else {
        return err(StatusCode::NOT_FOUND, "link not found or expired");
    };
    let reviewer = v["reviewer"].as_str().unwrap_or("guest").trim().to_string();
    let reviewer = if reviewer.is_empty() {
        "guest".to_string()
    } else {
        reviewer
    };
    let entry = PresenceEntry {
        reviewer: reviewer.clone(),
        version: v["version"].as_u64().unwrap_or(0) as u32,
        frame: v["frame"].as_u64().unwrap_or(0),
        seen_at_ms: p.provider.now_ms(),
    };
    {
        let mut guard = p.presence.lock().expect("presence lock");
        guard.insert((token, reviewer), entry);
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"ok": true})),
    )
        .into_response()
}

/// POST /r/:token/api/comment — frame-anchored, content-deduped.
async fn comment(
    State(p): State<Portal>,
    UrlPath(token): UrlPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let Some(Json(v)) = body else {
        return err(
            StatusCode::BAD_REQUEST,
            "body required: {version, frame, body, author}",
        );
    };
    let Some((root, file, link)) = p.resolve(&token).await else {
        return err(StatusCode::NOT_FOUND, "link not found or expired");
    };
    if !link.role.can_comment() {
        return err(StatusCode::FORBIDDEN, "viewer links cannot comment");
    }
    let version = v["version"].as_u64().unwrap_or(0) as u32;
    // visibility: a latest_only link may only touch versions it can see
    // (404, not 403 — never confirm the existence of hidden versions)
    if !file
        .versions_for(&link)
        .iter()
        .any(|vv| vv.number == version)
    {
        return err(StatusCode::NOT_FOUND, "no such version");
    }
    let Some(vn) = file.version(version) else {
        return err(StatusCode::NOT_FOUND, "no such version");
    };
    let frame = v["frame"].as_u64().unwrap_or(0);
    if frame >= vn.frames {
        return err(StatusCode::BAD_REQUEST, "frame beyond end of cut");
    }
    let body_text = v["body"].as_str().unwrap_or("").trim();
    if body_text.is_empty() || body_text.len() > 2000 {
        return err(
            StatusCode::BAD_REQUEST,
            "comment body required (1..2000 chars)",
        );
    }
    let author = v["author"].as_str().unwrap_or("guest").trim();
    let author = if author.is_empty() {
        "guest".to_string()
    } else {
        author.to_string()
    };
    match Store::add_comment(
        &root,
        version,
        &author,
        body_text,
        frame,
        vn.tc_rate(),
        p.provider.now_ms(),
    ) {
        Ok(n) => Json(json!({
            "ok": true,
            "id": n.id,
            "tc": vn.timecode(frame),
            "status": n.status.as_str(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// POST /r/:token/api/resolve — mark resolved / reopened.
async fn resolve(
    State(p): State<Portal>,
    UrlPath(token): UrlPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let Some(Json(v)) = body else {
        return err(
            StatusCode::BAD_REQUEST,
            "body required: {version, id, status}",
        );
    };
    let Some((root, file, link)) = p.resolve(&token).await else {
        return err(StatusCode::NOT_FOUND, "link not found or expired");
    };
    if !link.role.can_comment() {
        return err(StatusCode::FORBIDDEN, "viewer links cannot resolve");
    }
    let version = v["version"].as_u64().unwrap_or(0) as u32;
    // visibility: a latest_only link may only touch versions it can see
    if !file
        .versions_for(&link)
        .iter()
        .any(|vv| vv.number == version)
    {
        return err(StatusCode::NOT_FOUND, "no such version");
    }
    if file.version(version).is_none() {
        return err(StatusCode::NOT_FOUND, "no such version");
    }
    let id = v["id"].as_str().unwrap_or("").to_string();
    let status = match v["status"].as_str().unwrap_or("RESOLVED") {
        "OPEN" => NoteStatus::Open,
        "REJECTED" => NoteStatus::Rejected,
        _ => NoteStatus::Resolved,
    };
    match Store::set_status(&root, version, &id, status) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, &e),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    }
}

/// Containment check: the served path must live inside the project root
/// (the session file is editor-authored, but defense in depth is cheap).
fn contained(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches(['/', '\\']);
    let p = root.join(rel);
    let canon = p.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    if canon.starts_with(&root_canon) {
        Some(canon)
    } else {
        None
    }
}

/// Parse `Range: bytes=start-end` (open ends supported).
fn parse_range(h: Option<&HeaderValue>, len: u64) -> Option<(u64, u64)> {
    let raw = h?.to_str().ok()?.trim();
    let spec = raw.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        len.saturating_sub(1)
    } else {
        b.parse::<u64>().ok()?.min(len.saturating_sub(1))
    };
    if start <= end && start < len {
        Some((start, end))
    } else {
        None
    }
}

/// GET /r/:token/media/:version?full=1 — HTTP-range media serving so the
/// browser can scrub. Default stream is the proxy when the version has
/// one (ADR-0020 §3: remote reviewers pull MBs, not GBs); `?full=1`
/// serves the original media.
async fn media(
    State(p): State<Portal>,
    UrlPath((token, version_str)): UrlPath<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some((root, file, link)) = p.resolve(&token).await else {
        return err(StatusCode::NOT_FOUND, "link not found or expired");
    };
    let Ok(version) = version_str.parse::<u32>() else {
        return err(StatusCode::BAD_REQUEST, "version must be a number");
    };
    // visibility: a latest_only link may only stream versions it can see
    // (404, not 403 — never confirm the existence of hidden versions)
    if !file
        .versions_for(&link)
        .iter()
        .any(|vv| vv.number == version)
    {
        return err(StatusCode::NOT_FOUND, "no such version");
    }
    let Some(vn) = file.version(version) else {
        return err(StatusCode::NOT_FOUND, "no such version");
    };
    let full = q.get("full").map(|f| f == "1").unwrap_or(false);
    let rel = if full {
        vn.media_rel.as_str()
    } else {
        vn.stream_rel()
    };
    // resolve with fallback: a promised proxy that is not on disk (still
    // generating, cache pruned) must never black-screen a guest — serve
    // the original media instead
    let path = match contained(&root, rel).filter(|p| p.is_file()) {
        Some(p) => p,
        None if !full && vn.proxy_rel.is_some() => match contained(&root, &vn.media_rel) {
            Some(p) if p.is_file() => p,
            _ => return err(StatusCode::NOT_FOUND, "media not available"),
        },
        _ => return err(StatusCode::NOT_FOUND, "media not available"),
    };
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return err(StatusCode::NOT_FOUND, "media not available"),
    };
    if !meta.is_file() {
        return err(StatusCode::NOT_FOUND, "media not available");
    }
    let len = meta.len();
    let ct = content_type(&path);
    if len == 0 {
        return err(StatusCode::NOT_FOUND, "media empty");
    }

    let mut rh = HeaderMap::new();
    rh.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    rh.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    rh.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );

    // Range request → 206 with a capped window; plain request → 200
    // streaming the whole file.
    if let Some((start, end)) = parse_range(headers.get(header::RANGE), len) {
        let end = end.min(start.saturating_add(CHUNK - 1));
        let n = end - start + 1;
        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => return err(StatusCode::NOT_FOUND, "media not available"),
        };
        if f.seek(SeekFrom::Start(start)).await.is_err() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
        }
        let mut buf = vec![0u8; n as usize];
        if f.read_exact(&mut buf).await.is_err() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "read failed");
        }
        let cr = format!("bytes {start}-{end}/{len}");
        let mut resp = Response::new(Body::from(buf));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&cr).unwrap_or(HeaderValue::from_static("bytes 0-0/0")),
        );
        for (k, v) in rh.iter() {
            resp.headers_mut().insert(k, v.clone());
        }
        resp
    } else {
        // stream the full file in 64 KiB reads (Body::from_stream keeps
        // memory flat regardless of file size)
        let f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => return err(StatusCode::NOT_FOUND, "media not available"),
        };
        let stream = file_stream(f);
        let mut resp = Response::new(Body::from_stream(stream));
        for (k, v) in rh.iter() {
            resp.headers_mut().insert(k, v.clone());
        }
        resp
    }
}

/// Incremental 64 KiB read stream over a tokio file (futures::stream::
/// unfold — no new external crates; futures is a workspace dep).
fn file_stream(f: tokio::fs::File) -> impl Stream<Item = std::io::Result<Bytes>> {
    futures::stream::unfold(f, |mut f| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => Some((Ok(Bytes::from(buf[..n].to_vec())), f)),
            Err(e) => Some((Err(e), f)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GuestRole, ReviewVersion};

    struct FixedProvider {
        roots: Vec<(String, PathBuf)>,
        now: std::sync::atomic::AtomicI64,
    }

    #[async_trait::async_trait]
    impl RootProvider for FixedProvider {
        async fn roots(&self) -> Vec<(String, PathBuf)> {
            self.roots.clone()
        }
        fn now_ms(&self) -> i64 {
            self.now.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn setup() -> (Portal, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let mut f = crate::model::ReviewFile {
            title: "Brand Film".into(),
            ..Default::default()
        };
        f.publish(ReviewVersion {
            number: 0,
            label: "v1".into(),
            media_rel: "cuts/v1.mp4".into(),
            proxy_rel: None,
            fps_num: 24,
            fps_den: 1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "editor".into(),
            published_at: 1,
        });
        Store::save(&root, &f).unwrap();
        let token = f.add_link(GuestRole::Commenter, "jane".into(), 0, false, 1);
        Store::save(&root, &f).unwrap();
        let provider = Arc::new(FixedProvider {
            roots: vec![("p1".into(), root.clone())],
            now: std::sync::atomic::AtomicI64::new(1_000),
        });
        (Portal::new(provider), root, token)
    }

    #[tokio::test]
    async fn session_resolves_valid_token_and_fails_closed() {
        let (p, _root, token) = setup();
        let r = session(State(p.clone()), UrlPath(token.clone())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let bad = session(State(p), UrlPath("nope".into())).await;
        assert_eq!(bad.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn comments_enforce_role_frame_and_body() {
        let (p, root, token) = setup();
        // happy path
        let body = json!({"version": 1, "frame": 10, "body": "tighten here", "author": "jane"});
        let r = comment(State(p.clone()), UrlPath(token.clone()), Some(Json(body))).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(Store::load_comments(&root, 1).unwrap().len(), 1);
        // frame beyond cut
        let bad_frame = json!({"version": 1, "frame": 1000, "body": "x", "author": "j"});
        let r2 = comment(
            State(p.clone()),
            UrlPath(token.clone()),
            Some(Json(bad_frame)),
        )
        .await;
        assert_eq!(r2.status(), StatusCode::BAD_REQUEST);
        // empty body
        let bad_body = json!({"version": 1, "frame": 5, "body": "  ", "author": "j"});
        let r3 = comment(
            State(p.clone()),
            UrlPath(token.clone()),
            Some(Json(bad_body)),
        )
        .await;
        assert_eq!(r3.status(), StatusCode::BAD_REQUEST);
        // viewer link cannot comment
        let mut f = Store::load(&root).unwrap().unwrap();
        let vt = f.add_link(GuestRole::Viewer, "v".into(), 0, false, 1);
        Store::save(&root, &f).unwrap();
        let vbody = json!({"version": 1, "frame": 5, "body": "hi", "author": "v"});
        let r4 = comment(State(p), UrlPath(vt), Some(Json(vbody))).await;
        assert_eq!(r4.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn media_serves_range_and_rejects_escapes() {
        let (p, root, token) = setup();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v1.mp4"), vec![7u8; 100_000]).unwrap();

        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_static("bytes=0-999"));
        let r = media(
            State(p.clone()),
            UrlPath((token.clone(), "1".into())),
            Query(HashMap::new()),
            h,
        )
        .await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 0-999/100000"
        );

        // plain request: 200 + full length
        let r2 = media(
            State(p.clone()),
            UrlPath((token.clone(), "1".into())),
            Query(HashMap::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r2.status(), StatusCode::OK);

        // unknown version
        let r3 = media(
            State(p.clone()),
            UrlPath((token.clone(), "9".into())),
            Query(HashMap::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r3.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn presence_heartbeats_and_session_lists() {
        let (p, _root, token) = setup();
        let body = json!({"reviewer": "jane", "version": 1, "frame": 42});
        let r = presence(State(p.clone()), UrlPath(token.clone()), Some(Json(body))).await;
        assert_eq!(r.status(), StatusCode::OK);
        let s = session(State(p), UrlPath(token)).await;
        let body = serde_json::from_slice::<serde_json::Value>(
            &axum::body::to_bytes(s.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let pres = body["presence"].as_array().unwrap();
        assert_eq!(pres.len(), 1);
        assert_eq!(pres[0]["reviewer"], "jane");
    }

    /// The guest-link leak the first dogfood run flagged: a `latest_only`
    /// link must not be able to stream, comment on, or resolve hidden
    /// older versions — the routes used `version()` directly.
    #[tokio::test]
    async fn latest_only_links_cannot_touch_hidden_versions() {
        let (p, root, _full_token) = setup();
        // publish v2, then mint a latest_only link (sees only v2)
        let mut f = Store::load(&root).unwrap().unwrap();
        f.publish(ReviewVersion {
            number: 0,
            label: "v2".into(),
            media_rel: "cuts/v2.mp4".into(),
            proxy_rel: None,
            fps_num: 24,
            fps_den: 1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "editor".into(),
            published_at: 2,
        });
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v2.mp4"), vec![7u8; 1000]).unwrap();
        let t_latest = f.add_link(GuestRole::Commenter, "client".into(), 0, true, 1);
        Store::save(&root, &f).unwrap();

        // session: only v2 visible
        let s = session(State(p.clone()), UrlPath(t_latest.clone())).await;
        let body = serde_json::from_slice::<serde_json::Value>(
            &axum::body::to_bytes(s.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["versions"].as_array().unwrap().len(), 1);
        assert_eq!(body["versions"][0]["number"], 2);

        // media for hidden v1: 404
        let r = media(
            State(p.clone()),
            UrlPath((t_latest.clone(), "1".into())),
            Query(HashMap::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        // media for visible v2: 200
        let r2 = media(
            State(p.clone()),
            UrlPath((t_latest.clone(), "2".into())),
            Query(HashMap::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r2.status(), StatusCode::OK);

        // comment on hidden v1: 404 (not FORBIDDEN — never confirm it exists)
        let body1 = json!({"version": 1, "frame": 5, "body": "x", "author": "j"});
        let r3 = comment(
            State(p.clone()),
            UrlPath(t_latest.clone()),
            Some(Json(body1)),
        )
        .await;
        assert_eq!(r3.status(), StatusCode::NOT_FOUND);
        // resolve on hidden v1: 404
        let body2 = json!({"version": 1, "id": "nope", "status": "RESOLVED"});
        let r4 = resolve(State(p), UrlPath(t_latest), Some(Json(body2))).await;
        assert_eq!(r4.status(), StatusCode::NOT_FOUND);
    }

    /// A promised proxy that is not on disk must fall back to the original
    /// media (guests see the cut, not a dead player) — the "guest link
    /// 403/black screen" dogfood bug.
    #[tokio::test]
    async fn missing_proxy_falls_back_to_full_media() {
        let (p, root, token) = setup();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v1.mp4"), vec![9u8; 50_000]).unwrap();
        // promise a proxy that does not exist
        let mut f = Store::load(&root).unwrap().unwrap();
        f.versions[0].proxy_rel = Some("proxies/v1.mp4".into());
        Store::save(&root, &f).unwrap();

        // session reports proxy not ready
        let s = session(State(p.clone()), UrlPath(token.clone())).await;
        let body = serde_json::from_slice::<serde_json::Value>(
            &axum::body::to_bytes(s.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["versions"][0]["has_proxy"], true);
        assert_eq!(body["versions"][0]["proxy_ready"], false);

        // media still serves (the original), with the original's bytes
        let r = media(
            State(p),
            UrlPath((token, "1".into())),
            Query(HashMap::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.len(), 50_000);
    }
}
