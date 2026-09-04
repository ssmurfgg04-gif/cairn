# Cairn v4 — SPEC

Status: **AUTHORITATIVE** (implementation spec for the headless v4 core)
Supersedes: the original placeholder-named brief ("Terra"). Naming per ADR-0002.
Rule: **every decision below is verbatim from the product/engineering brief.** Deviations exist
only where an ADR in `docs/adr/` records them, with rationale. Deviations without an ADR are bugs.

---

## 0. NAMING

The name "Terra" was a placeholder. The project name is **Cairn**. All artifacts rename:
CLI `cairn`, crates `cairn-*`, proto package `cairn.v4`, config/state `~/.cairn/`, metrics
`cairn_*`, daemon ports unchanged. Tenant-scoped storage keys keep the documented `t{tenant}/...`
shape. See ADR-0002 for the full decision record (alternates considered: Strata, Slate, Basalt,
Quarry; rejected for collision, genericness, or scope mismatch).

## 1. PRODUCT DEFINITION

Teams mount project folders via native OS placeholders (Windows CfAPI, macOS FileProvider, Linux
FUSE). A local daemon watches for changes, chunks file content content-defined (FastCDC), hashes
with BLAKE3, uploads only missing chunks directly to object storage via presigned URLs, and
appends file operations to a server-linearized journal. Snapshots are folded from the journal into
Git-style commits/trees. Project files are protected by leases with fencing tokens so two editors
cannot corrupt each other. A 50GB camera file opens scrub-ready in an NLE in <50ms from a local
header cache while the rest streams in.

## 2. HARD INVARIANTS (non-negotiable, tested, alertable)

- **I1 LATENCY**: placeholder → first byte of file header served <50ms (cached), <500ms
  (uncached, edge). Instrumented from day one as `cairn_hydration_first_byte_ms`.
- **I2 INTEGRITY**: a crash at ANY point — client `kill -9` mid-upload, network partition, server
  failover, GC running concurrently — never loses an acknowledged journal append, never leaves a
  ref inconsistent, never materializes a corrupt file. Enforced by deterministic simulation tests
  (§15), not hope.
- **I3 TENANCY**: no byte, hash lookup, lease, or metadata row is ever readable across tenants.
  No cross-tenant dedup, ever. Every DB row and storage key is tenant-scoped.
- **I4 SERVER CLOCK ONLY**: client timestamps are informational. Ordering comes from
  server-assigned journal seq.

## 3. NON-GOALS (do not build; if a design seems to need these, stop and ask)

- No UI of any kind (web, desktop, tray). CLI + localhost gRPC ctl API only.
  *User-mandated exception: a local diagnostics dashboard served by the daemon, built against the
  frozen ctl contract — see ADR-0009. The headless rule stands for everything else.*
- No billing UI (server-side metering counters are in scope; presentation is not).
- No AI/ONNX runtime, no smart-tagging.
- No LAN/P2P sync (design hooks only: chunk store must be shareable in-process).
  *User-mandated exception (2026-09-04, ADR-0017): a P2P swarm transport — rendezvous,
  NAT punching, encrypted relay fallback, peer-first block hydration — ships as an
  overlay on the data plane. It never replaces the cloud control plane (journal/leases/
  cursors stay authoritative), and admission is join-code gated by the host. Review notes
  and the timeline round-trip audit (ADR-0018) are editorial tooling, not a chat network.*
