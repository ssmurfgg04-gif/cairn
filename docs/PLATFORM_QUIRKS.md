# Cairn — Windows & platform quirks journal

Start today, append forever. Every entry here was PAID FOR in debug time;
the fix (if any) is cited so the next person doesn't rediscover it. Quirks
are facts about the platform as we found it, not opinions.

## Windows / CfAPI (cldflt)

### W1. The ParamSize union is 8-aligned on x64 — offsetof+sizeof or E_INVALIDARG
`CF_CALLBACK_PARAMETERS` carries a `union { UInt32, Int32, UInt64, ... }` tagged
by `ParamSize`. windows-rs exposes the fields, but computing the parameter
address as `base + 4` (the 32-bit size) hands the filter's union read the wrong
bytes and every call fails with E_INVALIDARG. On x64 the union sits at offset 8.
**Fix:** compute `offsetof(union) + sizeof(active_field)`; see
`cairn-fs-win/src/cfapi.rs` (WO2 port, credited nextcloud/desktop cfapiwrapper).

### W2. The callback-table sentinel is CF_CALLBACK_TYPE_NONE (0xFFFFFFFF), not 0
`CfConnectSyncRoot` rejects a registration table terminated with
`Type: 0` and returns E_INVALIDARG. The SDK's sentinel is
`CF_CALLBACK_TYPE_INVALID = 0xFFFFFFFF`, which windows-rs spells
`CF_CALLBACK_TYPE_NONE = -1`. A 0-terminated table compiles and fails at
runtime. **Fix:** `cfapi.rs` WO2 commit aa9e9ae.

### W3. CfConnectSyncRoot rejects `\\?\`-prefixed roots; CfRegisterSyncRoot tolerates them
Registering the sync root with a `\\?\C:\...` path succeeds; the subsequent
`CfConnectSyncRoot` on the SAME path fails. The filter wants plain DOS paths
for the connection. **Fix:** register/connect both use the plain form.

