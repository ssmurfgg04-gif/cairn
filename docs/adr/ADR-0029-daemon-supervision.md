# ADR-0029: The tray supervises the daemon — install-and-it-just-works

Date: 2026-09-05 · Status: accepted (implemented round 26) · Scope: cairn-tray, install.ps1, release.yml

## Context

The v1.0 release path works: `irm …/install.ps1 | iex` downloads the
statically-linked engine, verifies SHA256, installs the tray, registers
autologin, runs `cairn init`. And then it hands the user a *dead product*:

- the tray autostarts at login, but it only **watches**. Its status worker
  polls `cairn status --json`, and when the daemon is down the tooltip says
  "Cairn — daemon not running", the balloon says "restart it from your
  terminal", and the attach error message says "Is the daemon running?".
- nothing else ever starts the daemon. `cairn attach`, `cairn status`
  (live view), the dashboard at 127.0.0.1:17778, the review portal — all of
  it needs a running daemon, and the only path to one is a terminal:
  `cairn daemon`.

That contradicts the product's one promise (ADR-0016 "Clicky-Clicky": the
video editor never needs a terminal). The user's framing of the gap, this
round: *"most important is just a windows exe so users can install this and
it just works."*

Two adjacent facts sharpened the round:

1. The v1.0.0 re-tag release run (33948944674) **failed to build**: the
   workflow's `RUSTFLAGS: -C target-feature=+crt-static` *replaces*
   `.cargo/config.toml`'s `rustflags` (cargo's documented precedence), so
   tokio's `io-uring` feature (ADR-0025) lost its `--cfg tokio_unstable`
   and hard-errored — the config file even documents this exact trap.
   The installer gate never ran.
2. Every prior dashboard/review verification used the **mock daemon**
   (`scripts/mock_dashboard.py`), never the real one against real
   projects. Both gaps close this round (local Linux daemon, real file
   trees, real portal).

## Decision

### A. The tray supervises the daemon (probe → spawn → backoff)

A pure state machine (`crates/cairn-tray/src/supervise.rs`, no Win32, no
I/O — unit-tested on every platform, wired into the windows-only poll
worker):

- every 3s poll feeds `observe(daemon_up, now)`;
- daemon **down** + backoff elapsed → `Action::Spawn`: the tray runs
  `cairn.exe daemon` **hidden** (`CREATE_NO_WINDOW`, same flag every
  wrapped CLI call already uses), stderr appended to
  `<CAIRN_HOME or ~/.cairn>/daemon.log`;
- daemon **up** → reset the crash ladder, do nothing;
- backoff ladder `8s → 15s → 30s → 60s → 120s → 300s cap`: the first
  entry doubles as boot grace (a slow cold start is not judged a crash),
  and a persistently dying daemon is retried at most every 5 minutes —
  never a fork bomb, never silent (the daemon-lost balloon now says
  "daemon stopped — restarting it…" instead of telling the user to open
  a terminal).

**The child outlives the tray.** The `Child` handle is dropped
deliberately: Windows does not kill orphaned processes, so quitting the
tray never takes sync down. This *extends* the ADR-0016 boundary rather
than bending it — the tray still never links the engine, never opens the
store, never speaks gRPC; it may now *start* the engine as a subprocess,
which is the same mechanism every other tray capability uses.

**It never fights a live daemon.** Spawn only happens after a probe
reports down, so a user's terminal `cairn daemon` is invisible to us. If
both race, the ctl/dashboard binds are exclusive — one exits, the next
probe agrees on the survivor, the ladder absorbs the blip.

### B. The installer starts the daemon immediately (step 6b)

`install.ps1` gains one step between `cairn init` and the done-banner:
`Start-Process cairn.exe daemon -WindowStyle Hidden` with stderr to
`daemon.log`, skipped when `CAIRN_INSTALL_NO_LAUNCH=1` (CI). The user
finishes the install with a live dashboard at
`http://127.0.0.1:17778` — no terminal, no reboot, no tray click. The
tray takes over supervision at the next login.

### C. The release gate now proves the daemon runs

The `installer-gate` job (which was *skipped* in the failed run) gains a
final step: start the installed `cairn.exe daemon` exactly the way the
tray does, poll `http://127.0.0.1:17778/` for HTTP 200 within 60s (with
the daemon log tail in the failure message), run `cairn status --json`
against the live daemon, then clean up. "Install → daemon runs →
dashboard serves" is now a *tested contract* on a real Windows runner,
not a hope.

### D. The RUSTFLAGS fix

Both `cargo build` steps in `release.yml` now pass
`--cfg tokio_unstable -C target-feature=+crt-static` together, with a
comment naming the dead run. (The Tauri build needs nothing: cairn-app is
workspace-*excluded*, so no workspace tokio in its tree.)

## Rejected alternatives

- **Register the daemon itself in the HKCU Run key** (not the tray): two
  autostart entries with no recovery story — a crash at 2am stays down
  until reboot. The tray already polls every 3s; supervision is free there.
- **Windows Service / scheduled task**: needs admin or a service wrapper;
  the product is per-user, no-admin by design (ADR-0016).
- **CLI commands auto-starting the daemon on-demand**: tempting, but it
  hides lifetimes (`cairn attach` spawns a daemon that dies when the CLI
  exits? or lingers with no supervisor?) and muddies the one-process
  contract in SPEC §11. The tray is the *supervised* path; the terminal
  stays the *explicit* path.
- **Job objects to kill the daemon with the tray**: inverts the guarantee
  we want. Sync outliving the tray is a feature, not a leak.

## Consequences

- Fresh install → live dashboard in seconds; every login → daemon back
  within one poll cycle (3s) plus boot time.
- `~/.cairn/daemon.log` becomes a real diagnostic surface (JSON tracing
  lines, append-only across tray restarts) — `Status Details` and support
  can answer "why did the daemon die" without a terminal.
- The installer gate is strictly stronger: release green now *means*
  install-and-it-just-works, end to end, on a clean Windows profile.