- No OPRF/DupLESS. Encryption tiers per §13 (T3 uses AES-SIV; nothing else).
- No CRDTs. Last-writer-wins per path + conflict copies + leases.
- No QUIC, no mobile clients, no sharing links, no multi-region routing (leave a region column on
  tenants; that's the hook).
- No rsync-style rolling-hash delta (CDC chunking already provides it).

## 4. ARCHITECTURE — THREE PLANES

**CONTROL PLANE** (background jobs, idempotent, resumable, kill-switchable):
GC mark-sweep (14d grace) · journal folding · small-object packing · tiering to B2 · bloom
rebuild · metering rollup · canary.

**METADATA PLANE** (gRPC, stateless, p99 <150ms):
Journal append/watch · snapshots & refs · leases+fencing · BatchExists · upload sessions &
presigning · download URLs · authz · audit log.
Backing store: SQLite-compatible SQL (libsql/D1 in prod; plain SQLite in dev). DDL must be
dialect-portable. No stored procedures.

**DATA PLANE** (client ↔ bucket direct; the API server NEVER proxies blob bytes):
Content-addressed chunks in R2/S3 · presigned PUT with trailing checksums · signed CDN GET URLs
(immutable, Range-capable, 1h TTL, renew-on-403).

## 5. DATA MODEL

### 5.1 Objects (all hashes are BLAKE3-256, hex)

- **CHUNK** FastCDC: min 1MB, avg 4MB (boundary mask 2^22), max 16MB. Immutable.
  Key: `t{tenant}/c/{hash[0:2]}/{hash}`
- **MANIFEST** Sorted list of `(offset u64, len u32, chunk_hash)`. Max 8,192 entries per manifest
  object; larger files fan out into manifest trees (Git-style).
  `manifest_hash` = BLAKE3 of top manifest bytes.
  `file_hash` = BLAKE3(concat of chunk hashes in file order). This construction is frozen
  verbatim (see §6 and ADR-0004) — it must never change silently.
- **TREE** `(mode, name, manifest_hash | tree_hash)`. **NO mtime in the hash input** (mtime churn
  is why Git excludes it).
- **COMMIT** `(tree_hash, parent_commit_hash, author, message, label, snapshot_seq)`.
- **JOURNAL** Server-linearized log per project — rows in DB, NOT objects (§7).
- **REF** `(project, ref_name)` → commit_hash. CAS-updated at fold only.
- **HEADER CACHE** (client-only): first 2MB + last 1MB per pointer, SQLite BLOBs.

### 5.2 Server DDL (portable SQL; dev = SQLite file, prod = libsql/D1)

```sql
tenants(id PK, region, created_at);
users(id PK, tenant_id FK, email, role, created_at);
devices(id PK, tenant_id, user_id, token_hash, scopes TEXT 'sync|admin', revoked INT,
        last_seen, created_at);
projects(tenant_id, project_id, name, next_lease_token INT, fold_seq INT, created_at,
         PRIMARY KEY(tenant_id, project_id));
journal(tenant_id, project_id, seq, request_id, device_id, op BLOB, server_ts INT,
        PRIMARY KEY(tenant_id, project_id, seq));
  UNIQUE(tenant_id, project_id, request_id)  -- idempotent retries
  INDEX(tenant_id, project_id, path, seq)    -- conflict checks
journal_cursors(device_id, project_id, last_seq, PRIMARY KEY(device_id, project_id));
refs(tenant_id, project_id, ref_name, commit_hash, version INT,
     PRIMARY KEY(tenant_id, project_id, ref_name));
chunks(tenant_id, hash, size, tier 'hot|warm|archive', state 'present|deleting',
       last_touched, PRIMARY KEY(tenant_id, hash));  -- authoritative BatchExists KV
manifests(tenant_id, hash, size, entry_count, PRIMARY KEY(tenant_id, hash));
upload_sessions(id PK, tenant_id, device_id, chunk_hashes BLOB, expires_at, state);
leases(tenant_id, project_id, path, device_id, token INT, expires_at,
       PRIMARY KEY(tenant_id, project_id, path));
  -- token from projects.next_lease_token (DB seq, restart-safe)
packs(tenant_id, pack_key, size, state 'building|active', created_at);
pack_index(tenant_id, object_hash, pack_key, offset, len, PRIMARY KEY(tenant_id, object_hash));
trash(tenant_id, project_id, path, deleted_seq, purge_after, manifest_hash,
      PRIMARY KEY(tenant_id, project_id, path));
  -- manifest_hash column added per ADR-0006 (GC reachability of trashed content)
metering(tenant_id, day, bytes_stored, bytes_uploaded, bytes_downloaded,
         PRIMARY KEY(tenant_id, day));
audit_log(id, tenant_id, actor, action, resource, ts, detail);
```

ADR-0006 records all DDL refinements required to make GC/recall/jobs implementable:
`trash.manifest_hash`, `legal_holds` table, `config_flags` (kill switches), `jobs` (progress),
`jobs_leader` (leader lease), `tenants.deep_archive` opt-in.

### 5.3 Client SQLite (`~/.cairn/db.sqlite`, WAL mode, busy_timeout=5000,
single writer task, migrations via `PRAGMA user_version`)

