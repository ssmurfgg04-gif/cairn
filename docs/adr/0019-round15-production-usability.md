# ADR-0019: Round 15 — the production-usability five

Date: 2026-09-04
Status: accepted

## Context

Round 14 closed the swarm transport and the Kani-heavy-shard hardening. The
user's round-15 direction (evidence: their five-item TL;DR) targets the gap
between a robust headless engine and a production-grade media sync tool:
CfAPI lifecycle resilience, multi-root single-daemon operation, OTIO schema
leniency, zero-config LAN discovery, and Windows shell visual feedback.

## §1 Cloud Filter lifecycle resilience

**Problem**: Explorer intermittently shows `cloud file provider not running`
for placeholders until some application call triggers the callback; directory
enumeration callbacks (`FETCH_PLACEHOLDERS`) ran synchronously on the filter's
callback thread, so a cold cache hit could stall (or hide) directory nodes.

**Design**:
- `FETCH_PLACEHOLDERS` now completes ASYNCHRONOUSLY: the callback captures the
  three filter-owned keys (`ConnectionKey`, `TransferKey`, `RequestKey` —
  plain handles the documented async-completion contract keeps valid past the
  callback), the owned `path`/`pattern` and the `Arc<dyn PlaceholderSource>`,
  posts a job to a 4-thread pool, and returns immediately. The worker runs the
  enumeration and completes via `CfExecute` built from the captured keys
  (`transfer_placeholders_keys`). A dead pool falls back to inline completion
  — never a hung Explorer.
- A per-root **watchdog** (`spawn_cfapi_watchdog`, `CAIRN_CFAPI_WATCHDOG_SECS`,
  default 30 + jitter) re-attaches through the same idempotent path as boot
  attach whenever the `CFAPI_CONNS` entry vanished (mid-session drop, filter
  reload), logging each recovery.
- The write-back variant (`on_fetch_placeholders_wb`) stays synchronous: its
  caller is the daemon's own attach path, not Explorer's UI thread.

## §2 Multi-root routing on a single daemon

**Problem**: two roots of one project under one login share the journal's
`device_id`, so the pull phase's own-op suppression ("already folded
locally") skipped EVERY cross-root entry — the cursor advanced, nothing
applied — forcing separate `CAIRN_HOME`s + daemons just to test two-device
flows.

**Design**: local root namespacing + root-qualified journal authorship.
- Every attached root gets a `root_id` ("" for the default/legacy root, 8-hex
  blake3 for additional roots); registry in the store meta
  (`root:<pid>:<rid>` → canonical path), `workspace:<ns>` per root.
- `local_ns = pid` (default root) or `pid#<rid>` — all LOCAL row tables
  (files, cursor, outbox, forks, leases) key on this; the row tables of two
  roots of one project are fully isolated.
- `author_id = dev` (default) or `dev#<rid>` — the journal author AND the
  own-op-suppression key: only SAME-root entries are skipped; cross-root
  entries apply, which is exactly the two-device convergence contract. This
  is the "journal sequence generator decoupled from local socket identity"
  the user asked for.
- **Compatibility**: the first/legacy root keeps the plain namespace and
  plain authorship — pre-round-15 stores, cursors and journals are
  byte-compatible; the legacy `workspace:<pid>` binding is adopted by the
  default root on upgrade (test-pinned).
- Request ids stay server-scoped (same content from either root → same id →
  server dedup — one journal entry, convergent either way).

