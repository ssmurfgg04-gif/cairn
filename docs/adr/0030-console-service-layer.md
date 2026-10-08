# ADR-0030: The console service layer — dashboard is an adapter, ProjectManager owns lifecycle

Date: 2026-10-08 · Status: accepted (implemented) · Scope: cairn-cli (dashboard.rs, service/*, projects.rs, daemon.rs, review.rs)

## Context

The architecture review scored two areas below the rest of the system:

- **UI/application boundary 7/10** — `dashboard.rs` had become a *second
  application layer*. Handlers opened the store, walked runtimes, loaded
  members/audit files, wrote review JSON, probed media with ffprobe, and
  shelled out to ffmpeg — directly, inline. The gRPC ctl surface served
  the same operations through its own service impls. Two application
  layers over one domain means two places to drift; we had already lived
  this once with the set_flag re-implementation that answered `{ok:true}`
  on HTTP while gRPC answered NOT_FOUND (fixed round 20 by delegating).

  The score's exact complaint: handlers reached *simultaneously* into
  store, filesystem, runtime map, review state, and service calls:

  ```text
  UI
   ↓
  dashboard handlers
   ├── direct store
   ├── direct filesystem
   ├── direct runtime
   ├── direct review
   └── service calls          ← only some handlers did this
  ```

- **Lifecycle/state ownership 7.5/10** — attached projects were discovered
  through a *process-wide global*:

  ```rust
  pub static RUNTIMES: LazyLock<RwLock<HashMap<String, Arc<ProjectRuntime>>>> = …;
  pub static PRESENCE_TX: LazyLock<broadcast::Sender<LocalPresence>> = …;
  ```

  Any module could reach into the map. daemon.rs, dashboard.rs and
  review.rs all did. Lifecycle, identity, UI, syncing and networking were
  implicitly coupled through shared process state; nothing owned "which
  projects exist, in which states, and who may start/stop them".

## Decision

**1. A typed service module — `cairn-cli/src/service/` — is the console's
application API.** The HTTP layer keeps only parsing, responses, and the
security gate (ADR-0009 posture: host/origin/token, untouched):

```text
HTTP adapter (dashboard.rs)    — parse, respond, gate ONLY
        ↓
service (mod/views/actions)    — one function per console operation,
        ↓                        typed in/out, JSON shapes the UI consumes
domain (projects/review/tl/…)  — sync engine, stores, OS
```

Rules enforced by review, not by compiler:

- **No axum types in service signatures.** serde_json and plain Rust
  types only. The tray, a future native panel, and tests call the same
  functions the browser does — no HTTP client required.
- **Authorization lives in the service, not the adapter.** attach/detach
  run `rbac_guard` inside `service::actions::{attach_root, detach_project}`.
  A new endpoint wired by a future PR cannot forget the guard — it has to
  *bypass* it to.
- **Parity ops delegate to the ctl service impls** (snapshots, pins,
  recall, flags, presence) — the round-20 lesson, now structural: the
  HTTP layer is not *able* to re-implement them.
- **Reads in `views`, writes in `actions`.** The two genuinely HTTP-shaped
  operations — SSE streaming and the chunked file download — keep their
  transport in the adapter, but the *policy* (root resolution, traversal
  refusal, materialization checks) returns as typed enums
  (`service::Export`, `service::Download`) decided by the service.

**2. `ProjectManager` (in projects.rs) is the explicit owner of
attached-project lifecycle.** The `RUNTIMES` and `PRESENCE_TX` statics
are gone. The daemon holds exactly one `Arc<ProjectManager>` inside
`DaemonState`; it and only it mutates the runtime map.

```text
ProjectManager
    ├── Project A (runtime, swarm, view, presence handle)
    ├── Project B
    └── presence hub (broadcast)
```

- attach/detach/resume are **methods** on the manager (previously free
  functions mutating a global).
- Readers ask for a **snapshot** (`list()`, `first()`, `find_by_project()`,
  `project_root()`, `roots()`) — deterministic order by namespace, so
  dashboards and review cards stop depending on HashMap iteration order.
- Each runtime carries its own clone of the presence-sender, so the sync
  loop's forwarder needs neither the manager nor a global.
- Deliberately still process-local (documented, not forgotten):
  `CFAPI_CONNS` / `OVERLAY_FP` — Windows filter-driver caches that must
  outlive the attach. Platform cache, not project state.

## What did NOT change

- The wire contract. Every `/api/v1/*` endpoint answers byte-identical
  JSON shapes; the review portal, ctl gRPC, tray and NLE panel are
  untouched. This is a boundary move, not a behavior change.
- cairn-app (ADR-0022) stays `native shell → WebView → 127.0.0.1:17778`.
  The review called this choice "pragmatic, not wrong"; a native IPC
  surface would add a third transport for zero user-visible gain. The
  service layer is the seam a future IPC would call if we ever build one —
  and it cost no Tauri work.

## Consequences

**Good:** one place to look for console behavior; RBAC can no longer be
forgotten at the HTTP boundary; `first()`/`list()` determinism removes
order-dependent UI flicker on multi-root machines; the manager's registry
API is unit-tested (`manager_tests`).

**Costs:** dashboard.rs dropped from ~2050 to ~840 lines but the *total*
line count grew (~200 lines of signatures/imports) — the price of an
explicit seam. Service functions take `&DaemonState`/`&Arc<DaemonState>`,
which couples the service module to the daemon's state type; a future
`cairn-service` crate extraction would introduce a `ConsoleContext` trait
instead. Not done now — the crate boundary buys nothing until a second
binary actually calls it.

**Verification:** cargo check + clippy clean on the workspace;
207 tests across cairn-cli/core/store/sync/review pass, including the new
`manager_tests` and `service::tests` (traversal refusal, badge mapping).