### W4. Self-implicit hydration deadlocks the provider — always block it
A provider process that touches its own placeholder re-enters its own
FETCH_DATA callback and hangs. Set `CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_
HYDRATION` and read served bytes in a CHILD process (the WO2 round-trip
test's probe design exists because of this).

### W5. `bitnami/minio` was chosen partly by accident — and GHA service containers drop image CMD
GitHub Actions service containers run the image ENTRYPOINT but NOT its CMD.
`bitnami/minio`'s wrapper entrypoint defaulted to starting the server, which
masked the quirk for months; the official `minio/minio` image runs bare and
prints USAGE. When `bitnami/minio:latest` vanished from Docker Hub (2026
catalog reshuffle), the "fix" to the official image exposed the quirk.
**Fix:** run the pinned `dl.min.io` binary as a plain step (ci.yml
`s3-wire-conformance`), health-polled from the runner.

### W6. windows-rs 0.58 has no `Win32::Foundation::ULARGE_INTEGER` export
GetDiskFreeSpaceExW's ULARGE_INTEGER out-params are flattened to `*mut u64`
in windows-rs 0.58 signatures — there is no ULARGE_INTEGER type to import at
`windows::Win32::Foundation::ULARGE_INTEGER`, and the docs' struct form
misleads. **Fix:** `cairn-store/src/eviction.rs` uses plain `u64` locals
(verified against the vendored crate source, not memory).

### W7. Filter NormalizedPath ≠ your root path (prefix + case)
Callbacks deliver NormalizedPath that can differ from the registered root in
both PREFIX (NT namespace forms `\\?\`, `\??\`, `\\.\`) and CASE (8.3 short
names like `RUNNER~1` vs the long form). A naive `strip_prefix` silently
swallows every close/delete notification: `rel_path` mismatches, the row
lookup fails, the handler returns, and the file NEVER dirty-marks.
**Fix:** `win_attach.rs::rel_path` strips the prefixes and matches the root
case-insensitively but slices the ORIGINAL string (row keys keep registered
casing — SQLite TEXT is case-sensitive). Harness equivalent: key dirty
markers/leases by FILE NAME (cfapi_roundtrip `name_key`).

### W8. Close-notification delivery for modified placeholders — open questions welcome
First real run of the WO6-1 gates (2026-09-01): NOTIFY_FILE_OPEN_COMPLETION
fires reliably (lease test passes) but the dirty marker written in
NOTIFY_FILE_CLOSE_COMPLETION never appeared — even with 10s of polling. Two
hypotheses: the filter doesn't deliver close for a placeholder that was
modified (hydrated-then-edited), or it delivers with a path form whose stat
fails (the old handler silently returned on stat error). The harness now
prints `opens/closes/deletes` counters + last paths on failure so the CI log
decides. **Belt-and-braces either way:** the reconcile sweep (size+mtime
walk) is the authoritative dirty-detect backstop; the close hook is an
optimization, never the guarantee.

### W9. Shared CI runners make one-shot latency gates meaningless
windows-latest I1 (first 2 MiB through the CfAPI callback): 16.32 ms on a
calm runner (2026-08-31) vs 606/934/529 ms across three fresh placeholders
on a contended one (2026-09-01). A one-shot 50 ms gate fails on contention,
not regression. **Fix:** best-of-N with all samples printed + a two-tier
policy: CI asserts the structural ceiling (800 ms), the 50 ms gate is the
studio-hardware human gate. Same lesson shaped the attach-acceptance 4a
visibility window (30 s flaky → 120 s window / 30 s threshold + bounded
sweep budget so the sweep stops starving the pull loop).

### W10. PARTIAL population policy + unregistered FETCH_PLACEHOLDERS = 60 s freeze, then os error 426
Registering the sync root with nextcloud's policy set (we copied
`CF_POPULATION_POLICY_PARTIAL`, cfapiwrapper.cpp:887) but WITHOUT registering
`CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS` compiles, connects, hydrates, and
validates fine — then the FIRST app-side create of a NEW file in the root
(`std::fs::write`) hangs for the filter's fixed 60 s callback timeout and
fails with `ERROR_CLOUD_FILE_REQUEST_TIMEOUT` (426). With PARTIAL population
the filter treats "an attempt to open a file underneath the directory" as a
population query; a callback type absent from the table is not "skipped" —
the provider just never answers, and nothing else times it out. This sat
latent for two CI rounds because the write-back gate died earlier (W5) and
create is the first user-side operation on a not-yet-known path.
**Fix:** register FETCH_PLACEHOLDERS + CANCEL_FETCH_PLACEHOLDERS in BOTH
connect tables and answer via `CfExecute(CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS)`
with `DISABLE_ON_DEMAND_POPULATION` (empty answer for v1: attach pre-creates
every placeholder); additionally set
`CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION` on created file
placeholders so the filter never waits on population for them. Cited from
nextcloud/desktop cfapiwrapper.cpp:656-657 (table), :887 (same PARTIAL
policy), :183 (the DISABLE flag on the transfer), :1095 (the flag on file
entries). See `cfapi.rs::transfer_placeholders`. Test gate: the W2 create in
`cfapi_roundtrip.rs` now completes in-process as the regression tripwire.

## S3 / wire

### S1. `x-amz-checksum-sha256` is BASE64 on the wire — hex gets 400 "Invalid checksum provided"
Session presigns are HOST-ONLY (checksum not bound into SignedHeaders), and the
daemon attaches `x-amz-checksum-sha256` itself. We computed that header as
lowercase HEX — every presigned PUT returned `400 InvalidArgument: Invalid
checksum provided` and soak S1 never converged, while the wire-conformance job
stayed green because its checksum-bound check signs the SAME hex value into the
signature (signed values skip MinIO's base64 decode; unsigned values don't).
Proven against the pinned MinIO (RELEASE.2024-06-13T22-53-53Z): hex header →
400; correct base64 → 200; WRONG base64 → 400 `XAmzContentChecksumMismatch` —
so the bucket still rejects corrupt uploads with a host-only presign, no
checksum-bound session extension needed for that property. **Fix:** send
base64 (cairn-core `b64_encode` of the raw SHA-256); the dev local-Fs verifier
accepts base64 + hex. Diagnosability lesson: a bare "HTTP 400" cost two CI
round-trips — presign failures now carry the S3 error XML body
(plane_grpc put_presigned). Diagnosed 2026-09-01.

## Linux

### L1. GitHub Actions ubuntu runners allow `sudo` — use it for cold-cache honesty
True cold-fetch numbers need `echo 3 > /proc/sys/vm/drop_caches`; hosted
runners permit passwordless sudo. The soak escalates when it can and PRINTS
when it cannot (never silently fakes cold).

## SQLite / data

### D1. Case-sensitive TEXT joins on Windows paths
`journal.path` / `files.path` compare case-sensitively (BINARY collation).
Any "normalize to lowercase for matching" shortcut in the FFI layer breaks
row lookups while appearing to work for equality checks. Normalize at the
BOUNDARY, store the original.

### D2. Idempotency keys must be content-derived, not random (WO6-4 soak finding)
Random UUIDv7 request_ids make the server's UNIQUE(request_id) dedup
useless against racing enqueues: the watcher and the scan both enqueued a
fresh file, the server accepted BOTH as distinct (8 duplicate journal
paths after a mid-push kill -9). **Fix:** `ids::request_id_for` = 
`req-<blake3(tenant|project|path|manifest|size|mtime_millis)>` — same edit
dedups; a re-save (new mtime/manifest) is a new, legitimate entry, so
A→B→A undo arcs stay correct. A pure content hash (no mtime) would have
collapsed the A→B→A arc into two entries and broken replay.

## CI methodology

### C1. Compile-error cascades mask whole layers
Three Windows bugs (test compile, cairn-store eviction, cairn-cli
win_attach) existed SIMULTANEOUSLY because each CI run died at the first
one. A fix released green only reveals the next layer. Budget for cascade
iterations on platform-gated code, and cross-check what you can locally
(stub-crate type-checking of Windows-gated test files caught 3 additional
errors before any CI round trip).

### C2. `cargo check --workspace` on Linux proves nothing about Windows code
Everything under `#![cfg(windows)]` is invisible to every Linux leg
(check, clippy, test, coverage). The windows-macos-compile job exists
because of this; keep its crate list current (it caught win_attach only
after cairn-cli was added to it).