**Evidence**: `cairn-sim::multoroot_tests::one_home_two_roots_converge` — one
home, one server journal, two roots: A→B and B→A convergence, quiet passes
(no livelock, bounded journal), and the W5 deterministic-conflict contract
(original path converges on both roots, B's bytes survive as a conflict copy)
all hold on a single daemon. The test also caught a real test-harness bug
(dropping the server TempDir deletes the sqlite file; later pool connections
reopen an empty DB — "no such table: chunks" only under parallelism).

## §3 OTIO schema leniency (`tl-capture`)

**Problem**: strict parse rejected structurally-variant OTIO from third-party
editors (bare `Track.1` roots, `Timeline.tracks` as an array or a single
track object, `children` as a single object, roots with no `OTIO_SCHEMA`).

**Design**: a pure JSON→JSON pre-ingestion normalizer
(`normalize_otio_value`) coerces those shapes into the canonical
`Timeline{tracks: Stack{children: [...]}}` hierarchy before the strict parse.
Coercion is STRUCTURAL ONLY — unknown schema-version tags are still refused
with the exact error (a version rewrite could silently change semantics;
honesty beats a bad guess). Idempotent: canonical documents are a fixed
point (corpus-pinned — the 17/18 real-timeline gate is unchanged).
Six leniency tests: bare-track root, tracks-as-array, tracks-as-single-track,
missing-schema sniffing, garbage-still-refuses, canonical fixed point.

## §4 mDNS LAN discovery (zero-config joins)

**Problem**: joining a swarm needed BOTH the join code and the host's signal
address. On a trusted LAN the address is discoverable.

**Design** (`cairn-p2p/src/mdns.rs`, zero new dependencies):
- Hand-rolled DNS packet codec (PTR/SRV/TXT; compression pointers parsed,
  emitted uncompressed — both legal), `MdnsTransport` seam (real: multicast
  UDP 224.0.0.251:5353 with bounded reads; tests: in-memory bus — the
  stun.rs pattern).
- The signal server announces a beacon (PTR+SRV+TXT) whose TXT carries a
  16-hex FINGERPRINT of the join code (`blake3("cairn-mdns/v1" ‖ code)`) —
  the code itself never travels. Joiners with `--swarm-mdns` browse for a
  matching fingerprint and fill in the signal address automatically
  (1.5 s window; explicit `--swarm-signal` remains available and takes
  precedence via mutual exclusion).
- **Trust model** (deliberately conservative): mDNS is spoofable; a forged
  beacon can only redirect a joiner to a fake signal server — which fails
  exactly like a wrong `--swarm-signal` today (registration is HMAC'd with
  the cluster key derived from the code; peers fail-closed). Discovery is
  never admission; the code always is.
- The host side: `cairn signal` announces by default when a join code is
  active and the bind is wildcard (`--no-mdns` opts out).

## §5 Windows shell extension (visual feedback)

**Problem**: sync health is invisible outside the CLI.

**Design** (`cairn-shell-ext` crate): reference-grade polish without WebViews.
- **State transport**: the daemon writes `<root>/.cairn/overlay.json` (one
  best-effort write per pass, conflict/fetching states prioritized under the
  entry cap); `<root>/.cairn/root.json` (written at attach) marks cairn
  roots and names the project. The COM layer reads files only — never
  sqlite — so Explorer's process never contends the store.
- **`core` module** (cross-platform, 5 unit tests): state model with icon
  priority Conflict > Fetching > Pinned > Synced, state-file read/parse
  (fail-quiet on corruption — a corrupt file means no icons, never wrong
  icons), bounded writer, root resolution (walk-up marker search),
  project-relative path computation, and the context-menu actions
  (`cairn lock/unlock --project --path`, `cairn snapshot create --project`)
  as argv constructions.
- **`com` module** (cfg(windows), compiled by the windows CI matrix):
  manual-vtable COM (the workspace's cfapi.rs pattern) — DLL exports
  (`DllGetClassObject`, `DllCanUnloadNow`, `DllRegisterServer` writing
  HKCU-based keys so regsvr32 needs no elevation), four
  `IExplorerIconOverlayIdentifier` handlers (overlay slots named with a
  leading space — Explorer's ~15-overlay cap sorts by name), and the
  `IShellExtInit`/`IContextMenu` adapter invoking the audited CLI via
  `ShellExecuteExW`. The first milestone ships the full overlay pipeline and
  the invoke plumbing; the CF_HDROP selection parse and QI overrides for the
  menu object land with the icon resource pack (tracked below).

**Known follow-ups (honest ledger)**: icon resource pack
(`cairn-icons.ico` + full `QueryContextMenu` insertion + CF_HDROP
plumbing + per-interface QI for the overlay/menu objects); a signed build +
installer step (shell extensions require signing for real deployment);
per-machine (HKCR) registration variant.

## Non-goals

- No new external crates anywhere (mDNS hand-rolled; overlays file-based).
- mDNS does NOT authenticate peers (ADR-0017 §7 unchanged — the join code is
  the admission boundary; discovery only fills an address).
- The shell extension does not write project state — every mutating action
  shells out to the CLI (one audited entry point).

## Test evidence

- `cairn-sim`: `one_home_two_roots_converge` (§2, including the W5 contract)
- `cairn-tl`: 86 tests + 6 leniency tests; real-timeline corpus 17/18 (§3)
- `cairn-p2p`: 55 unit + 7 e2e (incl. 5 mDNS: codec roundtrip, compression
  pointers, fingerprint stability, browse-and-filter) (§4)
- `cairn-shell-ext`: 5 core tests (§5)
- `cairn-sync`: 41 tests (multi-root workspace registry)
- Windows-only surfaces (§1 COM glue, §5 com module) compile on the windows
  CI matrix; the Kani-fix shards re-prove on the 27-runner kani.yml.
