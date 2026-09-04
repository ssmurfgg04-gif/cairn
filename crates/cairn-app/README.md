# cairn-app — the native window

A Tauri 2 shell around the daemon's loopback console
(`http://127.0.0.1:17778`), using the OS webview — WebView2 on Windows,
WKWebView on macOS. No bundled Chromium, no plugins, no IPC: the window
is a viewport onto a surface the daemon already serves (ADR-0022).

## Build

```sh
# Windows (WebView2 is preinstalled on 10/11; `embed` pins stragglers)
cd crates/cairn-app
cargo tauri build          # NSIS bundle in target/release/bundle/
cargo check                # compile gate only

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
