// mock-daemon.mjs - Task 2-b dashboard UI smoke harness (dev-only, no Rust
// build needed). Serves the REAL dashboard assets (index.html/app.js/app.css
// - they are include_str!'d into the daemon binary, so a stale build would
// serve stale UI) with the %%CAIRN_TOKEN%% placeholder filled, plus
// contract-shaped stubs for every /api/v1 endpoint the console calls. The
// shapes mirror crates/cairn-cli/src/dashboard.rs + the frozen contract in
// the worklog (proxy generate 200/409, review link ttl_hours, restore
// checkpoint_version, state-records records).
//
// Usage:   node scripts/mock-daemon.mjs [port]   (default 17890)
// Verify:  GET /api/v1/debug returns what the UI POSTed (link bodies,
//          proxy bodies, merge-offer bodies, detach bodies).
import http from "node:http";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "crates/cairn-cli/assets/dashboard");
const PORT = Number(process.argv[2]) || 17890;
const TOKEN = "mock-token-123";

const html = readFileSync(join(ROOT, "index.html"), "utf8").replaceAll("%%CAIRN_TOKEN%%", TOKEN);
const css = readFileSync(join(ROOT, "app.css"), "utf8");
const js = readFileSync(join(ROOT, "app.js"), "utf8");

const PROJECT = { project_id: "projA", display_name: "Seaside Doc", state: "ok", root_path: "/tmp/seaside" };

const FILES = [
  { path: "seq/master.braw", size: 48318382080, state: "synced", pinned: true, placeholder: false, proxy_state: "ready" },
  { path: "seq/proxy.mov", size: 1048576, state: "synced", pinned: false, placeholder: false, proxy_state: "stale" },
  { path: "seq/timeline.prproj", size: 20480, state: "syncing", pinned: false, placeholder: false },
  { path: "scene.prproj.xml", size: 4096, state: "conflict", pinned: false, placeholder: false, merge_available: true },
  { path: "notes.txt", size: 0, state: "synced", pinned: false, placeholder: true },
  { path: "seq/edl.txt", size: 512, state: "synced", pinned: false, placeholder: false },
];

const JSON_H = { "Content-Type": "application/json", "Cache-Control": "no-store" };

const state = {
  pinned: new Set(["seq/master.braw"]),
  recallJobs: 0,
  // what the UI actually POSTed, per endpoint (the smoke asserts on these)
  calls: { reviewLink: [], proxyGenerate: [], mergeOffer: [], detach: [], restore: [] },
};

function filesPayload() {
  return {
    ok: true,
    files: FILES.map((f) => ({ ...f, pinned: state.pinned.has(f.path) })),
    summary: {
      files: FILES.length,
      synced: FILES.filter((f) => f.state === "synced").length,
      syncing: FILES.filter((f) => f.state === "syncing").length,
      conflict: FILES.filter((f) => f.state === "conflict").length,
      pinned: state.pinned.size,
      merge_offers: 1,
    },
  };
}

function readBody(req, then) {
  let b = "";
  req.on("data", (c) => (b += c));
  req.on("end", () => then(JSON.parse(b || "{}")));
}

