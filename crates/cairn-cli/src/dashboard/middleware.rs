//! Cross-cutting dashboard middleware (review #96): the loopback/token
//! gate, one-time download tickets (#61), idempotent mutations (#42),
//! local-only usage counts (#95) and request ids (#94). Handlers and the
//! route table stay in `mod.rs`: one place to audit every endpoint
//! against the gate.

use super::*;

/// True for the loopback Host forms the dashboard accepts: `127.0.0.1`
/// (any 127.x), `localhost`, `[::1]` — any port, case-insensitive.
pub(super) fn is_loopback_host(hostport: &str) -> bool {
    let h = hostport.trim();
    if h.eq_ignore_ascii_case("localhost") || h == "::1" {
        return true;
    }
    // strip :port (last colon; IPv6 arrives bracketed from real clients)
    let host = match h.rsplit_once(':') {
        Some((hp, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => hp,
        _ => h,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "::1" || host.eq_ignore_ascii_case("localhost") || host.starts_with("127.")
}

/// `scheme://loopback[:port]` origins only (the scheme is whatever the
/// browser says; the AUTHORITY decides — evil.com rebinding to 127.0.0.1
/// still arrives with Host: evil.com and Origin: https://evil.com).
pub(super) fn is_loopback_origin(origin: &str) -> bool {
    let Some((_scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    is_loopback_host(authority)
}

/// Length-independent string compare (the token is a 32-hex uuid; the
/// constant-time posture costs 4 lines and never lies in a review).
pub(super) fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The P0 gate, in order:
/// 1. HOST — DNS-rebinding protection. A loopback bind is not a trust
///    boundary: an attacker page can resolve evil.com to 127.0.0.1 and the
///    browser sends `Host: evil.com`. Anything but a loopback Host dies.
/// 2. ORIGIN — browsers attach Origin to every fetch/XHR and all
///    cross-site form POSTs. Present-but-not-loopback (`https://evil.com`,
///    `null`) dies. Absent means a non-browser client (curl, the tray),
///    which then still faces the token gate.
/// 3. TOKEN — every `/api` call must carry the per-launch token. This is
///    what stops a plain `<img>`/no-cors drive-by from the whole big
///    internet pointing at the user's own 127.0.0.1.
///
/// The token rides downstream as an Extension so the index handler can
/// inject it into the page without a second source of truth.
pub(super) async fn security_gate(
    axum::extract::State(DashToken(expected)): axum::extract::State<DashToken>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header;
    let deny = |msg: &'static str| {
        (
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": msg})),
        )
            .into_response()
    };

    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(is_loopback_host)
        .unwrap_or(false);
    if !host_ok {
        return deny("forbidden: loopback Host required");
    }

    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = origin.to_str().map(is_loopback_origin).unwrap_or(false);
        if !ok {
            return deny("forbidden: cross-origin request");
        }
    }

    if req.uri().path().starts_with("/api/") {
        // #61: the token travels ONLY in the x-cairn-token header (the live
        // stream reads with fetch + headers now). The one URL-borne
        // credential left is a download ticket: single use, 60s, bound to
        // one project+path, minted by a token-authenticated POST.
        if req.uri().path() == "/api/v1/file/download" && download_ticket_ok(req.uri()) {
            return next.run(req).await;
        }
        let provided = req
            .headers()
            .get("x-cairn-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_default();
        if !token_eq(&provided, &expected) {
            // #57: a distinct machine code so an open page whose daemon
            // restarted (new per-launch token) can reload itself instead of
            // failing every call with a raw 403.
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "code": "STALE_TOKEN",
                    "error": "forbidden: missing or bad dashboard token",
                })),
            )
                .into_response();
        }
    }

    req.extensions_mut().insert(DashToken(expected));
    next.run(req).await
}

/// #61 one-time download tickets: ticket -> (project, path, expiry).
pub(super) type TicketMap = std::collections::HashMap<String, (String, String, std::time::Instant)>;
pub(super) static DOWNLOAD_TICKETS: std::sync::Mutex<Option<TicketMap>> =
    std::sync::Mutex::new(None);
pub(super) const TICKET_TTL: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) fn mint_download_ticket(project: &str, path: &str) -> String {
    let ticket = uuid::Uuid::new_v4().simple().to_string();
    let mut g = DOWNLOAD_TICKETS.lock().unwrap_or_else(|p| p.into_inner());
    let m = g.get_or_insert_with(Default::default);
    m.retain(|_, (_, _, at)| at.elapsed() < TICKET_TTL);
    m.insert(
        ticket.clone(),
        (
            project.to_owned(),
            path.to_owned(),
            std::time::Instant::now(),
        ),
    );
    ticket
}

