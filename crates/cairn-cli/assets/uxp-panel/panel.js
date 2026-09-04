/* cairn markers panel — the NLE-side half of the marker bridge (round 19).
 *
 * Talks to the daemon's loopback gateway (127.0.0.1:17778) — the same
 * surface the browser console uses:
 *   GET /api/v1/status   daemon + version line
 *   GET /api/v1/review   projects + review versions (labels, fps, frames)
 *   GET /api/v1/markers?project=&version=&format=fcpxml|otio|csv
 *                       the SAME body `cairn review export-markers` writes
 *
 * Zero dependencies, no build step, no secrets: the panel is a viewport.
 * The UXP-only branch (saving via the UXP file picker) is guarded so the
 * same file runs in a plain browser for development against a mock daemon.
 */
"use strict";

const API = "http://127.0.0.1:17778";
const $ = (id) => document.getElementById(id);

const state = { projects: [], project: "", version: 0, markers: [], daemon: "" };

/* ---- tiny CSV parser (RFC 4180, quoted fields, \n rows) ---- */

function parseCsv(text) {
  const rows = [];
  let row = [], field = "", inQ = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (inQ) {
      if (c === '"') {
        if (text[i + 1] === '"') { field += '"'; i++; }
        else { inQ = false; }
      } else { field += c; }
    } else if (c === '"') { inQ = true; }
    else if (c === ",") { row.push(field); field = ""; }
    else if (c === "\n") { row.push(field); rows.push(row); row = []; field = ""; }
    else if (c !== "\r") { field += c; }
  }
  if (field !== "" || row.length) { row.push(field); rows.push(row); }
  return rows;
}

/* ---- daemon calls ---- */

async function getJSON(path) {
  const res = await fetch(API + path);
  if (!res.ok) throw new Error(`${path} -> HTTP ${res.status}`);
  return res.json();
}

async function getStatus() {
  try {
    const s = await getJSON("/api/v1/status");
    state.daemon = `v${s.version}`;
    setStatus(true, `daemon ${state.daemon}`);
  } catch {
    setStatus(false, "daemon down — start it: cairn daemon");
  }
}

function setStatus(ok, text) {
  const el = $("status");
  el.classList.toggle("ok", ok);
  $("status-text").textContent = text;
}

/* ---- data + render ---- */

async function loadReview() {
  let r;
  try {
    r = await getJSON("/api/v1/review");
  } catch (e) {
    note(`review state unavailable (${e.message})`, true);
    return;
  }
  state.projects = (r.review || []).filter((p) => (p.versions || []).length > 0);
  const sel = $("project");
  sel.innerHTML = "";
  for (const p of state.projects) {
    const opt = document.createElement("option");
    opt.value = p.project_id;
    opt.textContent = p.title ? `${p.title} · ${p.project_id}` : p.project_id;
    sel.appendChild(opt);
  }
  if (!state.projects.length) {
    $("empty").hidden = false;
    $("empty").textContent = "no published review versions on this machine";
    note("publish one first: cairn review publish --media cuts/v1.mp4");
    setExportEnabled(false);
    return;
  }
  state.project = state.projects[0].project_id || "";
  fillVersions();
}

function fillVersions() {
  const p = state.projects.find((x) => x.project_id === state.project) || state.projects[0];
  const sel = $("version");
  sel.innerHTML = "";
  for (const v of p.versions || []) {
    const opt = document.createElement("option");
    opt.value = String(v.number);
    const fps = v.fps_den === 1 ? v.fps_num : (v.fps_num / v.fps_den).toFixed(3);
    opt.textContent = `v${v.number} · ${fps}fps`;
    if (v.label) opt.textContent += ` · ${v.label}`;
    sel.appendChild(opt);
  }
  const last = (p.versions || [])[p.versions.length - 1];
  state.version = last ? last.number : 0;
  sel.value = String(state.version);
  loadMarkers();
}

async function loadMarkers() {
  if (!state.project || !state.version) {
    note("pick a project and version", !state.project);
    setExportEnabled(false);
    return;
  }
  const url = `${API}/api/v1/markers?project=${encodeURIComponent(state.project)}&version=${state.version}&format=csv`;
  let text;
  try {
    const res = await fetch(url);
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(body.error || `HTTP ${res.status}`);
    }
    text = await res.text();
  } catch (e) {
    $("empty").hidden = false;
    $("empty").textContent = "markers unavailable";
    document.querySelector("table").hidden = true;
    note(`${e.message}`, true);
    setExportEnabled(false);
    return;
  }
  const rows = parseCsv(text);
  const head = rows.shift() || [];
  state.markers = rows.map((r) => ({
    frame: r[0], tc: r[1], author: r[2], status: r[3], note: r[4],
  }));
  render();
}

function render() {
  const table = document.querySelector("table");
  const tbody = $("rows");
  const empty = $("empty");
  tbody.innerHTML = "";
  if (!state.markers.length) {
    table.hidden = true;
    empty.hidden = false;
    empty.textContent = `no notes on v${state.version}`;
    setExportEnabled(false);
    return;
  }
  empty.hidden = true;
  table.hidden = false;
  for (const m of state.markers) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td class="frame">${esc(m.frame)}</td>` +
      `<td class="tc">${esc(m.tc)}</td>` +
      `<td class="author">${esc(m.author)}</td>` +
      `<td class="status-c ${esc(m.status)}">${esc(m.status)}</td>` +
      `<td class="note">${esc(m.note)}</td>`;
    tbody.appendChild(tr);
  }
  note(`${state.markers.length} note(s) · true-rate frames — import lands exactly`);
  setExportEnabled(true);
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

function setExportEnabled(on) {
  for (const id of ["export-fcpxml", "export-otio", "export-csv"]) {
    $(id).disabled = !on;
  }
}

function note(text, isErr) {
  const el = $("note");
  el.textContent = text;
  el.classList.toggle("err", !!isErr);
}

/* ---- export + save ---- */

async function exportAs(format) {
  const url = `${API}/api/v1/markers?project=${encodeURIComponent(state.project)}&version=${state.version}&format=${format}`;
  try {
    const res = await fetch(url);
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(body.error || `HTTP ${res.status}`);
    }
    const data = await res.text();
    const name = `cairn-markers-v${state.version}.${format === "fcpxml" ? "xml" : format}`;
    await saveFile(name, data);
    note(`saved ${name} — import it: File > Import (${format === "fcpxml" ? "FCP7 XML" : format.toUpperCase()})`);
  } catch (e) {
    note(e.message, true);
  }
}

/* UXP file picker when running inside Premiere; browser download when
 * running in a plain browser (dev against the mock daemon). */
async function saveFile(name, data) {
  if (typeof require === "function") {
    const fs = require("uxp").storage.localFileSystem;
    const file = await fs.getFileForSaving(name);
    if (!file) return; // user cancelled
    await file.write(data, { append: false });
    return;
  }
  const blob = new Blob([data], { type: "text/plain" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 4000);
}

/* ---- wiring ---- */

$("project").addEventListener("change", (ev) => {
  state.project = ev.target.value;
  fillVersions();
});
$("version").addEventListener("change", (ev) => {
  state.version = Number(ev.target.value) || 0;
  loadMarkers();
});
$("refresh").addEventListener("click", async () => {
  await getStatus();
  await loadReview();
});
$("export-fcpxml").addEventListener("click", () => exportAs("fcpxml"));
$("export-otio").addEventListener("click", () => exportAs("otio"));
$("export-csv").addEventListener("click", () => exportAs("csv"));

getStatus().then(loadReview);