const server = http.createServer((req, res) => {
  const u = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const p = u.pathname;
  const auth = req.headers["x-cairn-token"] === TOKEN || u.searchParams.get("t") === TOKEN;
  if (p !== "/" && !p.startsWith("/assets/") && !auth) { res.writeHead(403, JSON_H); res.end(JSON.stringify({ ok: false, error: "forbidden: missing or bad dashboard token" })); return; }
  const body = (v) => { res.writeHead(200, JSON_H); res.end(JSON.stringify(typeof v === "function" ? v() : v)); };

  if (p === "/") { res.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" }); res.end(html); return; }
  if (p === "/assets/app.css") { res.writeHead(200, { "Content-Type": "text/css; charset=utf-8", "Cache-Control": "no-store" }); res.end(css); return; }
  if (p === "/assets/app.js") { res.writeHead(200, { "Content-Type": "text/javascript; charset=utf-8", "Cache-Control": "no-store" }); res.end(js); return; }

  switch (p) {
    case "/api/v1/status":
      return body({ version: "0.9.7", proto: "1", uptime_ms: 3600e3 + 60e3, summary: { healthy: true, outbox_pending: 0, journal_cursor: 42, conflicts: 1, files: FILES.length, hydration_first_byte_ms: 12 } });
    case "/api/v1/projects":
      return body({ ok: true, projects: [{ ...PROJECT, files_synced: 4, pending_outbox: 0 }] });
    case "/api/v1/files":
      return body(filesPayload());
    case "/api/v1/feed":
      return body({
        activity: [
          { kind: "pinned", path: "seq/master.braw", ts: Date.now() - 60e3 },
          { kind: "upsert", state: "synced", path: "seq/proxy.mov", ts: Date.now() - 300e3 },
          { kind: "lease", path: "seq/timeline.prproj", ts: Date.now() - 900e3 },
        ],
        leases: [{ path: "seq/timeline.prproj", expires_at: Date.now() + 30e3, token: "lease-abc" }],
      });
    case "/api/v1/team":
      return body({
        join_code: "JOIN-ABCD-1234", signal: "sig.example:443", swarm: "swarm-xyz",
        projects: [{
          join_code: "JOIN-ABCD-1234", signal: "sig.example:443",
          my_role: "Owner", my_device: "workstation-a",
          members: [
            { name: "Alex", device_id: "workstation-a", role: "Owner", is_me: true },
            { name: "Sam", device_id: "laptop-b", role: "Editor", is_me: false },
          ],
          audit: [{ allowed: true, action: "attach", device: "workstation-a", role: "Owner", ts_ms: Date.now() - 60e3 }],
        }],
      });
    case "/api/v1/state-records": {
      // ADR-0031 Phase 1 read surface (views.rs state_records): family=member
      // rows with tombstones; the People card renders these as human news
      const family = u.searchParams.get("family") || "";
      const okFam = ["member", "audit", "review_version", "review_link", "review_comment"].includes(family);
      return body({
        ok: true, project: "projA", family,
        records: okFam && family === "member"
          ? [
              { record_id: "r1", key: "Sam (laptop-b)", ts_ms: Date.now() - 120e3, device_id: "laptop-b", tombstone: false, payload: { role: "Editor" } },
              { record_id: "r2", key: "Guest (tablet-c)", ts_ms: Date.now() - 7200e3, device_id: "tablet-c", tombstone: true, payload: {} },
            ]
          : [],
      });
    }
    case "/api/v1/storage":
      return body({
        ok: true,
        disk: { total_bytes: 500e9, free_bytes: 120e9 },
        volumes: [{ label: "store", total_bytes: 500e9, free_bytes: 120e9 }],
        blobs: { count: 1284, bytes: 483e9, pinned_count: 1 },
      });
    case "/api/v1/activity":
      return body({
        ok: true,
        days: [0, 1, 2, 3, 4, 5, 6].map((i) => ({ start_ms: Date.now() - i * 86400e3, bytes: 40e3 * (7 - i), files: 3 })),
      });
    case "/api/v1/update":
      return body({ ok: true, update_offered: false });
    case "/api/v1/doctor":
      return body({ ok: true, checks: [{ name: "store.open", latency_ms: 0.4, detail: "ok" }, { name: "server.ping", latency_ms: 3.2, detail: "ok" }] });
    case "/api/v1/flags":
      return body({ ok: true, flags: [{ name: "packing_enabled", value: "true" }, { name: "semantic_merge", value: "false" }, { name: "placeholder_driver", value: "native" }] });
    case "/api/v1/snapshots":
      return body({ ok: true, snapshots: [
        { commit_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8550", label: "before color pass", author: "Alex" },
        { commit_hash: "cafebabe00000000000000000000000000000000000000000000000000000000", label: "", author: "Sam" },
      ] });
    case "/api/v1/review":
      return body({
        ok: true,
        review: [{
          title: "Seaside Doc v2",
          versions: [{ number: 2, label: "client cut", duration: "00:02:31", frames: 3616, fps_num: 24, fps_den: 1 }],
          links: [
            { token: "tok-1234567890", note: "Client A", role: "commenter", expires_at: Date.now() + 6 * 86400e3, expired: false },
            { token: "tok-deadbeef99", note: "Client B", role: "commenter", expires_at: Date.now() + 86400e3, expired: false, revoked_remotely: true },
          ],
        }],
      });
    case "/api/v1/live/snapshot":
      return body({ enabled: true, projects: [{ project: "projA", events: [{ from: "laptop-b", editor: "Sam", frame: 900, rate: 24, action: "scrubbing" }] }] });
    case "/api/v1/search":
      return body({ ok: true, results: [{ project: "projA", path: "seq/proxy.mov", kind: "file" }] });
    case "/api/v1/review/publish":
      return body({ ok: true, version: 3 });
    case "/api/v1/debug":
      return body({ calls: state.calls, pinned: [...state.pinned] });
    case "/api/v1/pick-folder":
      return body({ ok: true, cancelled: true });
    // ---- mutating endpoints (bodies captured for the smoke) ----
    case "/api/v1/proxy/generate":
      // contract freeze: 200 {ok,proxy_rel,bytes,state} | 409 {ok:false,error:"in_progress"}
      return readBody(req, (b) => {
        state.calls.proxyGenerate.push(b);
        body({ ok: true, proxy_rel: ".cairn/proxy-cache/9a8b7c6d5e4f.mp4", bytes: 1048576, state: "ready" });
      });
    case "/api/v1/recall":
      state.recallJobs += 1;
      return body({ ok: true, job_id: `job-${state.recallJobs}` });
    case "/api/v1/review/link":
      // contract freeze: optional ttl_hours (168/720/2160 from the chooser)
      return readBody(req, (b) => {
        state.calls.reviewLink.push(b);
        body({ ok: true, token: "fresh-token-42", link: "/r/fresh-token-42" });
      });
    case "/api/v1/snapshots/restore":
      // 2-d contract: checkpoint BEFORE restore, version echoed back
      return readBody(req, (b) => {
        state.calls.restore.push(b);
        body({ ok: true, restored_files: 6, bytes: 123456789, checkpoint_version: 41, checkpoint_commit: "feedface", checkpoint_label: "safety checkpoint before restore" });
      });
    case "/api/v1/detach":
      return readBody(req, (b) => {
        state.calls.detach.push(b);
        body({ ok: true });
      });
    default:
      if (p.startsWith("/api/v1/merge/offer/")) {
        // dashboard.rs exact body keys: {project_id, path}
        return readBody(req, (b) => {
          state.calls.mergeOffer.push({ op: p.slice("/api/v1/merge/offer/".length), body: b });
          body({ ok: true });
        });
      }
      if (p.startsWith("/api/v1/recall/")) return body({ ok: true, state: "running", progress: 0.4 });
      if (p.startsWith("/api/v1/pins")) {
        return readBody(req, (b) => {
          if (b.path) { if (p.endsWith("/unpin")) state.pinned.delete(b.path); else state.pinned.add(b.path); }
          body({ ok: true });
        });
      }
      if (req.method === "POST") return body({ ok: true });
      return body({ ok: true });
  }
});

server.listen(PORT, "127.0.0.1", () => console.log(`mock daemon on http://127.0.0.1:${PORT} (token ${TOKEN})`));
