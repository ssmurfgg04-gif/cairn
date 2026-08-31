/* cairn local console — poll the loopback JSON gateway, render honestly.
   No build step, no framework, no fake data: empty states stay empty until real
   data exists (taste-skill: no placeholder content). */

"use strict";

const $ = (id) => document.getElementById(id);

function fmtBytes(n) {
  if (!Number.isFinite(n)) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function fmtUptime(ms) {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return `uptime ${h}h ${m % 60}m`;
  if (m > 0) return `uptime ${m}m ${s % 60}s`;
  return `uptime ${s}s`;
}

function setChip(el, cls, label) {
  el.className = `state-chip ${cls}`;
  el.lastElementChild.textContent = label;
}

async function getJSON(url) {
  const res = await fetch(url, { headers: { Accept: "application/json" } });
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.json();
}

/* ---------- status header + overview ---------- */

async function refreshStatus() {
  try {
    const s = await getJSON("/api/v1/status");
    $("daemon-version").textContent = `v${s.version}`;
    $("daemon-proto").textContent = `v${s.proto}`;
    $("daemon-uptime").textContent = fmtUptime(s.uptime_ms);

    const summary = s.summary || {};
    const attached = (s.projects || []).length;
    $("project-name").textContent =
      attached > 0 ? (s.projects[0].root_path || "project root") : "no roots attached";

    const healthy = summary.healthy === true;
    setChip(
      $("state-chip"),
      healthy ? "is-ok" : "is-warn",
      healthy ? "healthy" : "degraded"
    );

    $("stat-pending").textContent = summary.outbox_pending ?? 0;
    $("stat-cursor").textContent = summary.journal_cursor ?? 0;
    $("stat-files").textContent = summary.files ?? 0;
    $("stat-conflicts").textContent = summary.conflicts ?? 0;

    // I1 gauge: reported by the daemon from hydration instrumentation
    const i1 = summary.hydration_first_byte_ms;
    if (Number.isFinite(i1)) {
      $("stat-i1").textContent = `${i1.toFixed(1)} ms`;
      const pct = Math.max(4, Math.min(100, (i1 / 50) * 100));
      $("i1-meter").style.width = `${pct}%`;
      $("i1-meter").style.background =
        i1 < 50 ? "var(--green-fg)" : "var(--red-fg)";
    }
  } catch {
    setChip($("state-chip"), "is-bad", "daemon unreachable");
  }
}

/* ---------- activity ---------- */

function renderActivity(entries) {
  const body = $("activity-body");
  body.innerHTML = "";
  if (!entries || entries.length === 0) {
    body.innerHTML =
      '<tr><td colspan="4" class="empty">No entries yet — saves appear here as they are journaled.</td></tr>';
    return;
  }
  for (const e of entries.slice(-12).reverse()) {
    const tr = document.createElement("tr");
    const kind = e.kind || "upsert";
    const tag = kind === "delete" ? "bad" : kind === "rename" ? "info" : "ok";
    tr.innerHTML =
      `<td>${e.seq ?? "—"}</td>` +
      `<td>${e.path ?? ""}</td>` +
      `<td><span class="tag ${tag}">${kind}</span></td>` +
      `<td>${fmtBytes(e.size)}</td>`;
    body.appendChild(tr);
  }
}

/* ---------- leases ---------- */

function renderLeases(leases) {
  const body = $("lease-body");
  body.innerHTML = "";
  if (!leases || leases.length === 0) {
    body.innerHTML =
      '<tr><td colspan="4" class="empty">No live leases on this machine.</td></tr>';
    return;
  }
  const now = Date.now();
  for (const l of leases) {
    const remainMs = (l.expires_at ?? 0) - now;
    const live = remainMs > 0;
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td>${l.path ?? ""}</td>` +
      `<td>${l.token ?? "—"}</td>` +
      `<td>${live ? `${Math.ceil(remainMs / 1000)}s` : "expired"}</td>` +
      `<td><span class="tag ${live ? "ok" : "warn"}">${live ? "held" : "stale"}</span></td>`;
    body.appendChild(tr);
  }
}

/* ---------- snapshots ---------- */

function renderSnapshots(snapshots) {
  const body = $("snapshot-body");
  body.innerHTML = "";
  if (!snapshots || snapshots.length === 0) {
    body.innerHTML =
      '<tr><td colspan="4" class="empty">Snapshots list is served by the storage server after the first fold.</td></tr>';
    return;
  }
  for (const s of snapshots.slice(0, 10)) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td>${(s.commit_hash || "").slice(0, 12)}</td>` +
      `<td>${s.label || ""}</td>` +
      `<td>${s.snapshot_seq ?? "—"}</td>` +
      `<td>${s.author || ""}</td>`;
    body.appendChild(tr);
  }
}

/* ---------- flags ---------- */

function renderFlags(flags) {
  const grid = $("flag-grid");
  grid.innerHTML = "";
  for (const f of flags || []) {
    const on = String(f.value).toLowerCase() !== "false";
    const div = document.createElement("div");
    div.className = "flag";
    div.innerHTML =
      `<span class="flag-name">${f.name}</span>` +
      `<button type="button" data-name="${f.name}" data-next="${on ? "false" : "true"}">` +
      `${f.name === "placeholder_driver" ? f.value : on ? "enabled" : "disabled"}</button>`;
    div.querySelector("button").addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      await fetch("/api/v1/flags", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: btn.dataset.name, value: btn.dataset.next }),
      });
      refreshAll();
    });
    grid.appendChild(div);
  }
}

/* ---------- doctor ---------- */

function renderDoctor(report) {
  const box = $("doctor-body");
  box.innerHTML = "";
  for (const c of report.checks || []) {
    const div = document.createElement("div");
    div.className = "check";
    div.innerHTML =
      `<span class="check-name">${c.name}</span>` +
      `<span class="check-detail">${c.detail}</span>` +
      `<span class="check-ms">${Number(c.latency_ms).toFixed(1)} ms</span>`;
    box.appendChild(div);
  }
}

/* ---------- orchestration ---------- */

async function refreshOnce() {
  try {
    const d = await getJSON("/api/v1/doctor");
    renderDoctor(d);
  } catch { /* daemon down: status chip already reports it */ }
}

async function refreshAll() {
  await refreshStatus();
  try {
    const feed = await getJSON("/api/v1/feed");
    renderActivity(feed.activity);
    renderLeases(feed.leases);
  } catch { /* covered by status chip */ }
  try {
    const f = await getJSON("/api/v1/flags");
    renderFlags(f.flags);
  } catch { /* covered */ }
}

/* staggered card entry (taste-skill: cascade, never all at once) */
document.querySelectorAll(".card").forEach((el, i) => {
  el.style.setProperty("--i", String(i % 6));
});

/* nav active state */
document.querySelectorAll(".nav-item").forEach((a) => {
  a.addEventListener("click", () => {
    document.querySelectorAll(".nav-item").forEach((x) => x.classList.remove("is-active"));
    a.classList.add("is-active");
  });
});

refreshOnce();
refreshAll();
setInterval(refreshAll, 2000);
setInterval(refreshOnce, 15000);
