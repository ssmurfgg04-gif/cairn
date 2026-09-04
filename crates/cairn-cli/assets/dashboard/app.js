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

async function postJSON(url, body) {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
  return res.json();
}

/* ---------- selected project (shared by snapshot/pin/recall controls) ---------- */

let PROJECTS = [];

function selectedProject(selectId) {
  const el = $(selectId);
  if (el && el.value) return el.value;
  return PROJECTS.length > 0 ? PROJECTS[0].project_id : "";
}

function fillProjectSelects() {
  for (const id of ["snapshot-project", "pin-project", "recall-project"]) {
    const el = $(id);
    if (!el) continue;
    const prev = el.value;
    el.innerHTML = "";
    for (const p of PROJECTS) {
      const opt = document.createElement("option");
      opt.value = p.project_id;
      opt.textContent = p.project_id;
      el.appendChild(opt);
    }
    if (prev && PROJECTS.some((p) => p.project_id === prev)) el.value = prev;
  }
}

/* ---------- status header + overview ---------- */

async function refreshStatus() {
  try {
    const s = await getJSON("/api/v1/status");
    $("daemon-version").textContent = `v${s.version}`;
    $("daemon-proto").textContent = `v${s.proto}`;
    $("daemon-uptime").textContent = fmtUptime(s.uptime_ms);

    const summary = s.summary || {};
    // header title is owned by refreshProjects (real per-project roots)

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

/* ---------- projects (WO6-UI: real ctl parity) ---------- */

function renderProjects(projects) {
  PROJECTS = projects || [];
  fillProjectSelects();
  const body = $("project-body");
  body.innerHTML = "";
  if (!PROJECTS.length) {
    body.innerHTML =
      '<tr><td colspan="7" class="empty">No attached projects — attach a root above or via `cairn attach`.</td></tr>';
    return;
  }
  for (const p of PROJECTS) {
    const tr = document.createElement("tr");
    const stateTag =
      p.state === "error" ? "bad" : p.state === "syncing" ? "info" : "ok";
    const err = p.last_error
      ? `<span class="tag bad">error</span> ${p.last_error}`
      : "—";
    tr.innerHTML =
      `<td class="mono">${p.project_id}</td>` +
      `<td>${p.root_path ?? ""}</td>` +
      `<td><span class="tag ${stateTag}">${p.state ?? "?"}</span></td>` +
      `<td>${p.files_synced ?? 0}</td>` +
      `<td>${p.pending_outbox ?? 0}</td>` +
      `<td>${err}</td>` +
      `<td><button type="button" class="btn btn-ghost" data-detach="${p.project_id}">detach</button></td>`;
    tr.querySelector("[data-detach]").addEventListener("click", async (ev) => {
      if (!confirm(`Detach ${ev.currentTarget.dataset.detach}? Local files stay.`)) return;
      await postJSON("/api/v1/detach", { project_id: ev.currentTarget.dataset.detach });
      refreshAll();
    });
    body.appendChild(tr);
  }
}

async function refreshProjects() {
  try {
    const r = await getJSON("/api/v1/projects");
    renderProjects(r.projects);
    const attached = PROJECTS.length;
    $("project-name").textContent =
      attached === 0
        ? "no roots attached"
        : attached === 1
          ? PROJECTS[0].root_path || PROJECTS[0].project_id
          : `${attached} roots attached`;
  } catch { /* covered by status chip */ }
}

/* ---------- snapshots (list + create + restore) ---------- */

function renderSnapshots(snapshots) {
  const body = $("snapshot-body");
  body.innerHTML = "";
  if (!snapshots || snapshots.length === 0) {
    body.innerHTML =
      '<tr><td colspan="5" class="empty">No snapshots yet — create one after the first sync.</td></tr>';
    return;
  }
  for (const s of snapshots.slice(0, 10)) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td class="mono">${(s.commit_hash || "").slice(0, 12)}</td>` +
      `<td>${s.label || ""}</td>` +
      `<td>${s.snapshot_seq ?? "—"}</td>` +
      `<td>${s.author || ""}</td>` +
      `<td><button type="button" class="btn btn-ghost" data-restore="${s.commit_hash}">restore</button></td>`;
    tr.querySelector("[data-restore]").addEventListener("click", async (ev) => {
      const project = selectedProject("snapshot-project");
      if (!project) return alert("attach a project first");
      if (!confirm("Restore this snapshot into the workspace?")) return;
      const r = await postJSON("/api/v1/snapshots/restore", {
        project_id: project,
        commit_hash: ev.currentTarget.dataset.restore,
      });
      if (r.ok) alert(`Restored ${r.restored_files} files (${fmtBytes(r.bytes)})`);
      else alert(`Restore failed: ${r.error}`);
      refreshAll();
    });
    body.appendChild(tr);
  }
}

async function refreshSnapshots() {
  const project = selectedProject("snapshot-project");
  if (!project) return;
  try {
    const r = await getJSON(`/api/v1/snapshots?project=${encodeURIComponent(project)}`);
    renderSnapshots(r.ok ? r.snapshots : []);
  } catch { /* server may be down; empty stays honest */ }
}

/* ---------- pins ---------- */

function renderPins(pins) {
  const body = $("pin-body");
  body.innerHTML = "";
  if (!pins || pins.length === 0) {
    body.innerHTML = '<tr><td colspan="4" class="empty">No pins on this machine.</td></tr>';
    return;
  }
  for (const p of pins) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td class="mono">${p.path}</td>` +
      `<td>${fmtBytes(p.size)}</td>` +
      `<td><span class="tag ok">${p.state || "pinned"}</span></td>` +
      `<td><button type="button" class="btn btn-ghost" data-unpin="${p.path}">unpin</button></td>`;
    tr.querySelector("[data-unpin]").addEventListener("click", async (ev) => {
      const project = selectedProject("pin-project");
      if (!project) return;
      await postJSON("/api/v1/pins/unpin", { project_id: project, path: ev.currentTarget.dataset.unpin });
      refreshPins();
    });
    body.appendChild(tr);
  }
}

async function refreshPins() {
  const project = selectedProject("pin-project");
  if (!project) return;
  try {
    const r = await getJSON(`/api/v1/pins?project=${encodeURIComponent(project)}`);
    renderPins(r.ok ? r.pins : []);
  } catch { /* covered */ }
}

/* ---------- recall jobs (progress) ---------- */

const RECALL_JOBS = new Map();

function renderRecallJobs() {
  const box = $("recall-jobs");
  if (RECALL_JOBS.size === 0) {
    box.innerHTML = '<p class="note">No recall jobs yet.</p>';
    return;
  }
  box.innerHTML = "";
  for (const [id, j] of RECALL_JOBS.entries()) {
    const div = document.createElement("div");
    div.className = "recall-job";
    const tag = j.state === "failed" ? "bad" : j.state === "completed" ? "ok" : "info";
    div.innerHTML =
      `<div class="recall-head"><span class="mono">${id.slice(0, 8)}</span>` +
      `<span class="tag ${tag}">${j.state}</span></div>` +
      `<div class="meter"><div class="meter-fill" style="width:${Math.max(4, Math.round((j.progress || 0) * 100))}%"></div></div>`;
    box.appendChild(div);
  }
}

async function pollRecallJobs() {
  for (const [id, j] of RECALL_JOBS.entries()) {
    if (j.state === "completed" || j.state === "failed") continue;
    try {
      const r = await getJSON(`/api/v1/recall/${encodeURIComponent(id)}`);
      if (r.ok) { RECALL_JOBS.set(id, r); }
    } catch { /* keep last state */ }
  }
  renderRecallJobs();
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

function renderReview(rows) {
  const body = document.getElementById("review-body");
  if (!body) return;
  body.textContent = "";
  const live = rows.filter((r) => r.title !== null && r.title !== undefined);
  if (!live.length) {
    body.innerHTML =
      '<div class="muted-note">no review sessions — publish one: <code>cairn review publish --media cuts/v1.mp4 --frames N</code></div>';
    return;
  }
  for (const r of live) {
    const card = document.createElement("div");
    card.className = "review-project";
    const head = document.createElement("div");
    head.className = "review-head";
    head.innerHTML =
      '<span class="rv-title"></span> <span class="rv-links"></span>';
    head.querySelector(".rv-title").textContent = r.title;
    head.querySelector(".rv-links").textContent =
      r.live_links + " live link" + (r.live_links === 1 ? "" : "s") +
      (r.expired_links ? " · " + r.expired_links + " expired" : "");
    card.appendChild(head);
    const versions = document.createElement("div");
    versions.className = "rv-versions";
    for (const v of r.versions.slice(-4).reverse()) {
      const row = document.createElement("div");
      row.className = "rv-row";
      row.textContent =
        "v" + v.number + "  " + v.label + "  ·  " + v.duration +
        "  ·  " + v.frames + "fr  ·  by " + v.published_by +
        (v.has_proxy ? "  ·  proxy" : "");
      versions.appendChild(row);
    }
    card.appendChild(versions);
    const notes = document.createElement("div");
    notes.className = "rv-notes";
    notes.textContent = r.open_notes + " note" + (r.open_notes === 1 ? "" : "s");
    card.appendChild(notes);
    body.appendChild(card);
  }
}

async function refreshReview() {
  try {
    const r = await getJSON("/api/v1/review");
    renderReview(r.review || []);
  } catch {
    /* dashboard keeps polling */
  }
}

async function refreshAll() {
  await refreshStatus();
  await refreshProjects();
  try {
    const feed = await getJSON("/api/v1/feed");
    renderActivity(feed.activity);
    renderLeases(feed.leases);
  } catch { /* covered by status chip */ }
  await refreshSnapshots();
  await refreshPins();
  await pollRecallJobs();
  try {
    const f = await getJSON("/api/v1/flags");
    renderFlags(f.flags);
  } catch { /* covered */ }
}

/* ---------- action buttons ---------- */

$("btn-attach").addEventListener("click", async () => {
  const root = $("attach-root").value.trim();
  const project = $("attach-project").value.trim();
  if (!root) return alert("root path required");
  const r = await postJSON("/api/v1/attach", { root_path: root, project_id: project });
  if (!r.ok) alert(`attach failed: ${r.error}`);
  else { $("attach-root").value = ""; $("attach-project").value = ""; }
  refreshAll();
});

$("btn-snapshot").addEventListener("click", async () => {
  const project = selectedProject("snapshot-project");
  if (!project) return alert("attach a project first");
  const r = await postJSON("/api/v1/snapshots", {
    project_id: project,
    label: $("snapshot-label").value.trim(),
  });
  if (r.ok) { $("snapshot-label").value = ""; refreshSnapshots(); }
  else alert(`snapshot failed: ${r.error}`);
});

$("btn-snapshots-refresh").addEventListener("click", refreshSnapshots);

$("btn-pin").addEventListener("click", async () => {
  const project = selectedProject("pin-project");
  const path = $("pin-path").value.trim();
  if (!project || !path) return alert("project and path required");
  const r = await postJSON("/api/v1/pins", { project_id: project, path });
  if (r.ok) { $("pin-path").value = ""; refreshPins(); }
  else alert(`pin failed: ${r.error}`);
});

$("btn-recall").addEventListener("click", async () => {
  const project = selectedProject("recall-project");
  if (!project) return alert("attach a project first");
  const r = await postJSON("/api/v1/recall", {
    project_id: project,
    path: $("recall-path").value.trim(),
  });
  if (r.ok) { RECALL_JOBS.set(r.job_id, { state: "running", progress: 0 }); renderRecallJobs(); }
  else alert(`recall failed: ${r.error}`);
});

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
refreshReview();
setInterval(refreshAll, 2000);
setInterval(refreshReview, 5000);
setInterval(refreshOnce, 15000);