```sql
files(path, project_id, manifest_hash, size, mode, mtime,
      local_state 'synced|dirty|placeholder|pinned|conflict');
outbox(request_id UUIDv7, project_id, op BLOB, state, attempts);  -- pending appends
blobs(hash, size, atime, pinned INT);                             -- local chunk CAS
dir_headers(pointer_hash, head BLOB /*2MB*/, tail BLOB /*1MB*/);
devices(device_id, project_id, last_seq);
leases_local(path, token, expires_at);
meta(key, value);
```

## 6. CHUNKING & HASHING PIPELINE (single pass)

1. **Stable-state gate**: file quiescent 2s (no fs events; size+mtime stable). If a file is
   modified during chunking → discard, re-run (idempotent).
2. **Stream once** (mmap if >100MB): BLAKE3 whole-stream + FastCDC boundaries + per-chunk BLAKE3
   simultaneously.
3. **Extension/content-sniff table** decides compression:
   media (`braw/prores/mxf/r3d/wav/mp4/mov`) → NO compression;
   text-ish/JSON/XML → zstd-3 stored flag;
   NLE project files (`.prproj/.drp/.fcpxmld`) → zstd with per-project dictionary trained on the
   previous version.
4. **Build manifest** (fanout at 8,192 entries). Idempotent: same bytes → same hashes.
5. Property tests must assert: >70% chunk reuse between consecutive NLE auto-saves of the same
   project file; boundary stability under byte insertion (CDC guarantee); golden corpus of real
   save sequences (see §15.3).

**Compression placement (ADR-0004):** chunking runs on RAW bytes; compression is applied at chunk
granularity (per-file policy flag), so chunk hashes and `file_hash` remain a pure function of file
content. Stored object bytes may be compressed; the manifest carries the per-file compression
flag + optional dictionary hash. Media is stored verbatim. This preserves the documented
`file_hash` construction while keeping >70% reuse across auto-saves.

## 7. SYNC PROTOCOL (the journal IS the database)

Two metadata planes. Never CAS a ref on a file save. Never push without cursors.

### 7.1 Journal (sync log)

Append-only, per-project, server-assigned u64 seq. Ops:
- `FileUpsert{path, manifest_hash, size, base_seq}`
- `FileDelete{path, base_seq}`
- `Rename{old_path, new_path, manifest_hash, base_seq}`
- `LeaseEvent{path, kind, device_id}` (informational)

**Idempotency**: client generates `request_id` (UUIDv7); server dedupes. Retries safe.

**Conflict rule (implement exactly)**: `FileUpsert` accepted iff no entry from a DIFFERENT device
has seq > base_seq for the same path. Same-device upserts always supersede. On rejection →
`CONFLICT` result → client writes conflict copy `"name (conflict — {device} — {date}).ext"` and
re-appends for the new path.

Renames are metadata-only ops; never re-chunk.
Deletes are tombstones; trash retained 30d; per-device cursors prevent resurrection on reconnect.
Watch (stream `JournalBatch` from cursor) is a HINT. Cursor replay is the guarantee.
Compaction: entries older than last folded snapshot + 30d are removed. A client whose cursor
predates compaction re-syncs: latest snapshot + local tree diff.

