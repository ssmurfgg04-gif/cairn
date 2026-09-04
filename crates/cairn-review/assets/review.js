// cairn review player — no framework, no build step, no CDN. The page is
// served by the local daemon; everything talks to /r/<token>/api/*.
// Frame math is integer-first: the server supplies fps as num/den and the
// integer timecode rate; comments anchor to frame numbers, timecodes are
// display only. NDF convention throughout.

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
    hbTimer: null,
  };

  const video = $("video");
  const track = $("track");
  const fill = $("fill");
  const head = $("head");

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

    // presence heartbeat
    hbTimer = setInterval(heartbeat, 15_000);
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
      const row = el("div", "vrow" + (v === state.version ? " active" : "")
        + (i === state.session.versions.length - 1 ? " latest" : ""));
      row.append(el("span", "vnum", "v" + v.number));
      row.append(el("span", "vlabel", v.label || ("version " + v.number)));
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
    $("dur").textContent = v.frames + " fr";
    $("fps").textContent = fpsOf(v).toFixed(3).replace(/0+$/, "").replace(/\.$/, "") + " fps";
    $("toggle-proxy").hidden = !v.has_proxy;
    const wasPlaying = !first && !video.paused;
    video.src = mediaUrl(v);
    if (wasPlaying) video.play().catch(() => {});
    renderVersions();
    renderComments();
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
    $("ncount").textContent = String(active.length) +
      (other.length ? " (+" + other.length + " other versions)" : "");

    active.forEach((c) => list.append(commentRow(c, true)));
    other.forEach((c) => list.append(commentRow(c, false)));

    renderMarks(active);
  }

  function commentRow(c, isActive) {
    const row = el("li", "crow" + (c.status === "RESOLVED" ? " resolved" : ""));
    if (!isActive) row.style.opacity = "0.55";

    const left = el("div");
    const tc = el("div", "ctc", c.tc);
    if (isActive) {
      tc.addEventListener("click", () => {
        seekFrame(state.version, c.frame);
        video.pause();
      });
    }
    left.append(tc);
    left.append(el("div", "cmeta", c.author + " · v" + c.version));

    const right = el("div");
    const body = el("div", "cbody", c.body);
    const stat = el("span", "cstat " + c.status, c.status);
    body.append(stat);
    right.append(body);
    if (state.session.role === "commenter") {
      const canResolve = c.status === "OPEN";
      const btn = el("button", "cresolve", canResolve ? "mark resolved" : "reopen");
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
      const m = el("div", "cmark" + (c.status === "RESOLVED" ? " resolved" : ""));
      m.style.left = (100 * c.frame / dur) + "%";
      m.title = c.tc + " — " + c.body;
      m.addEventListener("click", () => {
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
      li.append(el("span", "dot"));
      li.append(el("span", "", p.reviewer +
        (state.version && p.version === state.version.number
          ? " · v" + p.version + " · " + tcOf(p.frame, state.version.tc_rate)
          : " · v" + p.version)));
      list.append(li);
    });
    if (!list.children.length) {
      const li = el("li", "me");
      li.append(el("span", "dot"));
      li.append(el("span", "", "just you"));
      list.append(li);
    }
  }

  // ---------- transport ----------

  video.addEventListener("timeupdate", () => {
    if (!state.version) return;
    const f = frameNow(state.version);
    if (f !== state.lastFrame) {
      state.lastFrame = f;
      $("tc").textContent = tcOf(f, state.version.tc_rate);
      const pct = 100 * f / Math.max(1, state.version.frames - 1);
      fill.style.width = pct + "%";
      head.style.left = pct + "%";
    }
  });

  $("play").addEventListener("click", () => {
    if (video.paused) video.play().catch(() => {}); else video.pause();
  });

  function step(delta) {
    if (!state.version) return;
    video.pause();
    seekFrame(state.version, frameNow(state.version) + delta);
  }
  $("nextf").addEventListener("click", () => step(1));
  $("prevf").addEventListener("click", () => step(-1));

  track.addEventListener("pointerdown", (ev) => {
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

  document.addEventListener("keydown", (ev) => {
    if (ev.target.tagName === "INPUT" || ev.target.tagName === "TEXTAREA") return;
    if (ev.code === "Space") { ev.preventDefault(); $("play").click(); }
    else if (ev.code === "ArrowRight") step(1);
    else if (ev.code === "ArrowLeft") step(-1);
  });

  $("toggle-proxy").addEventListener("click", () => {
    state.useFull = !state.useFull;
    $("toggle-proxy").textContent = state.useFull ? "proxy" : "full res";
    if (state.version) {
      const f = frameNow(state.version);
      const wasPlaying = !video.paused;
      video.src = mediaUrl(state.version);
      video.addEventListener("loadedmetadata", function once() {
        video.removeEventListener("loadedmetadata", once);
        seekFrame(state.version, f);
        if (wasPlaying) video.play().catch(() => {});
      });
    }
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
