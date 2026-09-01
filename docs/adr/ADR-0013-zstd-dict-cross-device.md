# ADR-0013: Per-project zstd dictionaries become content-addressed CAS objects, fetched on demand at hydration

Date: 2026-09-01 · Status: Accepted

## Context

SPEC §6 and ADR-0004 give NLE project files (`prproj`, `drp`, `avp`, `veg`, `aep`, …)
per-chunk zstd compression with a per-project dictionary trained on the previous file
version. Today (post-WO6-5 audit) the machinery is:

- `cairn_core::compress::train_project_dict` trains on the previous version (legacy ZDICT
  trainer, opportunistic; falls back to a deterministic raw-prefix dictionary — the first
  64 KiB of the previous version — when the trainer rejects the sample ratio).
- `cairn_core::compress::DictRegistry` is an **in-memory, per-device** map
  (project_id → dict). The engine consults it at push time
  (`cairn-sync/src/engine.rs`), training on first push when empty.
- The manifest (frozen v1/v2 format) records the per-file compression flag **and
  `dict_hash`** — but the dictionary **bytes** never leave the training device. They are
  not chunked, not uploaded, not referenced anywhere the receiving device can reach.

**The gap (known since round 4, honest in STATUS):** a file pushed by the device that
trained the dictionary cannot be hydrated by any other device —
`decompress_chunk(…, ZstdDict, dict=None)` attempts a plain zstd decode and fails loudly.
No silent corruption, but the failure is per-file and permanent until the user intervenes.
The container-normalization path sidesteps this only for gzip-wrapped files (inner payload
chunks are plain zstd-3); native `.prproj`/`.avp`/`.veg` files stay dict-compressed.

This is a beta blocker for the multi-device promise: studio 2 opens a project that studio
1 pushed and every dict-compressed file fails hydration.

## Decision

1. **Dictionaries are first-class, immutable, content-addressed objects.** A trained
   dictionary is stored in the CAS under the tenant-scoped dict key prefix
   (`t{tenant}/d/{dict_hash}`), exactly like chunks and manifests. Its address is the
   BLAKE3 of its bytes — the same `dict_hash` the manifest already records. No manifest
   format change is needed; the pointer exists in the frozen format today.

2. **Push path:** when the engine trains (or reuses) a dictionary for a file's manifest,
   it additionally CAS-puts the dict bytes and includes the dict hash in the upload
   batch (`batch_exists` treats dicts like chunks: absent → upload). Dict upload rides
   the same session/AIMD machinery as chunk upload; a dict upload failure fails the
   push (same contract as a chunk upload failure — I2).

3. **Hydration path:** before decompressing any chunk of a manifest whose
   `compression == ZstdDict` and `dict_hash` is `Some`, the hydrator ensures the dict is
   in local CAS (present → reuse; absent → fetch via the normal download path,
   hash-verified like every other object). A dict fetch failure is a retryable
   UNAVAILABLE; a dict **hash mismatch** is loud corruption handling (same as a chunk
   hash mismatch).

4. **The stored dict object is the single source of truth.** Devices never re-derive a
   dictionary from synced data. Rationale: the legacy ZDICT trainer is sensitive to
   sample layout and zstd version — two devices can produce different dicts from the
   same previous version, which would silently fork the compression namespace for a
   project. Re-derivation is only deterministic for the raw-prefix fallback, and relying
   on that alone would forfeit the trainer's ratio advantage. Content-addressing makes
   the question moot: whatever bytes the trainer produced are the object; the hash
   matches or it doesn't.

5. **Registry semantics unchanged locally.** `DictRegistry` stays an in-process cache
   backed by local CAS (hit → serve; miss → train on push / fetch on hydrate). The
   registry is an optimization, never a correctness authority.

6. **Degradation ladder (unchanged, now explicit):**
   - No previous version (< 1 KiB sample) → train returns None → plain zstd-3
     (`dict_hash` recorded as None). Cross-device safe by construction.
   - Trainer rejects samples → deterministic raw-prefix dict (still a synced object).
   - Container-normalized files → inner payload, plain zstd-3, no dict (existing rule).

## Alternatives considered

- **Server-side dictionary service** (metadata-plane endpoint returning project dicts):
  rejected — the data plane is direct client↔bucket (§9); the API server never proxies
  bytes, and dicts are bytes. Routing them through metadata would re-introduce a blob
  path through the one plane that must stay lean, plus a new ctl surface and auth scope.
- **Deterministic re-derivation on every device** (dict = f(previous version chunks)):
  rejected — see decision 4. The legacy trainer's output is not portable across zstd
  versions/platforms; pinning the trainer would couple hydration correctness to a
  zstd build, and the frozen-identity discipline of ADR-0004 exists precisely to avoid
  compressor-version-dependent identities.
- **Ban ZstdDict until post-beta** (force zstd-3 for all NLE files): rejected — the
  dictionary is where the win is for autosave deltas of structured project text; the
  fix is small (one object type, one fetch) and rides existing machinery end to end.
- **Embed dict bytes in the manifest object**: rejected — manifests are frozen-format,
  small, and frequently parsed on hot paths; inflating them with up-to-64-KiB dict
  payloads (and re-hashing manifests whenever a dict retrains) breaks the manifest
  hash's meaning as a pure file-content identity.

## Consequences

- Every device can hydrate every dict-compressed file: fetch-by-hash gives device B the
  exact bytes device A compressed with, so the loud-failure gap closes with zero new
  wire formats.
- CAS grows a third object class (`d/` prefix); GC must treat dicts as live while any
  manifest references their hash (mark phase walks manifest `dict_hash` — same rule as
  chunk reachability, one more edge).
- Push latency on first NLE push grows by the dict upload (≤ 64 KiB) — negligible vs
  the 1 MiB chunk minimum.
- BatchExists/usage metering see dict hashes alongside chunk hashes; server-side
  changes are none (objects are opaque keyed bytes to the metadata plane).
- The trainer remains opportunistic and version-dependent **by design** — its output is
  an immutable object, so a zstd upgrade can only change *future* dicts, never the
  decodability of existing ones.
- Implementation rides post-beta hardening (it is the last multi-device gap in the
  compression story); this ADR freezes the design so the CAS key layout (`t{t}/d/{h}`)
  and the manifest `dict_hash` contract are stable before any GC runbook depends on it.