### 7.2 Snapshots (version plane)

Folded from journal when: >5,000 entries OR 24h OR on demand (CLI) OR project close.
Fold = build tree from journal materialization → commit object → CAS ref update (expected
version). CAS happens once per fold, never per save.

### 7.3 Sync state machine (per file; explicit enum, exhaustively tested)

```
clean → dirty → (hash+chunk) → (upload pending) → (outbox append) → synced
```
Any transition may be interrupted. Recovery = WAL replay + outbox resend + BatchExists re-check.
Uploads and appends are idempotent; state machine may always safely re-enter.

## 8. LEASES & FENCING (NLE correctness primitive)

- `Acquire(path, device, ttl=60s)` → `{token (from projects.next_lease_token, a DB sequence —
  survives server restart), expires_at}`. `Renew(jittered)`. `Release`.
- **Enforcement at JOURNAL APPEND**: `FileUpsert`/`Rename` for a leased path must carry the
  current token; stale/expired/mismatched → `STALE_LEASE` error. Leases are advisory; fencing is
  the guarantee.
- Policy: NLE project files (`.prproj/.drp/.fcpxmld/etc.`) auto-acquire on open; media/exports
  never locked (immutable content-addressed model).
- TTL enforcement server-side is a cleanup job; correctness comes from fencing.

## 9. DATA PLANE (client ↔ bucket direct; server never proxies bytes)

### 9.1 Upload

1. `BatchExists(chunk hashes)` → server: bloom filter (negative pre-filter ONLY) → authoritative
   check in chunks table → exact missing set. Cap 10k/batch. **A bloom "maybe present" MUST be
   verified against the KV table. Bloom false positives must NEVER cause a skipped upload.**
2. `CreateUploadSession(missing)` → presigned PUTs to `t{tenant}/c/{ab}/{hash}`,
   `x-amz-checksum-sha256` required (bucket rejects corrupt uploads), TTL ≤1h, no list perms,
   write-scoped only.
3. Upload 4–64 concurrent streams, AIMD concurrency (additive increase on success, multiplicative
   decrease on 5xx/timeout), per-chunk retry with full jitter, resumable at chunk granularity
   across restarts (session rows).
4. `CompleteUpload(receipts: hash, size, etag)` → server HEAD-verifies 10% sample (100% for
   chunks >64MB) → insert chunks rows → upload manifest object (same presign path; it is a <8MB
   object).
5. Client appends `FileUpsert` with request_id. Step ordering may be interrupted at any point and
   resumed (this is I2).

### 9.2 Download

- `GetDownloadUrl(manifest_hash, path)` → signed CDN URL, immutable cache headers, Range-capable,
  1h TTL; on 403 mid-stream, transparently re-sign and resume.
- Hydration path serves head 2MB / tail 1MB from local header cache (<50ms, I1); streams the
  remainder; verifies every chunk's BLAKE3 on ingest (free at 10+GB/s).
- Signed URLs are bearer credentials: strip query strings from all logs/traces.

## 10. FILESYSTEM INTEGRATION (hardest platform first)

