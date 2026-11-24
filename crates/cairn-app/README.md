# cairn-app — the native window

A Tauri 2 shell around the daemon's loopback console
(`http://127.0.0.1:17778`), using the OS webview — WebView2 on Windows,
WKWebView on macOS. No bundled Chromium, no plugins, no IPC: the window
is a viewport onto a surface the daemon already serves (ADR-0022).

## What the window does (review #58/#60)

- **Opens without a daemon.** It starts on a bundled screen
  (`dist/index.html`), launches `cairn daemon` hidden if nothing answers on
  the console port (stderr appended to `<home>/daemon.log`, like the tray's
  supervisor), and hands over to the console as soon as it answers. After
  ~20 s it says plainly that Cairn isn't starting, with a Try again button.
  No browser error page.
- **One window.** A second launch focuses the existing window.
- **Remembers itself.** Size, position and maximized state persist.
- **Stays on the console.** Only the start screen and
  `http://127.0.0.1:17778` load in the window; any other link opens in the
  system browser.
- **Opens cairn:// links.** The scheme the console's Share copies
  (`cairn://<project>/<path>`) is registered for the user; a link opens
  that project and file, in the already-open window if there is one.
- **Closing is not quitting.** Closing the window never stops syncing.
  "Quit Cairn" lives in the tray menu and in the console's Settings, and
  both say that syncing stops (`cairn stop` / `POST /api/v1/quit`).

Icons are generated from `icons/cairn.svg` (rsvg-convert + Pillow): PNGs
for macOS/Linux and a multi-size `cairn.ico` (16-256 px) for Windows.

## Build

```sh
# Windows (WebView2 is preinstalled on 10/11; `embed` pins stragglers)
cd crates/cairn-app
cargo tauri build          # NSIS bundle in target/release/bundle/
cargo check                # compile gate only
cargo test                 # navigation guard + engine discovery
# Linux: needs libwebkit2gtk-4.1-dev (CI compiles on windows-latest)

# macOS
cargo tauri build          # .app bundle
```

The crate is a STANDALONE package (workspace `exclude`) on purpose: the
workspace's fmt/clippy/test gates run on Linux runners without
webkit2gtk, and this crate's dependency tree is Tauri's. The
`tauri-check` CI job compiles it on `windows-latest`.

## Relationship to the rest

| Piece | Owner |
| --- | --- |
| Tray icon, menu, autostart (HKCU Run) | `cairn-tray` (ADR-0016) |
| Console UI (files/team/search/help/i18n) | `cairn-cli` dashboard assets (ADR-0021) |
| Review portal (dark player) | `cairn-review` (guest links, not this window) |
| This crate | the window chrome only |

The tray's **Open** action should prefer `cairn-app.exe` when present and
fall back to the default browser — both render the same page.