/// Consume a ticket: valid only once, only within TTL, only for the exact
/// project+path it was minted for.
pub(super) fn download_ticket_ok(uri: &axum::http::Uri) -> bool {
    let Ok(axum::extract::Query(query)) =
        axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(uri)
    else {
        return false;
    };
    let (Some(ticket), Some(project), Some(path)) =
        (query.get("ticket"), query.get("project"), query.get("path"))
    else {
        return false;
    };
    let mut guard = DOWNLOAD_TICKETS.lock().unwrap_or_else(|p| p.into_inner());
    let Some(tickets) = guard.as_mut() else {
        return false;
    };
    match tickets.remove(ticket) {
        Some((minted_project, minted_path, at)) => {
            at.elapsed() < TICKET_TTL && &minted_project == project && &minted_path == path
        }
        None => false,
    }
}

pub(super) async fn file_ticket(body: Option<Json<serde_json::Value>>) -> Json<serde_json::Value> {
    let Some(Json(v)) = body else {
        return Json(json!({"ok": false, "error": "body required: {project, path}"}));
    };
    let (project, path) = (
        v["project"].as_str().unwrap_or_default(),
        v["path"].as_str().unwrap_or_default(),
    );
    if project.is_empty() || path.is_empty() {
        return Json(json!({"ok": false, "error": "project and path are required"}));
    }
    Json(
        json!({"ok": true, "ticket": mint_download_ticket(project, path), "expires_in_s": TICKET_TTL.as_secs()}),
    )
}

