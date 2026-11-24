# Review backlog tracker

Every item from the second product review (`pasted contnet by ai reviewer`, 108 items) with its status and evidence. `scripts/check-backlog.py` runs in CI (review #103): it fails if an item is missing, or if an Open / Partial / Blocked item has no acceptance line. When an item closes, change its row here in the same commit.

Totals: 102 done · 4 decided (closed by a written decision) · 0 partial · 0 open · 2 blocked (needs something outside the repo).

| # | Item | Status | Landed | Evidence / acceptance |
|---|---|---|---|---|
| 1 | Finish cross-device project state | Done | round 31 (2026-10-08) | ADR-0031 Phases 1-3 (synced member/audit/review records, .cairn materialized view); two_homes_e2e revoke propagation |
| 2 | Fix the RBAC holes in the new service layer | Done | round 30 | service actions funnel through rbac_guard (POLICY BOUNDARY, ADR-0030) |
| 3 | Stop pretending `team_join()` actually joins | Done | round 30 | honest join copy: code saved, not joined; real next step |
| 4 | Make join codes authoritative, single-use, expiring and revocable | Done | round 31 (2026-10-08) | single-use burn, revocation, 5-fail/15-min lockout (machine-local, audit-logged) |
| 5 | Reconsider fail-open `Editor` for unknown devices | Done | round 33 (2026-11-24) | MemberFile.unlisted_role: explicit, owner-configurable newcomer role (Editor default kept because joining does not add a roster entry; Reviewer = fail-closed pending); core tests + dashboard-smoke |
| 6 | Fix symlink / reparse-point containment properly | Done | round 30 | safe_join canonicalizes deepest existing ancestor; real-symlink escape tests |
| 7 | Eliminate `first()` for mutations | Done | round 32 (2026-11-24) | views follow a named project; remaining first() uses are explicit 'no project given' fallbacks only (ProjectManager::view_of) |
| 8 | Add a global project context | Done | round 32 (2026-11-24) | one shared project context (select.project-ctx on Files/People/Review); ui_browser_check |
| 9 | Fix Review page selecting only the first project | Done | round 32 (2026-11-24) | Review uses selectedProject(); shares the picker |
| 10 | Make Team/People project-scoped | Done | round 32 (2026-11-24) | GET /api/v1/team?project=; People switches with the picker (browser-checked) |
| 11 | Make marker export project selection explicit | Done | round 31 (2026-10-08) | marker export + review link take explicit project ids |
| 12 | Make review-link generation project-explicit | Done | round 31 (2026-10-08) | marker export + review link take explicit project ids |
| 13 | Kill “Dashboard” as the mental model | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 14 | Primary navigation should be tiny | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 15 | Rename “People & devices” | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 16 | Remove engineering concepts from normal Settings | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 17 | Replace “Locks” with “Who's editing” | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 18 | Replace “placeholder” everywhere user-facing | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 19 | Replace “Recall” everywhere | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 20 | Replace “Pinned” | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 21 | Replace sync-state vocabulary | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 22 | Remove “Linked” | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 23 | Fix onboarding headline | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 24 | Remove the BRAW/GPU jargon from first-run | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 25 | Never tell a normal user to start `cairn daemon` | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 26 | Remove terminal commands from beginner guides | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 27 | Simplify How-to to actual workflows | Done | round 31 (2026-10-08) | plain-language pass (Home/Files/People nav, jargon purge, onboarding copy); not re-audited line by line this round |
| 28 | Review should choose a project first | Done | round 32 (2026-11-24) | Review follows the selected project and shows the picker when 2+ projects |
| 29 | Replace technical version metadata | Done | round 33 (2026-11-24) | versions read label/'Automatic save N' + 'Saved 5 min ago' + who; hash in tooltip only |
| 30 | Simplify review roles | Done | round 33 (2026-11-24) | two everyday roles (Can comment / View only); internal role is a 'for my team' checkbox |
| 31 | Let users choose expiration before generating | Done | round 31 (2026-10-08) | TTL chooser (7/30/90 days) before generating; expiry shown |
| 32 | Make generated link copy UX obvious | Done | round 33 (2026-11-24) | new review link copied at once when allowed, else one big Copy button |
| 33 | Finish cross-device revocation | Done | round 31 (2026-10-08) | cross-machine revoke via synced tombstones, portal gate |
| 34 | Add server-side pagination | Done | round 32 (2026-11-24) | files_page offset/limit/total/next_offset, 5000 ceiling; unit test |
| 35 | Virtualize the file table | Done | round 32 (2026-11-24) | 500-row pages + Show more + content-visibility rows; 1,200-file browser check |
| 36 | Make search indexed | Done | earlier | search module index; search_ten_k_files_and_timelines test |
| 37 | Improve empty file states | Done | round 33 (2026-11-24) | Files empty states: no project / no match / empty folder, each with its one action |
| 38 | Make row actions visible | Done | round 31 | always-visible ••• row menu (44px) |
| 39 | Kill right-click “delete” | Done | round 31 | no destructive right-click delete in the console |
| 40 | Replace browser `confirm()` | Done | round 31 | window.confirm banned; in-page modal with focus trap |
| 41 | Fix the duplicate-file collision bug | Done | round 31 (2026-10-08) | create_new duplicates; restore checkpoint; ffmpeg/ffprobe/merge via spawn_blocking |
| 42 | Make mutations idempotent | Done | round 33 (2026-11-24) | idempotency-key on every console POST; replay returns the first answer, concurrent duplicate 409; tested |
| 43 | Disable buttons during active mutations | Done | round 31 | busy()/done(): buttons disable + aria-busy during mutations |
| 44 | Make restore much harder to accidentally trigger | Done | round 31 (2026-10-08) | create_new duplicates; restore checkpoint; ffmpeg/ffprobe/merge via spawn_blocking |
| 45 | Move `ffmpeg` out of the async runtime | Done | round 31 (2026-10-08) | create_new duplicates; restore checkpoint; ffmpeg/ffprobe/merge via spawn_blocking |
| 46 | Move `ffprobe` out of request execution | Done | round 31 (2026-10-08) | create_new duplicates; restore checkpoint; ffmpeg/ffprobe/merge via spawn_blocking |
| 47 | Move OTIO/FCPXML merge parsing/processing out of async request thread | Done | round 31 (2026-10-08) | create_new duplicates; restore checkpoint; ffmpeg/ffprobe/merge via spawn_blocking |
| 48 | Use a bounded job system | Done | round 33 (2026-11-24) | media_slot(): ffmpeg compress + proxy share 2 FIFO slots; running/waiting in diagnostics; tested |
| 49 | Don't expose fake/incomplete Cloudinary functionality | Done | round 32 (2026-11-24) | via=cloudinary used to return ok:true while doing nothing; now refuses (NOT_AVAILABLE) |
| 50 | Make cloud processing explicitly opt-in | Done | round 32 (2026-11-24) | no cloud processing path exists; nothing leaves the machine |
| 51 | Bundle media tooling or remove the dependency | Done | round 31 (2026-10-08) | NSIS installer: optional SHA256-pinned ffmpeg component (fails closed) |
| 52 | Do not refresh expensive views every 2 seconds | Done | round 31 | 2s fast tier / 15s slow tier, visibilitychange pause |
| 53 | Cache storage telemetry | Done | round 33 (2026-11-24) | 10s server-side view cache for storage + review summary, invalidated before and after review mutations; failures never cached |
| 54 | Cache review summary counts | Done | round 33 (2026-11-24) | 10s server-side view cache for storage + review summary, invalidated before and after review mutations; failures never cached |
| 55 | Avoid full-file-list reload just to populate review media | Done | round 33 (2026-11-24) | ?kind=media server-side filter for the Review picker |
| 56 | Add fetch cancellation/timeouts | Done | round 31 | fetchWithTimeout (15s AbortController) on every JSON call |
| 57 | Handle stale dashboard tokens after daemon restart | Done | round 32 (2026-11-24) | STALE_TOKEN code + one guarded reload; browser-checked across a real daemon restart |
| 58 | Add explicit “Quit Cairn” semantics | Done | round 32 (2026-11-24) | Quit Cairn in Settings + tray ('stops syncing'), POST /api/v1/quit, cairn stop; closing the window never quits |
| 59 | Make auto-recovery user-visible but quiet | Done | round 33 (2026-11-24) | one polite 'Back to normal' line after >=10s of trouble; flap-guarded |
| 60 | Finish native desktop lifecycle cross-platform | Done | round 32 (2026-11-24) | cairn-app: start screen, spawns daemon, single instance, window state, navigation guard; verified on Xvfb |
| 61 | Stop putting dashboard auth tokens in URLs where possible | Done | round 33 (2026-11-24) | token never in a URL: one-time 60s download tickets bound to one file; live stream via fetch + header; ?t= no longer accepted; browser-checked |
| 62 | `index.html` containing a token should be `no-store` | Done | round 30 | no-store index, self-hosted CSP, fonts beacon removed |
| 63 | Add a Content Security Policy | Done | round 30 | no-store index, self-hosted CSP, fonts beacon removed |
| 64 | Remove or vendor Google Fonts | Done | round 30 | no-store index, self-hosted CSP, fonts beacon removed |
| 65 | Audit all secret-bearing logs | Done | round 32 (2026-11-24) | security-sweep 'no raw token logging'; diagnostics bundle redacts secret-shaped values (tested) |
| 66 | Add rate limiting to public review endpoints | Done | round 31 (2026-10-08) | portal rate limiting (60/min resolve, 30/min comments) + 429 Retry-After |
| 67 | Add review-token abuse controls | Done | round 31 (2026-10-08) | portal rate limiting (60/min resolve, 30/min comments) + 429 Retry-After |
| 68 | Add `cargo-deny` | Done | round 32 (2026-11-24) | deny.toml + cargo-deny CI job |
| 69 | Pin GitHub Actions to commit SHAs | Done | round 32 (2026-11-24) | all 170 workflow action refs pinned to commit SHAs |
| 70 | Generate an SBOM | Done | round 32 (2026-11-24) | CycloneDX SBOM attached to releases |
| 71 | Add release provenance | Done | round 32 (2026-11-24) | Sigstore build provenance for engine/tray/installer/SBOM |
| 72 | Code-sign the Windows binary | Blocked |  | pipeline ready (scripts/sign-windows.ps1 + Tauri thumbprint); needs a code-signing certificate in WINDOWS_CERT_PFX_BASE64 / WINDOWS_CERT_PASSWORD |
| 73 | Stop exposing raw backend errors to normal users | Done | round 32 (2026-11-24) | humanError(): plain action-oriented sentences in 4 languages; raw kept for diagnostics |
| 74 | Standardize action-oriented errors | Done | round 32 (2026-11-24) | humanError(): plain action-oriented sentences in 4 languages; raw kept for diagnostics |
| 75 | Finish modal focus management | Done | round 32 (2026-11-24) | panels + help overlay trap Tab and restore focus (browser-checked) |
| 76 | Finish language-selector keyboard semantics | Done | round 31 | language selector roving focus + Arrow/Escape |
| 77 | Give every file action a useful accessible name | Done | round 31 | row menu buttons carry file-specific accessible names |
| 78 | Make status changes screen-reader friendly | Done | round 31 | toast role=status aria-live (assertive for errors) |
| 79 | Increase minimum interactive target sizes | Done | round 31 | 44px primary row targets |
| 80 | Improve mobile/tablet responsiveness | Done | round 33 (2026-11-24) | phone-width Files table (menus on-screen at 420px); 768px verified |
| 81 | Make “Files” the actual center of gravity | Decided | round 33 (2026-11-24) | Decision: keep Home as the landing view. It answers 'is everything OK?' at a glance and already lists recent files with Files one click away; making Files the landing would hide warnings (disk, conflicts, offline). Revisit if usage counts (#95) show most sessions go straight to Files. |
| 82 | Make search genuinely global | Done | round 33 (2026-11-24) | search spans all projects with a fair per-project share; a hit switches project first |
| 83 | Add keyboard navigation throughout the file table | Done | round 32 (2026-11-24) | file rows: arrows/Home/End/PgUp/PgDn/Enter/menu key; focus survives refresh |
| 84 | Replace “disk quota” with real storage language | Done | round 33 (2026-11-24) | verified: no visible copy says 'quota' |
| 85 | Warn before the system is critically full | Done | round 32 (2026-11-24) | Home warns <10% or <10 GiB free, with See storage |
| 86 | Make cleanup actionable | Done | round 33 (2026-11-24) | Free up space (Settings + Home warning) runs the eviction sweep with its protections; confirm explains; browser-checked |
| 87 | Make “Version” understandable | Done | round 33 (2026-11-24) | versions read label/'Automatic save N' + 'Saved 5 min ago' + who; hash in tooltip only |
| 88 | Auto-create a safety checkpoint before restore | Done | round 31 (2026-10-08) | restore takes a safety checkpoint first |
| 89 | Keep expert merge UI, but don't make it the default mental model | Done | round 33 (2026-11-24) | conflict dialog = Merge / Keep both (both lossless); 3-version detail folded |
| 90 | Finish contract-debt #1 | Done | round 30 | CONTRACT-DEBT #1 closed (conflict auto-offer) |
| 91 | Finish the Premiere-host verification | Blocked |  | scaffold landed (premiere-uxp job); needs a self-hosted Windows runner with Premiere (CONTRACT-DEBT #3) |
| 92 | Finish large-waveform admission control | Done | round 30 | waveform admission gate + bounded decode |
| 93 | Add “Send diagnostics” | Done | round 32 (2026-11-24) | Settings > Send diagnostics: redacted bundle + recent errors with refs; nothing uploaded |
| 94 | Give every support failure a correlation ID | Done | round 32 (2026-11-24) | x-cairn-request-id on every response; ok:false bodies + daemon.log carry it |
| 95 | Track product-level metrics | Done | round 33 (2026-11-24) | decided + built: local-only action counts (ids stripped), never sent, visible in Settings, in diagnostics only when ticked; tested + browser-checked |
| 96 | Continue shrinking `dashboard.rs` | Done | round 33 (2026-11-24) | middleware split into dashboard/middleware.rs; route table deliberately kept as one audit surface; adapters thin |
| 97 | Make the service API typed instead of JSON-heavy | Decided | round 33 (2026-11-24) | Decision: not now. Typed request/response structs across ~60 service actions is high churn with no user-visible change; the JSON contract is pinned by dashboard-smoke, mock-daemon and the browser check. Do it when a second client (beyond the console) consumes the service API. |
| 98 | Separate application service from ctl service implementations | Decided | round 33 (2026-11-24) | Decision: not now. The service layer already serves both the console and the gRPC impls (ADR-0030); a further split adds indirection without a second consumer. Revisit with #97. |
| 99 | Remove production `expect()` from hot daemon paths | Done | earlier | no expect() in daemon.rs hot paths |
| 100 | Make process-level failure domains explicit | Done | round 33 (2026-11-24) | docs/FAILURE-DOMAINS.md: each domain, what users see, recovery, code |
| 101 | Clean stale RBAC module documentation | Done | round 29-31 | RBAC module docs reconciled (docs truth pass) |
| 102 | Fix stale live-presence wording | Done | round 32 (2026-11-24) | flag copy said live presence/FEC apply 'at the next swarm join'; flags restart swarms live (debt #2), copy fixed in 4 languages |
| 103 | Make `CONTRACT-DEBT.md` CI-enforced | Done | round 32 (2026-11-24) | this file + scripts/check-backlog.py in CI: open items must carry an acceptance line |
| 104 | Native deep-link registration | Done | round 33 (2026-11-24) | cairn:// links registered by the window app and opened by the console (cold start + forwarded to the open window, verified) |
| 105 | OS notifications | Done | round 32 (2026-11-24) | opt-in desktop notifications: background-only, change-only, 3 event types (browser-checked) |
| 106 | Drag-and-drop onboarding | Done | earlier | drag-and-drop onto the onboarding/attach surface |
| 107 | “Open folder” as the primary action everywhere | Done | round 31 | Open folder is the primary Home action |
| 108 | Better first-run demonstration | Decided | round 33 (2026-11-24) | Decision: no demo project. A fake project would sync to the team server and appear on every teammate's machine; onboarding already starts from the user's real folder in one step. |
