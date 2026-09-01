# Write-back design (WO6-1) — from read-only to a real sync engine on Windows

Status: implemented in this round (gates listed at the bottom; HUMAN-GATEs honest).
Read half: proven (I1 = 16.32 ms through the real cldflt driver, round 4). This doc
covers the write half: what happens when an editor SAVES into the sync root.

## 0. Principles

- **No invented ABI**: every call sequence below follows the CfAPI contract
  (Microsoft CloudFilter docs) and, where the docs are ambiguous, the proven
  open-source implementation (nextcloud/desktop vfs/cfapi, AGPL — credited). Any
  value windows-rs forgot to export is defined locally WITH the cfapi.h citation.
- **Reuse the proven engine**: chunking, delta upload, journal append, fencing,
  echo suppression, crash-safe outbox — all already exist in cairn-core/cairn-sync
  and are byte-budget-tested (round 4). Write-back ADDS platform glue; it does NOT
  reimplement sync.
- **Kill -9 anywhere is survivable** (I2): dirty state is durable in the client
  store BEFORE the ack; journal appends are idempotent by `request_id`.

## 1. The five write-path states of a file

| State | On disk | Who can move it here |
|---|---|---|
| `placeholder` (dehydrated) | metadata only, bytes on server | attach bulk-populate (WO6-2), dehydration |
| `hydrated + in-sync` | full bytes, equal to server head | hydration (read), save-back + in-sync set |
| `hydrated + dirty` | full bytes, differ from server head | user write; filter clears in-sync automatically |
| `new local` | full bytes, no server object yet | user creates file in root |
| `deleted` | gone | user delete (tombstone + 30-day trash, server-side) |

The provider tracks states in the client store (SQLite, same DB the engine already
uses). The filter driver tracks its OWN in-sync bit; Cairn keeps it true only while
the file is provably synced (CfSetInSyncState after a successful push).

## 2. Write-open: CF_CALLBACK_TYPE_VALIDATE_DATA (hydrate-before-write)

When a process opens a placeholder with write intent, the filter calls
`VALIDATE_DATA` (params: `Flags, RequiredFileOffset, RequiredLength`).

Provider behavior (callback thread, must not block the engine loop):

1. **Self-PID guard** — same as FETCH_DATA (BLOCK_SELF_IMPLICIT_HYDRATION is NOT
   set for the write connection? IT IS — the guard exists because the daemon itself
   reading its own root would deadlock; the filter rejects the hydration otherwise).
2. Ask the source: `write_open_validate(path, identity) -> ValidateOutcome`:
   - `Current { fully_hydrated }` — placeholder identity == server head:
     - if fully hydrated → `ACK_DATA` with `DataRequired = 0` (open proceeds on
       local bytes; no network on save = the studio-critical fast path);
     - if dehydrated → `ACK_DATA` with `DataRequired = 1` → the filter drives
       FETCH_DATA; we serve bytes exactly like the read path (hash-verified,
       block-aligned). **Hydrate-before-write is therefore the FETCH machinery
       reused, not a new path.**
   - `Stale` — server head moved (another device saved): respond `DataRequired = 1`
     AND record `conflict_expected`; the next scan folds the divergence into the
     conflict-copy rule (§7.1) — v1 does NOT try to merge.
   - `Offline` — the source cannot reach the server AND the file is dehydrated:
     complete with a clear failure (`STATUS_CLOUD_FILE_NOT_IN_SYNC` equivalent:
     `STATUS_UNSUCCESSFUL` + logged reason). This is the honest v1 rule from the
     work order: **writes requiring hydration fail loudly offline; writes to
     already-hydrated files succeed and buffer locally.** Explorer shows the error
     instead of hanging the app.
3. ACK via `CfExecute(CF_OPERATION_TYPE_ACK_DATA)` with
   `CF_OPERATION_ACK_DATA_FLAG_DATA_REQUIRED` (= `0x1`; windows-rs 0.58 exports only
   `..._FLAG_NONE`, the value comes from the cfapi.h SDK header — cited, not guessed).
   ParamSize uses the same `offsetof(union)+sizeof` ABI rule proven in round 4.

## 3. Write-close: detecting the modified placeholder (dirty)

windows-rs 0.58 exposes no `NOTIFY_WRITE` callback; the proven pattern (nextcloud)
is **FILE_OPEN_COMPLETION + FILE_CLOSE_COMPLETION bookkeeping**:

- `NOTIFY_FILE_OPEN_COMPLETION` (params `OpenCompletion.Flags`): if the open was
  with write access (flag `CF_CALLBACK_OPEN_COMPLETION_FLAG_MODE_WRITE...` when
  exported; otherwise detect via the store's hydration state + later size/mtime
  check) AND the path is a project file, auto-acquire a **lease** (§5).