- **Windows**: CfAPI (Cloud Filter API) via windows-rs bindings. Placeholder files, hydration
  callback, pin/unpin states. Long paths (`\\?\`), reserved-name sanitization, ADS ignored.
- **macOS**: File Provider framework (Swift shim compiled into the daemon via FFI is acceptable;
  budget for it). Materialization states, eviction, Finder integration. No kext, no macFUSE, NO
  loopback SMB ever.
- **Linux**: FUSE (`fuser` crate).
- **File watching**: OS-native (USN journal on Windows; FSEvents on macOS; inotify on Linux) with
  polling fallback; 2s quiescence debounce before hashing.
- **Filesystem landmines (handle ALL, tested each)**: case-insensitive collisions → conflict copy;
  NFC-normalize stored names, preserve original bytes for display; symlinks stored as symlink
  objects, never followed; hardlinks independent; `._*` AppleDouble and `.DS_Store` (default)
  never synced; mtime alone never trusted (size+mtime heuristic → verify hash when suspicious).
- Pin/unpin exposed via ctl API. Document: NLE media caches belong on local scratch.
- Fallback mode: WinFsp-based passthrough driver on Windows behind a feature flag for NLEs that
  misbehave with CfAPI (test matrix decides; keep the flag).

## 11. LOCAL DAEMON & CTL API (the UI team's contract — freeze it early)

- Single daemon process. Localhost gRPC on `127.0.0.1:17777`, token-authenticated.
- CLI `cairn`: login/logout (device enrollment; tokens in OS keychain via `keyring` crate — never
  plaintext), status, sync, snapshot create/list/restore, pin, unpin, lease ls, recall, doctor,
  gc-shadow-report.
- ctl services: `StatusService`, `ProjectService` (attach/detach roots), `SnapshotService`
  (create/list/restore), `PinService`, `RecallService`, `DiagnosticsService`.
- Document every RPC in `docs/ctl-api.md`. This contract is versioned like the wire protocol.
  UI team builds against it later — treat breaking changes as a bug.

## 12. STORAGE SERVER (Rust: tonic + axum where HTTP needed)

Services: `JournalService`, `SnapshotService`, `LeaseService`, `UploadService` (sessions,
presign, complete), `BatchExists`, `DownloadService`, `CtlAuthService`, `ProjectService`.
Layout: `crates/cairn-server`. Stateless; horizontal scale. Background jobs (§4 control plane)
run as separate idempotent workers with leader lease (DB row) — safe to kill and restart at any
time.

**Object storage layout**
```
t{tenant}/c/{ab}/{hash}                 chunks (immutable)
t{tenant}/o/{ab}/{hash}                 manifests/trees/commits (<8MB)
t{tenant}/packs/{yyyy-mm-dd}-{n}.pack/.idx   packed small objects
```

**Tiering (nightly)**: chunks untouched >90d → copy to B2 → verify checksum → tombstone hot
copy. NEVER tier manifests/trees/commits. Deep Archive is per-tenant opt-in only. RecallService
with progress + ETA.

**Packing (nightly, per tenant)**: objects <8MB only; packs immutable 50–128MB, zstd 3–7; verify
finished pack BEFORE atomically switching pack_index rows in one transaction; epoch guard
prevents GC/pack overlap on the same objects.

**GC**: reachability walk (NOT refcounts) from roots = refs ∪ trash tombstones ∪ in-flight
sessions <7d ∞ legal holds. 14-day grace. Sweep only after shadow-mode report has run clean for a
full beta month. Journal compaction: fold + 30d.

Every table and every storage key is tenant-scoped. I3 is tested (§15).

## 13. SECURITY

- **T1 (default, v1)**: TLS + provider at-rest; dedup by plaintext hash, per-tenant.
- **T2 (v1.1, enterprise)**: envelope encryption — KMS master key → per-tenant DEK (wrapped, in
  DB) → HKDF domain separation per purpose. Rotation re-wraps DEKs.
- **T3 (v2, flag-gated)**: client-side AES-SIV (RFC 5297), key = `HKDF(dek, "chunk-enc" ||
  tenant_id)`; deterministic ciphertext → per-tenant dedup survives E2EE. Document residual risk:
  intra-tenant confirmation attacks.
- **Authn**: device enrollment flow via CLI; PASETO/JWT device tokens, 90d rotation, scopes
  `sync|admin`, revocation on unlink, keychain storage.
- **Presigned URLs**: tenant-prefix-scoped, TTL ≤1h, no list, separate read/write; never logged
  with query strings.
- **Audit log rows for**: authz denials, ref updates, lease takeover, GC sweeps, tiering/recall,
  admin actions.

## 14. ERROR TAXONOMY & RETRY MATRIX (encode as one table in code + docs)

| Class | Behavior | Examples |
|---|---|---|
| Retryable (auto, full jitter, max 5) | idempotent ops only | 429/5xx/timeouts/network |
| Fatal-client (stop, surface via doctor) | | manifest verification failure, unexpected local CAS corruption (auto re-download), auth revocation |
| Conflict-class (explicit resolution) | | `CONFLICT` (→ conflict copy), `STALE_LEASE` (→ surface to user), `REF_CAS` (→ retry fold, never lose writes) |
| Server-class | respond precisely, never 500-as-catchall | every error carries code + retryability hint in proto |

## 15. TESTING (this section is a gate, not a suggestion)

- **15.1 Deterministic simulation (I2's enforcement)**: madsim or shuttle. Simulated clock,
  network (partition/latency/loss), filesystem, RNG. Nightly: ≥1,000 randomized schedules of:
  2–4 devices, kill -9 at every state transition, lease expiry mid-save, GC concurrent with
  uploads, fold concurrent with appends. Assertions: (a) every acknowledged append survives every
  crash; (b) all live devices converge to identical state; (c) no corrupt manifest/file ever
  materializes; (d) GC never deletes a reachable object (shadow verify pass).
  *In-house scheduler chosen over madsim/shuttle — see ADR-0008.*
- **15.2 Property tests (proptest)**: chunker stability, manifest round-trip, conflict rule truth
  table, journal idempotency (duplicate request_ids), fencing (stale token rejected),
  bloom-negative pre-filter can never cause a missed upload (fuzz: bloom mutated adversarially →
  uploads still complete).
- **15.3 Golden corpus**: real-world NLE save sequences (Premiere `.prproj` auto-saves, Resolve
  `.drp` grade changes, BRAW/ProRes samples, MXF, WAV). Checked into LFS. Assert: chunk reuse
  ratios, hydration timing, dedup ratios.
- **15.4 Fault injection harness**: scripted kill -9 at each numbered step of §9 upload/download;
  assert recovery with zero data loss and zero duplicate visible files.
- **15.5 Fuzz**: manifest parser, pack index parser, journal op deserializer (cargo-fuzz, 10-min
  CI runs).
- **15.6 CI gates**: `cargo clippy -D warnings`; no unwrap/panic in prod paths (thiserror
  everywhere); coverage ≥85% on cairn-core/cairn-sync; all milestones have acceptance tests (§19)
  that must pass before merge.

## 16. OBSERVABILITY & SLOs (from W1, not W8)

- tracing + OpenTelemetry end-to-end (trace_id across client→server→bucket calls); Sentry for
  client crashes.
- Metrics: `cairn_hydration_first_byte_ms` (I1 — alert forever), `sync_propagation_p95` (<5s),
  `journal_append_p99` (<150ms), upload success rate, AIMD concurrency distribution, GC shadow
  violations (must be 0), chunk reuse ratio, bloom hit/miss, canary loop result.
- PROD CANARY: headless client runs upload→edit→sync→verify→recall every 5 min; page on failure.
- KILL SWITCHES (flags): packing, tiering, delta-fold, compression, placeholder-driver mode
  (native ↔ WinFsp fallback on Windows).
- Ops runbooks: DR (bucket loss, metadata PITR restore), GC shadow report, lease server restart,
  recall from cold.
- *OTel exporter + Sentry wiring: see ADR-0007 (trace_id propagation is in scope from W1; OTLP
  export is flag-gated; Sentry integration point stubbed).*

## 17. TOOLCHAIN & REUSE (port, don't invent)

Crates: tokio, tonic+prost, rustls, hyper, rusqlite (client), sqlx (server), blake3, fastcdc (or
port restic's chunker — study it first either way), zstd, notify, fuser, windows (CfAPI via
Win32::Storage::CloudFilters), keyring, thiserror/anyhow, tracing+opentelemetry, proptest,
cargo-fuzz, aws-sdk-s3, uuid (v7), serde, ed25519/paseto tokens.

Reference implementations to STUDY/PORT (record in THIRD_PARTY.md with license):
restic (BSD-2: chunker, packing, crypto patterns) · kopia (Apache-2: dedup, parallel upload,
packing) · syncthing (MPL-2: sync engine + conflict handling) · rclone (MIT: transfer/backoff/
presign patterns) · git (packfile+idx format spec — port format, versioned byte) · SQLite (WAL
discipline). Copied code gets provenance headers. Prefer crates over copy-paste.

Monorepo crates: cairn-proto · cairn-core (chunk/hash/manifest, pure, heavily tested) ·
cairn-store (local CAS+SQLite) · cairn-sync (engine) · cairn-fs-win · cairn-fs-mac ·
cairn-fs-linux · cairn-server · cairn-cli · cairn-sim · cairn-x.

Tooling: rust-toolchain.toml, just or make, GitHub Actions, rustfmt, clippy pedantic on core
crates.

Deviations recorded in ADR-0005 (storage backends + presigning: internal SigV4 signer instead of
aws-sdk-s3 dependency; feature-flagged backend trait keeps S3/R2/B2 interchangeable).

## 18. WORKING AGREEMENT

- Produce `docs/SPEC.md` (all decisions verbatim from this prompt) and `docs/adr/` (one ADR per
  significant decision, dated). If you deviate from this spec for ANY reason, stop and write the
  ADR first; deviations without ADRs are bugs.
- Ask before inventing. If a section is ambiguous, choose the option consistent with I1–I4 and
  record it.
- No UI. No feature creep. Every milestone ends with tests green + a working `cairn doctor` that
  verifies it.
- All timestamps: UTC i64 millis, server-authoritative. All paths: UTF-8 NFC.
- Protocol versioning: package `cairn.v4`, reserve field numbers 100–199; pack format carries
  version byte; client/server negotiate min/max at connect.

## 19. MILESTONES (each has acceptance criteria — do not start Mn+1 before Mn passes)

- **M0 FOUNDATION (wk 1)**: monorepo, CI, proto v4 schemas, cairn-core with BLAKE3+FastCDC
  pipeline + manifest fanout + property tests. AC: property tests green; chunk-reuse property
  >70% on synthetic project-file save sequences; corpus harness runs.
- **M1 LOCAL CORE (wk 2)**: local CAS store, SQLite state, WAL, outbox, ctl daemon skeleton +
  `cairn status`/`doctor`. AC: kill -9 at any point → WAL replay → zero state loss (fault
  harness v1).
- **M2 SERVER METADATA (wk 3)**: journal (append/idempotency/conflict rule), cursors, watch,
  leases+fencing (DB-sequence tokens), device auth, DDL above. AC: conflict truth table 100%
  pass; duplicate request_id deduped; stale fencing token rejected; restart of metadata service
  loses nothing.
- **M3 DATA PLANE (wk 4)**: presigned upload sessions, checksums, BatchExists with bloom-negative
  prefilter (authoritative KV), CompleteUpload verification, signed CDN downloads with renewal,
  AIMD concurrency. AC: end-to-end 5GB file, kill -9 mid-upload, resume, byte-identical
  round-trip; adversarial-bloom fuzz proves no skipped upload ever.
- **M4 SYNC ENGINE (wk 5)**: full state machine, renames, tombstones/trash, conflict copies,
  folds→snapshots→CAS refs, two-device sync. AC: sim suite (15.1) 1,000 schedules green; two
  laptops converge; conflict copy path exercised in sim.
- **M5 FILESYSTEM (wk 6–7)**: CfAPI (Windows) first, File Provider (macOS) second, FUSE third;
  header cache + hydration + pin/unpin; fallback WinFsp flag. AC: I1 measured <50ms cached /
  <500ms uncached on test files (mp4 moov-at-end, moov-at-start, BRAW, MXF, WAV); DaVinci Resolve
  + Premiere open/scrub/save matrix passes on Windows+macOS with two clients sharing one project
  file (lease prevents divergence).
- **M6 STORAGE OPS (wk 8)**: packing job + atomic index switch + verify, GC reachability + grace
  + epoch guard (shadow mode), tiering job + RecallService, metering counters, canary client. AC:
  GC shadow zero violations over 10k synthetic churn ops; pack switch atomic under kill -9;
  recall round-trip works from B2.
- **M7 HARDENING (wk 9–10)**: fuzz targets, chaos scenarios, kill switches verified, rate
  limits/quotas, audit log, SLO dashboards, DR runbook, docs/ctl-api.md frozen, SPEC.md final.
  AC: 24h soak (canary + sim) with zero I2 violations; all flags flip without restart harm.
- **M8 BETA READY (wk 11+)**: onboarding via CLI end-to-end, `cairn doctor` diagnostics,
  THIRD_PARTY.md complete, beta runbook for 5 studios.

## 20. GLOBAL DEFINITION OF DONE

A change is done when: tests pass in CI, property/fuzz gates green, ADR written, ctl-api.md
updated if the contract moved, THIRD_PARTY.md updated if code was ported, and `cairn doctor`
reports healthy after the change. The two questions every design argument resolves against
remain:

- **I1**: "I opened a 50GB BRAW in Resolve — how long until I can scrub?"
- **I2**: "A crash happened at any point — did we lose an acknowledged save or corrupt a project
  file?" (Answer must always be: **no**.)

---

## 21. ADR INDEX

| ADR | Decision |
|---|---|
| ADR-0001 | Architecture overview: three planes, journal-as-database, headless core |
| ADR-0002 | Project name: Cairn (replaces placeholder "Terra") |
| ADR-0003 | FastCDC implemented in-house after restic study (Gear table, mask 2^22) |
| ADR-0004 | Compression at chunk granularity; file_hash frozen over raw chunk hashes |
| ADR-0005 | Object-store trait + internal SigV4 presigner (aws-sdk-s3 excluded from core deps) |
| ADR-0006 | DDL refinements: trash.manifest_hash, legal_holds, config_flags, jobs, jobs_leader, tenants.deep_archive |
| ADR-0007 | Observability: trace_id propagation now; OTLP export + Sentry behind flags |
| ADR-0008 | Deterministic simulation: in-house seeded scheduler + fault hooks (madsim/shuttle too heavy for v4 core) |
| ADR-0009 | Local diagnostics dashboard served by daemon at :17778 over loopback-only HTTP gateway (user-mandated UI exception; headless rule otherwise intact) |
| ADR-0010 | Error taxonomy: single retry-class table, codes carried in proto ErrorDetail |
| ADR-0011 | Device tokens: PASETO v4.public via pasetors crate (ed25519), 90d rotation |
| ADR-0012 | Rename + object formats: metadata-only renames, upload staging `.cairn.part` → atomic promote |
| ADR-0013 | zstd dictionary compression cross-device (per-tenant trained dict, chunk-granular apply) |
| ADR-0014 | NLE collaboration concurrency: leases are the correctness primitive, phases for sync passes |
| ADR-0015 | OTIO/FCPXML three-way timeline merge (deterministic, identity ladder, conflict surfacing) |
| ADR-0016 | Clicky-Clicky onboarding: install.ps1 + system tray (tray never links the engine) |
| ADR-0017 | P2P swarm transport: signal rendezvous, NAT punch, encrypted relay fallback, peer-first hydration — join-code gated, cloud plane stays authoritative (user-mandated §3 exception) |
| ADR-0018 | Frame-anchored review notes (content-derived ids, deterministic 3-way merge, CSV interop) + timeline round-trip audit (frame-exact drift, effect inventory, severity contract) |
