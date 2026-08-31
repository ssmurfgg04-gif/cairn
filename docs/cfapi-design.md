# CfAPI walking skeleton — design sketch (WO2)

Status: design-first per work order; the skeleton below is the ONLY code written before
real-Windows validation. Everything not listed in WO2 scope is explicitly out.

## Goal

ONE placeholder file round-trips through CloudFiles on real Windows: registered sync
root → placeholder created from a stored blob → Explorer shows a cloud-state badge →
Notepad opens it byte-identical → first 2 MB served < 50 ms measured THROUGH the FS
callback, not an in-process microbench.

## Topology

```
cairn daemon (Windows service, owns CAS + engine)
  └─ cairn-fs-win (this crate)  ← the ONLY Windows-specific code in the repo
       ├─ register:   CfRegisterSyncRoot(root, provider_id, policy = FullHydration)
       ├─ create:     CfCreatePlaceholders(path, size, metadata = manifest_hash)
       ├─ connect:    CfConnectSyncRoot(callbacks = { FETCH_DATA })
       └─ FETCH_DATA → PlaceholderSource trait → daemon's CAS/headers
```

The daemon already owns the store/CAS/header-cache. This crate must NOT open the
database itself — it receives a `PlaceholderSource` and stays a thin FFI adapter.
That keeps the crash discipline (I2) and eviction policy in one place.

## Callback contract (the part that must be right)

- CfAPI delivers FETCH_DATA on its own threadpool with a `CF_CALLBACK_PARAMETERS`
  carrying `{ required_file_offset, required_length, file_id }`.
- Skeleton serves bytes synchronously from the local CAS (dev topology: content is
  warm). Response = `CfExecute(CF_OPERATION_TYPE_RETRIEVE_DATA)` with the SAME
  offset/length and the bytes — no async hydration in the skeleton.
- Placeholder identity: the manifest hash hex rides in the placeholder's sync
  metadata blob; FETCH_DATA reads it from `CF_CALLBACK_PARAMETERS.Placeholder` — no
  side tables.
- Failure: source error → complete the callback with the corresponding NTSTATUS via
  `CF_OPERATION_PARAMETERS.Hydration.FinalStatus` — Explorer shows the error state;
  we NEVER serve unverified bytes (I2: chunk hashes verified in CAS `get`).
- Threading: callbacks may arrive concurrently for different files but serialize per
  file on the CfAPI side for the skeleton; the CAS mutex is the only shared state.

## Invariants honored by the skeleton

- I1 (< 50 ms first byte for cached headers): header-cache head 2 MB is pinned warm in
  the CAS-backed source; the callback path is one hash lookup + one CAS read.
- I2 (never materialize corrupt data): bytes come from `Cas::get`, which verifies
  BLAKE3 on every read — the callback cannot serve unverified content even if it
  wanted to.

## Explicitly OUT of the skeleton (where overclaiming would live)

- No pin policies / background hydration / de-hydration.
- No bulk enumeration, no change-tracking (CfNotify), no icon overlays.
- No in-place edit writeback (placeholder → full file promotion is Explorer-side).
- No service installer / per-user vs per-machine registration matrix (manual step,
  documented in the WO2 acceptance runbook line).

## Verification plan (cannot be done from Linux)

1. Cross-compile gate: `cargo check -p cairn-fs-win --target x86_64-pc-windows-msvc`
   locally + `windows-smoke` CI job on `windows-latest` (compile + unit tests that do
   not touch CfAPI itself — CfAPI cannot run in CI).
2. Human gate on a real Windows box (acceptance list in the review):
   Explorer badge, Notepad byte-identical open, instrumented < 50 ms first-2-MB.
3. WinFsp fallback stays behind `placeholder_driver` kill switch as spec'd.

## Skeleton surface (what got implemented)

```rust
// all #[cfg(windows)]
pub struct PlaceholderSource;            // trait, daemon-implemented
pub fn register_sync_root(path, id) -> Result<()>;   // CfRegisterSyncRoot
pub fn create_placeholder(path, size, manifest_hash) -> Result<()>; // CfCreatePlaceholders
pub fn connect(source, root) -> Result<Connection>;  // CfConnectSyncRoot + FETCH_DATA
```