- `NOTIFY_FILE_CLOSE_COMPLETION`: call `source.mark_dirty(path)`. The source does
  NOT trust the notification blindly: it compares size+mtime against the journaled
  row (the SAME echo-suppression predicate proven in round 4 — size-preserving
  edits are caught by the periodic reconcile sweep). If it differs → row becomes
  `dirty` in the store (durable BEFORE the callback returns) and the filter's
  in-sync bit is already cleared by the filter itself.

Why not hash-on-close? Hashing a 4 GB file in a callback stalls the editor's next
open. Dirty marking is O(1); chunking happens later in the engine loop.

## 4. Save-back: re-chunk, delta upload, journal append, in-sync

The engine's existing push path (unchanged, byte-budget-tested):

1. Scan sees the `dirty` row → chunk the file (fine profile `CHUNK_*_FINE` for
   transform-active content; gzip normalization for container payloads — round-4
   proven: `.blend`-class files re-chunk on the INNER payload, a 512-byte edit
   stays a 64–256 KB delta instead of a 4 MB media chunk).
2. `BatchExists` → only missing chunks upload (presigned PUTs) → **delta upload,
   reuse measured not assumed** (gate W6 asserts the byte budget).
3. `RegisterManifest` + journal `Append` with the lease **fencing token** in
   `AppendRequest.lease_token` (server rejects stale tokens — M2 proven; gate W4
   carries the token through the save path).
4. On ack: row → clean, `CfSetInSyncState(IN_SYNC)` via a protected handle
   (`CfOpenFileWithOplock`) — Explorer badge clears exactly when the cloud has the
   bytes, never before.
5. New files in the root (created by the editor, not by Cairn): the scan ingests
   them, pushes, and — because they must become evictable placeholders — converts
   them with `CfConvertToPlaceholder` (`CF_CONVERT_FLAG_MARK_IN_SYNC |
   ENABLE_ON_DEMAND_POPULATION`), identity = manifest hash, exactly like
   attach-created placeholders (gate W2).

## 5. Leases on project-file open (fencing)

- Which files: extensions configurable; v1 default set is the NLE project-file
  family (`.prproj .drp .nce .avp .veg .blend .aep .fcpxmld` — av/med files are
  NOT leased; leasing is for the small contentious head files).
- When: `NOTIFY_FILE_OPEN_COMPLETION` with write intent on a leased path →
  `source.acquire_lease(path)` → server token (restart-safe DB seq). The token
  rides every journal append until the file closes (`NOTIFY_FILE_CLOSE_COMPLETION`
  → release). Stale tokens (lost lease) fail the append with `STALE_LEASE` →
  the row stays dirty and surfaces as a conflict (v1 never silently overwrites).
- Crash safety: leases expire server-side (60 s TTL + renewal); a killed daemon
  cannot fence a file forever.

## 6. Deletes and tombstones

`NOTIFY_DELETE` (pre) → `source.note_delete(path)` records the intent; the engine
pushes a `FileDeleteOp` tombstone (journal is the source of truth). Server-side:
tombstones are retained **30 days** (trash window) — restore is a future ctl
feature (WO6-3 snapshot restore covers the data path). Device B's pull applies
the delete; placeholders vanish (gate W3; Explorer-recycle-bin behavior itself is
NOT claimed — HUMAN-GATE).

## 7. Offline behavior (explicit, honest v1)

- Writes to **already-hydrated** files: succeed; bytes land locally; rows dirty;
  sync on reconnect (AIMD backoff unchanged). The filter's in-sync bit stays off
  until the push ack — Explorer tells the truth.
- Writes requiring **hydration** while offline: fail with the clear error above.
  No partial content is ever written to a placeholder from a stale local cache
  (I2: never serve unverified bytes).

## 8. Crash matrix (gate W5)

Outbox semantics make `kill -9` between ANY steps safe: dirty rows persist in
SQLite before the callback acks; `request_id` (UUIDv7) deduplicates the journal
append; scan re-detects divergence after restart. Gate W5 proves the specific
window work order named: dirty-marked → crash → restart → push → journal has
EXACTLY ONE upsert for the path, byte-identical.

## 9. Gates (CI windows-latest, extending windows-cfapi-roundtrip)

| Gate | Proven in CI | Notes |
|---|---|---|
| W1 edit hydrated placeholder → byte-identical new version on device B | ✅ | child process writes; two sources; BLAKE3 |
| W2 new file in root → syncs, journal exactly 1 upsert, becomes placeholder | ✅ | CfConvertToPlaceholder |
| W3 delete → tombstone + trash, gone on device B | ✅ (journal semantics) / 🟨 recycle-bin UX | HUMAN-GATE: Explorer flow |
| W4 leased save carries fencing token; stale token rejected | ✅ | server M2 suite + token threading |
| W5 kill -9 between write-detect and journal-ack → resume, zero duplicate paths | ✅ | outbox request_id + durable dirty row |
| W6 byte-budget: edit uploads ONLY changed chunks | ✅ | measured delta vs 8 MiB base |
| Explorer badge, Notepad save, NLE matrix on a studio box | 🟨 HUMAN-GATE | listed in STATUS.md, not claimed |
