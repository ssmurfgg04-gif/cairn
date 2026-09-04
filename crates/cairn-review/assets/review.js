// cairn review player — no framework, no build step, no CDN. The page is
// served by the local daemon; everything talks to /r/<token>/api/*.
// Frame math is integer-first: the server supplies fps as num/den and the
// integer timecode rate; comments anchor to frame numbers, timecodes are
// display only. NDF convention throughout.
//
// Round 17 (ADR-0021): J/K/L shuttle, frame-accurate keyboard stepping,
// zoom to 400% with pan, honest waveform (drawn only when the real audio
// decodes — never faked), buffered range on the scrub bar, live composer
// timecode, comment filters, and full interactive states everywhere.

(function () {
  "use strict";

  const token = location.pathname.split("/").filter(Boolean)[1] || "";
  const $ = (id) => document.getElementById(id);

  const state = {
    session: null,
    version: null,      // active ReviewVersion object
    useFull: false,     // force full-res stream
    me: null,           // reviewer name (localStorage)
    lastFrame: -1,
    filter: "all",      // all | OPEN | RESOLVED
  };

  const video = $("video");
  const scrub = $("scrub");
  const track = $("track");
  const fill = $("fill");
  const head = $("head");
  const buffer = $("buffer");
  const zoomer = $("zoomer");

  // ---------- helpers ----------

  function tcOf(frame, rate) {
    const f = Math.max(0, Math.floor(frame));
    const secs = Math.floor(f / rate);
    const ff = f % rate;
    const ss = secs % 60, mm = Math.floor(secs / 60) % 60, hh = Math.floor(secs / 3600);
    const p = (n) => String(n).padStart(2, "0");
    return p(hh) + ":" + p(mm) + ":" + p(ss) + ":" + p(ff);
  }

  function fpsOf(v) { return v.fps_num / Math.max(1, v.fps_den); }
  function frameNow(v) {
    return Math.max(0, Math.min(v.frames - 1, Math.round(video.currentTime * fpsOf(v))));
  }
  function seekFrame(v, frame) {
    video.currentTime = Math.min(frame / fpsOf(v), Math.max(0, video.duration || 0));
  }
  function durOf(v) {
    const s = v.frames / fpsOf(v);
    const m = Math.floor(s / 60);
    return m > 0 ? `${m}m ${Math.round(s % 60)}s` : `${Math.round(s)}s`;
  }
  function relTime(ms) {
    if (!Number.isFinite(ms) || ms <= 0) return "";
    const d = Date.now() - ms;
    if (d < 0) return "";
    if (d < 60e3) return "just now";
    if (d < 3600e3) return Math.floor(d / 60e3) + "m ago";
    if (d < 86400e3) return Math.floor(d / 3600e3) + "h ago";
    return Math.floor(d / 86400e3) + "d ago";
  }
  function initials(name) {
    const parts = (name || "").trim().split(/\s+/).filter(Boolean);
    return (parts.map((w) => w[0]).slice(0, 2).join("") || "?").toUpperCase();
  }

  async function api(path, body) {
    const res = await fetch("/r/" + encodeURIComponent(token) + path, body
      ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }
      : {});
    return { status: res.status, data: await res.json().catch(() => ({})) };
  }

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  // ---------- session bootstrap ----------

  async function boot() {
    // skeleton while loading
    $("comments").replaceChildren(
      el("div", "skel"), el("div", "skel"), el("div", "skel"));

    const { status, data } = await api("/api/session");
    if (status !== 200 || !data.ok) {
      video.removeAttribute("src");
      $("notfound").hidden = false;
      $("comments").replaceChildren();
      return;
    }
    state.session = data;
    $("title").textContent = data.title || "Review";
    $("linknote").textContent = data.note || "client review link";
    document.title = (data.title || "Review") + " — Cairn";
    renderVersions();
    pickVersion(data.versions[data.versions.length - 1], true);
    renderComments();
    renderPresence();

    // composer visibility by role
    const canComment = data.role === "commenter";
    $("composer").hidden = !canComment;
    $("viewer-note").hidden = canComment;
    if (canComment) {
      state.me = localStorage.getItem("cairn-review-name") || "";
      $("author").value = state.me;
    }

    // restore volume preference
    const vol = parseFloat(localStorage.getItem("cairn-review-vol"));
    if (Number.isFinite(vol)) video.volume = vol;
    if (localStorage.getItem("cairn-review-muted") === "1") video.muted = true;
    syncVolumeUI();

    // presence heartbeat
    setInterval(heartbeat, 15_000);
    // poll for peer comments + presence (the portal is pull-based; the
    // daemon is local, 5 s polls are free)
    setInterval(refresh, 5_000);
  }

  async function refresh() {
    const { status, data } = await api("/api/session");
    if (status !== 200 || !data.ok) return;
    state.session = data;
    renderVersions();
    renderComments();
    renderPresence();
  }

  async function heartbeat() {
    if (!state.version) return;
    await api("/api/presence", {
      reviewer: state.me || "guest",
      version: state.version.number,
      frame: frameNow(state.version),
    });
  }

  // ---------- rendering ----------

  function renderVersions() {
    const list = $("versions");
    list.replaceChildren();
    state.session.versions.forEach((v, i) => {
      const isLatest = i === state.session.versions.length - 1;
      const row = el("div", "vrow" + (v === state.version ? " active" : ""));
      row.append(el("span", "vnum", "v" + v.number));
      const main = el("div", "vmain");
      const label = el("div", "vlabel", v.label || ("version " + v.number));
      if (isLatest) {
        const b = el("span", "badge latest", "latest");
        b.style.marginLeft = "6px";
        label.append(b);
      }
      const meta = el("div", "vmeta",
        v.frames + " fr · " + durOf(v) + " · " + (v.published_by || "—"));
      main.append(label, meta);
      row.append(main);
      row.addEventListener("click", () => pickVersion(v, false));
      list.append(row);
    });
    if (state.session.latest_only) {
      // view-only-to-latest links: hide the stack entirely
      list.replaceChildren();
    }
  }

  function mediaUrl(v) {
    const full = state.useFull ? "?full=1" : "";
    return "/r/" + encodeURIComponent(token) + "/media/" + v.number + full;
  }

  function pickVersion(v, first) {
    if (!v) return;
    state.version = v;
    state.lastFrame = -1;
    $("framesub").textContent = "frame 0 / " + v.frames;
    $("fps").textContent = fpsOf(v).toFixed(3).replace(/0+$/, "").replace(/\.$/, "") + " fps";
    // honest stream state: a promised proxy that is not ready means the
    // guest is watching full-res (say so; never black-screen, never lie)
    const ss = $("stream-state");
    if (v.has_proxy && !v.proxy_ready) {
      ss.hidden = false;
      ss.textContent = "serving full res — proxy not ready yet";
    } else {
      ss.hidden = true;
    }
    const tp = $("toggle-proxy");
    if (tp) tp.hidden = !v.has_proxy;
    const wasPlaying = !first && !video.paused;
    resetZoom();
    video.src = mediaUrl(v);
    if (wasPlaying) video.play().catch(() => {});
    buffer.style.width = "0%";
    renderVersions();
    renderComments();
    loadWave(v);
    heartbeat();
  }

  function renderComments() {
    const list = $("comments");
    list.replaceChildren();
    const all = state.session.comments || [];
    // frame order within the active version; other versions muted at the end
    const active = all
      .filter((c) => state.version && c.version === state.version.number)
      .sort((a, b) => a.frame - b.frame || a.created_ms - b.created_ms);
    const other = all.filter((c) => state.version && c.version !== state.version.number);

    const visible = (c) => state.filter === "all" || c.status === state.filter;
    const va = active.filter(visible);
    const vo = other.filter(visible);

    $("ncount").textContent = String(va.length) +
      (vo.length ? " (+" + vo.length + ")" : "") +
      (state.filter === "all" ? "" : " " + state.filter.toLowerCase());

    if (va.length + vo.length === 0) {
      const empty = el("li", "viewer-note");
      empty.style.border = "none";
      empty.style.padding = "14px 0";
      empty.textContent = state.filter === "OPEN"
        ? "no open notes on this version — every note is resolved."
        : "no notes yet — the first one lands at the playhead.";
      list.append(empty);
    }

    va.forEach((c) => list.append(commentRow(c, true)));
    vo.forEach((c) => list.append(commentRow(c, false)));

    renderMarks(active);
  }

  // (round 20) mechanical chip styles injected once
  if (!document.getElementById("cmech-style")) {
    const st = document.createElement("style");
    st.id = "cmech-style";
    st.textContent = `
.cmech { display:flex; gap:8px; align-items:center; margin-top:6px; padding:4px 8px;
  border:1px solid rgba(255,215,106,.25); border-radius:6px; background:rgba(255,215,106,.07); }
.cmech-tag { font-size:9px; font-weight:700; letter-spacing:.08em; text-transform:uppercase;
  color:#ffd76a; }
.cmech-ops { font-size:11px; color:#f2ede3; font-family:var(--mono, monospace); }
`;
    document.head.append(st);
  }

  function commentRow(c, isActive) {
    const row = el("li", "crow" + (c.status === "RESOLVED" ? " resolved" : "") +
      (isActive ? "" : " dim"));

    const left = el("div");
    left.append(el("span", "avatar", initials(c.author)));

    const right = el("div");
    const headLine = el("div");
    headLine.style.display = "flex";
    headLine.style.alignItems = "center";
    headLine.style.gap = "8px";
    headLine.style.flexWrap = "wrap";
    const tc = el("button", "ctc", c.tc);
    tc.type = "button";
    if (isActive) {
      tc.addEventListener("click", () => {
        seekFrame(state.version, c.frame);
        video.pause();
      });
    } else {
      tc.disabled = true;
      tc.style.cursor = "default";
    }
    headLine.append(tc);
    const stat = el("span", "cstat " + c.status, c.status);
    headLine.append(stat);
    const when = relTime(c.created_ms);
    const meta = el("span", "cmeta");
    const b = el("b", "", c.author);
    meta.append(b, document.createTextNode(
      (when ? " · " + when : "") + (isActive ? "" : " · v" + c.version)));
    headLine.append(meta);
    right.append(headLine);

    right.append(el("div", "cbody", c.body));

    // the no-AI robot's read (round 20): mechanical notes get a chip the
    // editor can act on; creative notes get the honest "your call" mark.
    if (Array.isArray(c.parsed) && c.parsed.length) {
      const chip = el("div", "cmech");
      chip.append(el("span", "cmech-tag", "mechanical"));
      chip.append(el("span", "cmech-ops", c.parsed.join(" · ")));
      right.append(chip);
    }

    if (state.session.role === "commenter") {
      const canResolve = c.status === "OPEN";
      const btn = el("button", "cresolve", canResolve ? "mark resolved" : "reopen");
      btn.type = "button";
      btn.addEventListener("click", async () => {
        await api("/api/resolve", {
          version: c.version, id: c.id,
          status: canResolve ? "RESOLVED" : "OPEN",
        });
        refresh();
      });
      right.append(btn);
    }
    row.append(left, right);
    return row;
  }

  function renderMarks(active) {
    const layer = $("marklayer");
    layer.replaceChildren();
    if (!state.version) return;
    const dur = state.version.frames || 1;
    active.forEach((c) => {
      const m = el("button", "cmark" + (c.status === "RESOLVED" ? " resolved" : ""));
      m.type = "button";
      m.style.left = (100 * c.frame / dur) + "%";
      m.title = c.tc + " — " + c.body;
      m.setAttribute("aria-label", c.tc);
      m.addEventListener("click", (ev) => {
        ev.stopPropagation();
        seekFrame(state.version, c.frame);
        video.pause();
      });
      layer.append(m);
    });
  }

  function renderPresence() {
    const list = $("presence");
    list.replaceChildren();
    const mine = state.me || "guest";
    (state.session.presence || []).forEach((p) => {
      const li = el("li", p.reviewer === mine ? "me" : "");
      li.append(el("span", "avatar", initials(p.reviewer)));
      li.append(el("span", "", p.reviewer === mine ? p.reviewer + " (you)" : p.reviewer));
      const where = el("span", "pwhere",
        state.version && p.version === state.version.number
          ? "v" + p.version + " · " + tcOf(p.frame, state.version.tc_rate)
          : "v" + p.version);
      li.append(where, el("span", "dot"));
      list.append(li);
    });
    if (!list.children.length) {
      const li = el("li", "me");
      li.append(el("span", "avatar", initials(mine)));
      li.append(el("span", "", "just you"));
      li.append(el("span", "dot"));
      list.append(li);
    }
  }

  // ---------- waveform (honest: drawn only when the audio really decodes) ----------

  const WAVE_BUDGET = 40 * 1024 * 1024; // only analyze small/proxy media
  const waveCache = new Map();          // version:full -> Float32Array | null
  let waveFor = null;                   // version the canvas currently shows

  async function loadWave(v) {
    const key = v.number + ":" + (state.useFull ? "full" : "proxy");
    waveFor = key;
    if (!waveCache.has(key)) {
      let peaks = null;
      try {
        const head = await fetch(mediaUrl(v), { method: "HEAD" });
        const len = Number(head.headers.get("Content-Length"));
        if (Number.isFinite(len) && len > 0 && len <= WAVE_BUDGET) {
          const buf = await (await fetch(mediaUrl(v))).arrayBuffer();
          const Ctx = window.AudioContext || window.webkitAudioContext;
          if (Ctx) {
            const actx = new Ctx();
            const audio = await actx.decodeAudioData(buf);
            peaks = computePeaks(audio, 700);
            actx.close();
          }
        }
      } catch { peaks = null; }
      waveCache.set(key, peaks);
    }
    if (waveFor === key) drawWave(waveCache.get(key), progressRatio());
  }

  function computePeaks(audio, n) {
    const ch = audio.getChannelData(0);
    const ch2 = audio.numberOfChannels > 1 ? audio.getChannelData(1) : null;
    const block = Math.max(1, Math.floor(ch.length / n));
    const peaks = new Float32Array(n);
    const stride = Math.max(1, Math.floor(block / 64));
    for (let i = 0; i < n; i++) {
      let max = 0;
      const start = i * block;
      for (let j = 0; j < block; j += stride) {
        const a = Math.abs(ch[start + j] || 0);
        if (a > max) max = a;
        if (ch2) {
          const b = Math.abs(ch2[start + j] || 0);
          if (b > max) max = b;
        }
      }
      peaks[i] = max;
    }
    return peaks;
  }

  function drawWave(peaks, ratio) {
    const cv = $("wave");
    if (!peaks || !peaks.length) { cv.hidden = true; return; }
    cv.hidden = false;
    const dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth || scrub.clientWidth;
    if (!w) return;
    cv.width = Math.round(w * dpr);
    cv.height = Math.round(30 * dpr);
    const ctx = cv.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, 30);
    const n = peaks.length;
    const played = Math.floor(n * ratio);
    const barW = Math.max(1, w / n - 0.5);
    for (let i = 0; i < n; i++) {
      const h = Math.max(1, peaks[i] * 26);
      ctx.fillStyle = i < played ? "rgba(47, 155, 255, 0.75)" : "rgba(166, 171, 181, 0.30)";
      ctx.fillRect((i / n) * w, 15 - h / 2, barW, h);
    }
  }

  function progressRatio() {
    if (!state.version || state.version.frames < 2) return 0;
    return state.lastFrame < 0 ? 0 : state.lastFrame / (state.version.frames - 1);
  }

  window.addEventListener("resize", () => {
    if (state.version) drawWaveCached();
  });
  function drawWaveCached() {
    if (!state.version) return;
    const key = state.version.number + ":" + (state.useFull ? "full" : "proxy");
    if (waveCache.has(key)) drawWave(waveCache.get(key), progressRatio());
  }

  // ---------- transport ----------

  function syncPlayIcon() {
    $("ic-play").hidden = !video.paused;
    $("ic-pause").hidden = video.paused;
  }
  video.addEventListener("play", () => { stopRewind(); syncPlayIcon(); });
  video.addEventListener("pause", () => { stopRewind(); syncPlayIcon(); });
  syncPlayIcon();

  function togglePlay() {
    if (video.paused) { resetRate(); video.play().catch(() => {}); }
    else video.pause();
  }
  $("play").addEventListener("click", togglePlay);

  video.addEventListener("timeupdate", () => {
    if (!state.version) return;
    const f = frameNow(state.version);
    if (f !== state.lastFrame) {
      state.lastFrame = f;
      $("tc").textContent = tcOf(f, state.version.tc_rate);
      $("framesub").textContent = "frame " + f + " / " + state.version.frames;
      const pct = 100 * f / Math.max(1, state.version.frames - 1);
      fill.style.width = pct + "%";
      head.style.left = pct + "%";
      const chip = $("composer-tc");
      if (chip && !$("composer").hidden) chip.textContent = tcOf(f, state.version.tc_rate);
      drawWaveCached();
    }
  });

  video.addEventListener("progress", () => {
    try {
      const b = video.buffered;
      if (b.length && video.duration > 0) {
        buffer.style.width = (100 * b.end(b.length - 1) / video.duration) + "%";
      }
    } catch { /* buffered races duration: keep last paint */ }
  });

  function step(delta) {
    if (!state.version) return;
    video.pause();
    seekFrame(state.version, frameNow(state.version) + delta);
  }

  // J/K/L shuttle: L doubles native forward rate (max 8x); J runs a synthetic
  // rewind ticker (negative playbackRate is not portable across browsers); K
  // stops everything. The spacebar always resets to 1x.
  let rate = 1;
  let rwTimer = null;
  let rwSpeed = 2;

  function resetRate() { rate = 1; video.playbackRate = 1; }
  function stopRewind() {
    if (rwTimer) { clearInterval(rwTimer); rwTimer = null; }
  }
  function kPress() { stopRewind(); resetRate(); video.pause(); }
  function lPress() {
    stopRewind();
    rate = video.paused ? 1 : Math.min(8, rate * 2 || 1);
    video.playbackRate = rate;
    video.play().catch(() => {});
  }
  function jPress() {
    video.pause();
    resetRate();
    if (!state.version) return;
    rwSpeed = rwTimer ? Math.min(64, rwSpeed * 2) : 2;
    stopRewind();
    const framesPerTick = () =>
      Math.max(1, Math.round(rwSpeed * fpsOf(state.version) / 20));
    rwTimer = setInterval(() => {
      seekFrame(state.version, frameNow(state.version) - framesPerTick());
    }, 50);
  }

  // ---------- scrub (pointer-exact, on the whole scrub surface) ----------

  scrub.addEventListener("pointerdown", (ev) => {
    if (!state.version) return;
    const seekTo = (ev2) => {
      const r = track.getBoundingClientRect();
      const pct = Math.min(1, Math.max(0, (ev2.clientX - r.left) / r.width));
      seekFrame(state.version, Math.round(pct * (state.version.frames - 1)));
    };
    seekTo(ev);
    const mv = (ev2) => seekTo(ev2);
    const up = () => {
      window.removeEventListener("pointermove", mv);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", mv);
    window.addEventListener("pointerup", up);
  });

  // ---------- zoom (100 / 200 / 400% with pan) ----------

  let zoomLevel = 1;
  let panX = 0, panY = 0;

  function applyZoom() {
    zoomer.style.transform =
      "translate(" + panX + "px, " + panY + "px) scale(" + zoomLevel + ")";
    $("zoom").textContent = zoomLevel + "x";
    const hud = $("zoom-hud");
    hud.hidden = zoomLevel === 1;
    hud.textContent = Math.round(zoomLevel * 100) + "%";
    $("viewport").style.cursor = zoomLevel > 1 ? "grab" : "pointer";
  }
  function resetZoom() {
    zoomLevel = 1; panX = 0; panY = 0;
    applyZoom();
  }
  function cycleZoom() {
    zoomLevel = zoomLevel >= 4 ? 1 : zoomLevel * 2;
    panX = 0; panY = 0;
    applyZoom();
  }
  $("zoom").addEventListener("click", cycleZoom);

  // click = play/pause, drag = pan (only meaningful when zoomed)
  $("viewport").addEventListener("pointerdown", (ev) => {
    if (ev.target.closest(".notfound")) return;
    const startX = ev.clientX, startY = ev.clientY;
    const startPan = { x: panX, y: panY };
    let moved = false;
    const vp = $("viewport");
    const clamp = () => {
      const r = vp.getBoundingClientRect();
      const lim = Math.max(40, r.width * 0.5 * (zoomLevel - 1));
      const limY = Math.max(40, r.height * 0.5 * (zoomLevel - 1));
      panX = Math.max(-lim, Math.min(lim, panX));
      panY = Math.max(-limY, Math.min(limY, panY));
    };
    const mv = (ev2) => {
      const dx = ev2.clientX - startX, dy = ev2.clientY - startY;
      if (!moved && Math.hypot(dx, dy) < 4) return;
      moved = true;
      if (zoomLevel > 1) {
        panX = startPan.x + dx;
        panY = startPan.y + dy;
        clamp();
        vp.style.cursor = "grabbing";
        applyZoom();
      }
    };
    const up = (ev2) => {
      window.removeEventListener("pointermove", mv);
      window.removeEventListener("pointerup", up);
      vp.style.cursor = zoomLevel > 1 ? "grab" : "pointer";
      if (!moved) togglePlay();
    };
    window.addEventListener("pointermove", mv);
    window.addEventListener("pointerup", up);
  });
  $("viewport").addEventListener("dblclick", () => {
    zoomLevel = zoomLevel > 1 ? 1 : 2;
    panX = 0; panY = 0;
    applyZoom();
  });

  // ---------- volume / mute / fullscreen ----------

  function syncVolumeUI() {
    $("volume").value = String(video.muted ? 0 : video.volume);
    $("ic-vol").hidden = video.muted || video.volume === 0;
    $("ic-muted").hidden = !(video.muted || video.volume === 0);
  }
  $("mute").addEventListener("click", () => {
    video.muted = !video.muted;
    localStorage.setItem("cairn-review-muted", video.muted ? "1" : "0");
    syncVolumeUI();
  });
  $("volume").addEventListener("input", (ev) => {
    const v = parseFloat(ev.target.value);
    if (Number.isFinite(v)) {
      video.volume = v;
      if (v > 0) video.muted = false;
      localStorage.setItem("cairn-review-vol", String(v));
      localStorage.setItem("cairn-review-muted", video.muted ? "1" : "0");
    }
    syncVolumeUI();
  });
  syncVolumeUI();

  $("full").addEventListener("click", () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else $("viewport").requestFullscreen().catch(() => {});
  });

  // ---------- proxy / full-res toggle ----------

  const toggleProxy = $("toggle-proxy");
  if (toggleProxy) {
    toggleProxy.addEventListener("click", () => {
      state.useFull = !state.useFull;
      toggleProxy.textContent = state.useFull ? "proxy" : "full res";
      if (state.version) {
        const f = frameNow(state.version);
        const wasPlaying = !video.paused;
        video.src = mediaUrl(state.version);
        video.addEventListener("loadedmetadata", function once() {
          video.removeEventListener("loadedmetadata", once);
          seekFrame(state.version, f);
          if (wasPlaying) video.play().catch(() => {});
        });
        loadWave(state.version);
      }
    });
  }

  // ---------- keyboard ----------

  document.addEventListener("keydown", (ev) => {
    if (ev.target.tagName === "INPUT" || ev.target.tagName === "TEXTAREA") return;
    switch (ev.code) {
      case "Space": ev.preventDefault(); togglePlay(); break;
      case "KeyK": kPress(); break;
      case "KeyJ": jPress(); break;
      case "KeyL": lPress(); break;
      case "ArrowRight": ev.preventDefault(); step(ev.shiftKey ? 10 : 1); break;
      case "ArrowLeft": ev.preventDefault(); step(ev.shiftKey ? -10 : -1); break;
      case "ArrowUp": ev.preventDefault(); step(ev.shiftKey ? 60 : 24); break;
      case "ArrowDown": ev.preventDefault(); step(ev.shiftKey ? -60 : -24); break;
      case "Home": if (state.version) { video.pause(); seekFrame(state.version, 0); } break;
      case "End": if (state.version) { video.pause(); seekFrame(state.version, state.version.frames - 1); } break;
      case "KeyN": {
        ev.preventDefault();
        if (!$("composer").hidden) {
          ($("author").value.trim() ? $("body") : $("author")).focus();
        }
        break;
      }
      case "KeyM": $("mute").click(); break;
      case "KeyF": $("full").click(); break;
      case "Slash":
      case "Question": {
        // '?' opens the key map (shift+/ on US layouts; Question on others)
        if (ev.shiftKey || ev.code === "Question") {
          ev.preventDefault();
          toggleHelp();
        }
        break;
      }
      case "Escape": {
        if (!$("help-overlay").hidden) $("help-overlay").hidden = true;
        break;
      }
      default: break;
    }
  });

  // ---------- help overlay (? — the full key map) ----------

  function toggleHelp() {
    const ov = $("help-overlay");
    ov.hidden = !ov.hidden;
  }
  $("help-close").addEventListener("click", () => { $("help-overlay").hidden = true; });
  $("help-overlay").addEventListener("click", (ev) => {
    if (ev.target === $("help-overlay")) $("help-overlay").hidden = true;
  });

  // ---------- filters ----------

  document.querySelectorAll("#filters .chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      state.filter = chip.dataset.filter;
      document.querySelectorAll("#filters .chip").forEach((c) =>
        c.classList.toggle("active", c === chip));
      renderComments();
    });
  });

  // ---------- composer ----------

  $("composer").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    if (!state.version) return;
    const author = $("author").value.trim() || "guest";
    const body = $("body").value.trim();
    if (!body) return;
    localStorage.setItem("cairn-review-name", author);
    state.me = author;
    const form = $("composer");
    form.classList.add("sending");
    const { data } = await api("/api/comment", {
      version: state.version.number,
      frame: frameNow(state.version),
      body, author,
    });
    form.classList.remove("sending");
    if (data && data.ok) {
      $("body").value = "";
      refresh();
    }
  });

  boot();
})();