/// #42 idempotent mutations. A POST carrying `idempotency-key` (the
/// console sends a fresh one per user action) is applied at most once:
/// a repeat of a COMPLETED request gets the stored answer back
/// (`x-idempotent-replay: 1`), and a repeat that arrives while the first
/// is still running is refused as busy (409) instead of running twice.
/// Keys live 10 minutes, in memory; only small JSON answers are stored.
pub(super) async fn idempotency(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, Method, StatusCode};
    use axum::response::IntoResponse;
    const TTL: std::time::Duration = std::time::Duration::from_secs(600);
    enum Slot {
        Running,
        Done(StatusCode, axum::body::Bytes),
    }
    static SEEN: std::sync::Mutex<
        Option<std::collections::HashMap<String, (std::time::Instant, Slot)>>,
    > = std::sync::Mutex::new(None);

    let key = (req.method() == Method::POST)
        .then(|| req.headers().get("idempotency-key"))
        .flatten()
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 64)
        .map(|k| format!("{}|{k}", req.uri().path()));
    let Some(key) = key else {
        return next.run(req).await;
    };
    {
        let mut g = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        let map = g.get_or_insert_with(Default::default);
        map.retain(|_, (at, _)| at.elapsed() < TTL);
        match map.get(&key) {
            Some((_, Slot::Done(code, body))) => {
                let mut res = (*code, body.clone()).into_response();
                let h = res.headers_mut();
                h.insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                h.insert("x-idempotent-replay", HeaderValue::from_static("1"));
                return res;
            }
            Some((_, Slot::Running)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(
                        json!({"ok": false, "code": "IN_PROGRESS", "error": "already in progress"}),
                    ),
                )
                    .into_response();
            }
            None => {
                map.insert(key.clone(), (std::time::Instant::now(), Slot::Running));
            }
        }
    }
    let res = next.run(req).await;
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, 256 * 1024)
        .await
        .unwrap_or_default();
    {
        let mut g = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(map) = g.as_mut() {
            if parts.status.is_server_error() {
                map.remove(&key); // a crash-y attempt may be retried
            } else {
                map.insert(
                    key,
                    (
                        std::time::Instant::now(),
                        Slot::Done(parts.status, bytes.clone()),
                    ),
                );
            }
        }
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// #95 usage counts. Policy (decided, see docs/REVIEW-BACKLOG.md #95):
/// LOCAL ONLY. Count which console actions are used (`POST` route, ids
/// stripped) and how often, plus first/last day. Never file names, paths,
/// project/device ids, review content or timings. Nothing is sent
/// anywhere; the counts live in `<home>/usage.json`, are visible in
/// Settings, and join a diagnostics file only when the user ticks
/// "Include usage counts".
pub(super) async fn usage_count(
    State(state): State<Arc<DaemonState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let feature = (req.method() == axum::http::Method::POST)
        .then(|| crate::usage::feature_of(req.uri().path()))
        .flatten();
    let res = next.run(req).await;
    if let Some(f) = feature {
        if res.status().is_success() {
            let home = state.home.clone();
            tokio::task::spawn_blocking(move || crate::usage::bump(&home, &f));
        }
    }
    res
}

pub(super) async fn usage(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(json!({"ok": true, "usage": crate::usage::load(&state.home), "sent_anywhere": false}))
}

/// #94 correlation IDs: every dashboard response carries a short
/// `x-cairn-request-id`, and every failed `/api` call is logged with it, so
/// "it said ref 3f9a2c1b" in a support message maps to one daemon.log line.
/// A client-supplied id (hex, <= 32 chars) is honored so retries keep one id.
pub(super) async fn request_id(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let rid = req
        .headers()
        .get("x-cairn-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_owned());
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let mut res = next.run(req).await;
    let status = res.status();
    let is_api = path.starts_with("/api/");
    if is_api && (status.is_client_error() || status.is_server_error()) {
        tracing::warn!(request_id = %rid, %method, %path, status = status.as_u16(), "dashboard api error");
    }
    // Service failures mostly answer 200 + {"ok":false}. For small JSON
    // bodies (never the SSE stream or downloads: those are not JSON or have
    // no exact size), stamp the id into the error body and log it.
    let small_json = is_api
        && res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"))
        && axum::body::HttpBody::size_hint(res.body())
            .exact()
            .is_some_and(|n| n <= 64 * 1024);
    if small_json {
        let (mut parts, body) = res.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .unwrap_or_default();
        let mut out = bytes.clone();
        if let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice(&bytes) {
            if obj.get("ok") == Some(&serde_json::Value::Bool(false)) {
                let err = obj
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_owned();
                tracing::warn!(request_id = %rid, %method, %path, error = %err, "dashboard action failed");
                obj.insert("request_id".into(), serde_json::Value::String(rid.clone()));
                if let Ok(v) = serde_json::to_vec(&obj) {
                    out = v.into();
                }
            }
        }
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        res = axum::response::Response::from_parts(parts, axum::body::Body::from(out));
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&rid) {
        res.headers_mut().insert("x-cairn-request-id", v);
    }
    res
}

#[cfg(test)]
mod security_gate_tests {
    use super::is_loopback_host;
    use super::is_loopback_origin;
    use super::token_eq;

    /// Regression (live merge-offer dogfood, 2026-10-08): the hardening
    /// round's header array dropped `content-type: text/html`, and with
    /// `nosniff` every real browser then rendered the page as PLAIN TEXT —
    /// the dashboard looked broken while every mock-based UI test stayed
    /// green (the mock serves its own headers). The page MUST be text/html.
    #[tokio::test]
    async fn served_page_is_html_not_plain_text() {
        let resp = super::index(axum::Extension(super::DashToken(std::sync::Arc::new(
            "tok".into(),
        ))))
        .await;
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/html"),
            "dashboard page must be text/html (nosniff renders anything else as source): {ct}"
        );
    }

    #[test]
    fn host_gate_accepts_only_loopback_forms() {
        for ok in [
            "127.0.0.1:17778",
            "127.0.0.1",
            "127.9.9.9:80",
            "localhost",
            "localhost:17778",
            "LOCALHOST:17778",
            "[::1]:17778",
            "::1",
        ] {
            assert!(is_loopback_host(ok), "should accept {ok}");
        }
        for bad in [
            "evil.com",
            "evil.com:17778",
            "192.168.1.10:17778",
            "10.0.0.5",
            "[2001:db8::1]:17778",
            "metadata.google.internal",
            "",
        ] {
            assert!(!is_loopback_host(bad), "should reject {bad}");
        }
    }

    #[test]
    fn origin_gate_accepts_only_loopback_authorities() {
        for ok in [
            "http://127.0.0.1:17778",
            "http://localhost:17778",
            "https://localhost",
            "http://[::1]:17778",
        ] {
            assert!(is_loopback_origin(ok), "should accept {ok}");
        }
        for bad in [
            "null",
            "https://evil.com",
            "http://evil.com:80",
            "evil.com",
            "http://192.168.1.10:17778",
            "",
        ] {
            assert!(!is_loopback_origin(bad), "should reject {bad}");
        }
    }

    #[test]
    fn token_compare_is_exact() {
        let t = "0123456789abcdef0123456789abcdef";
        assert!(token_eq(t, t));
        assert!(token_eq("", ""));
        assert!(!token_eq(t, "0123456789abcdef0123456789abcdeX"));
        assert!(!token_eq(t, "short"));
        assert!(!token_eq(t, ""));
    }
}

#[cfg(test)]
mod request_id_tests {
    use axum::{body::Body, http::Request, routing::get, Json, Router};
    use serde_json::json;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route(
                "/api/v1/ok",
                get(|| async { Json(json!({"ok": true, "n": 1})) }),
            )
            .route(
                "/api/v1/fail",
                get(|| async { Json(json!({"ok": false, "error": "permission denied"})) }),
            )
            .route("/", get(|| async { "page" }))
            .layer(axum::middleware::from_fn(super::request_id))
    }

    async fn call(uri: &str, rid: Option<&str>) -> (String, serde_json::Value) {
        let mut req = Request::builder().uri(uri);
        if let Some(r) = rid {
            req = req.header("x-cairn-request-id", r);
        }
        let res = app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let id = res.headers()["x-cairn-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            id,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn every_response_carries_an_id_and_failures_embed_it() {
        let (id, body) = call("/api/v1/fail", None).await;
        assert_eq!(id.len(), 12);
        assert_eq!(body["request_id"], id, "ok:false bodies carry the same ref");
        assert_eq!(
            body["error"], "permission denied",
            "original error preserved"
        );

        let (id, body) = call("/api/v1/ok", None).await;
        assert!(!id.is_empty());
        assert!(
            body.get("request_id").is_none(),
            "success bodies are untouched"
        );
        assert_eq!(body["n"], 1);
    }

    #[tokio::test]
    async fn client_ids_are_honored_only_when_safe() {
        let (id, _) = call("/api/v1/ok", Some("abc123")).await;
        assert_eq!(id, "abc123");
        let (id, _) = call("/api/v1/ok", Some("not hex; <script>")).await;
        assert_ne!(id, "not hex; <script>");
        assert_eq!(id.len(), 12);
    }

    #[tokio::test]
    async fn idempotency_key_applies_a_post_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let app = || {
            Router::new()
                .route(
                    "/api/v1/do",
                    axum::routing::post(|| async {
                        let n = HITS.fetch_add(1, Ordering::SeqCst) + 1;
                        Json(json!({"ok": true, "n": n}))
                    }),
                )
                .layer(axum::middleware::from_fn(super::idempotency))
        };
        let post = |k: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/do")
                .header("idempotency-key", k)
                .body(Body::empty())
                .unwrap()
        };
        let a = app().oneshot(post("k-1")).await.unwrap();
        let b = app().oneshot(post("k-1")).await.unwrap();
        assert_eq!(b.headers()["x-idempotent-replay"], "1");
        let body = |r: axum::response::Response| async move {
            serde_json::from_slice::<serde_json::Value>(
                &axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            body(a).await["n"],
            body(b).await["n"],
            "replay returns the first answer"
        );
        assert_eq!(HITS.load(Ordering::SeqCst), 1, "the handler ran once");
        let c = app().oneshot(post("k-2")).await.unwrap();
        assert!(c.headers().get("x-idempotent-replay").is_none());
        assert_eq!(HITS.load(Ordering::SeqCst), 2, "a new key is a new action");
    }

    #[test]
    fn download_tickets_are_single_use_and_bound_to_one_file() {
        let t = super::mint_download_ticket("p1", "cuts/a b.mov");
        let uri = |tk: &str, path: &str| -> axum::http::Uri {
            format!(
                "/api/v1/file/download?project=p1&path={}&ticket={tk}",
                path.replace(' ', "%20")
            )
            .parse()
            .unwrap()
        };
        assert!(
            !super::download_ticket_ok(&uri(&t, "cuts/other.mov")),
            "wrong file is refused"
        );
        // a refused attempt consumed it: tickets never get a second try
        assert!(!super::download_ticket_ok(&uri(&t, "cuts/a b.mov")));
        let t2 = super::mint_download_ticket("p1", "cuts/a b.mov");
        assert!(
            super::download_ticket_ok(&uri(&t2, "cuts/a b.mov")),
            "exact file, first use"
        );
        assert!(
            !super::download_ticket_ok(&uri(&t2, "cuts/a b.mov")),
            "second use is refused"
        );
        assert!(!super::download_ticket_ok(&uri("made-up", "cuts/a b.mov")));
    }
}
