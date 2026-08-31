# Cairn ctl API — v4 (FROZEN CONTRACT)

Status: **FROZEN as of M7** (changes = bug; require ADR + version bump per §18)
Audience: the future UI team, ops, and the CLI. This file is normative.

Two transports expose the same service layer:

1. **ctl gRPC** — `127.0.0.1:17777`, token-authenticated (`Authorization: Bearer <device-token>`).
   Proto: `proto/cairn/v4/cairn.proto`, package `cairn.v4`. Field numbers 100–199 reserved in
   every message. Version negotiated at connect via `Handshake` (client min/max ↔ server max).
2. **Local HTTP gateway (optional, loopback-only)** — `127.0.0.1:17778`, JSON projection of the
   same services for the local dashboard (ADR-0009). Read-only endpoints open to loopback;
   mutating endpoints require the same bearer token (dashboard gets a first-party loopback
   session cookie when served).

Error envelope (both transports): every error carries
`ErrorDetail { code: string, retry_class: enum, message: string }` (ADR-0010). Codes:
`CONFLICT, STALE_LEASE, REF_CAS, UNAUTHENTICATED, PERMISSION_DENIED, NOT_FOUND, SESSION_EXPIRED,
CHECKSUM_MISMATCH, BATCH_TOO_LARGE, RATE_LIMITED, INTERNAL, UNAVAILABLE, COMPACTION_REQUIRED,
SESSION_FULL`.

---

## Ctl services (daemon, 127.0.0.1:17777)

### Handshake
`Handshake(HandshakeRequest{client_min_proto, client_max_proto, client_version}) →
HandshakeResponse{negotiated_proto, server_version}`

### StatusService
`Status(StatusRequest{}) → StatusResponse{version, proto, uptime_ms, projects: [ProjectStatus],
sync_state_summary, daemon_flags, server_reachable}` — cheap, safe to poll at 1–5s.

### ProjectService
- `AttachRoot(AttachRootRequest{root_path, server_addr, project_id}) → AttachRootResponse{project_id}`
- `DetachRoot(DetachRootRequest{project_id}) → Ack`
- `ListProjects(ListProjectsRequest{}) → ListProjectsResponse{projects: [ProjectInfo]}`

### SnapshotService
- `CreateSnapshot(CreateSnapshotRequest{project_id, label}) → CreateSnapshotResponse{commit_hash}`
- `ListSnapshots(ListSnapshotsRequest{project_id}) → ListSnapshotsResponse{snapshots:
  [SnapshotInfo{commit_hash, parent, label, author, snapshot_seq, server_ts}]}`
- `RestoreSnapshot(RestoreSnapshotRequest{project_id, commit_hash, target_path?}) →
  RestoreSnapshotResponse{restored_files, bytes}`

### PinService
- `Pin(PinRequest{project_id, path}) → Ack` (downloads + pins all chunks; excludes from local eviction)
- `Unpin(UnpinRequest{project_id, path}) → Ack`
- `ListPins(ListPinsRequest{project_id}) → ListPinsResponse{paths: [PinInfo{path, size, state}]}`

### RecallService
- `StartRecall(StartRecallRequest{project_id, path?}) → StartRecallResponse{job_id}`
- `RecallStatus(RecallStatusRequest{job_id}) → RecallStatusResponse{state, progress, total,
  bytes_done, eta_ms}` — progress + ETA per §12.

### DiagnosticsService
- `Doctor(DoctorRequest{}) → DoctorReport{checks: [DoctorCheck{name, ok, detail, latency_ms}],
  healthy}`
- `GcShadowReport(GcShadowReportRequest{tenant_id?, project_id?}) → GcShadowReportResponse{violations,
  would_delete_count, scanned_objects, clean}` — shadow-mode reachability cross-check; beta gate.
- `SetFlag(SetFlagRequest{name, value}) → Ack` (kill switches: `packing_enabled`,
  `tiering_enabled`, `delta_fold_enabled`, `compression_enabled`, `placeholder_driver`;
  admin scope required; takes effect next job run, no restart)
- `GetFlags(GetFlagsRequest{}) → GetFlagsResponse{flags: [FlagInfo]}`

### Metrics exposure
- Daemon (client-side): `GET 127.0.0.1:17778/metrics` — Prometheus text, includes
  `cairn_hydration_first_byte_ms` (I1).
- Server: `GET :9091/metrics` (bind configurable) — journal_append p99, sync propagation p95,
  upload success, AIMD distribution, GC shadow violations, bloom hit/miss, canary result.

---

## Metadata/data-plane services (server, :7443 dev / TLS in prod)

### Auth (`CtlAuthService`)
- `EnrollCode(EnrollCodeRequest{tenant_id, email, scopes}) → EnrollCodeResponse{code, expires_at}`
  (admin-authenticated; single-use)
- `Enroll(EnrollRequest{code, device_pubkey, device_name}) → EnrollResponse{paseto, expires_at,
  device_id}`
- `Revoke(RevokeRequest{device_id}) → Ack` (admin; unlink semantics)

### JournalService
- `Append(AppendRequest{tenant_id, project_id, device_id, request_id, op: JournalOp, lease_token?})
  → AppendResponse{seq, accepted | CONFLICT | STALE_LEASE}` — server-assigned seq; request_id
  idempotency; fencing enforced here (§8); conflict rule implemented exactly (§7.1).
- `Watch(WatchRequest{tenant_id, project_id, device_id, cursor}) → stream JournalBatch` — HINT
  only; cursor replay is the guarantee.
- `UpdateCursor(CursorUpdate{device_id, project_id, last_seq}) → Ack`

`JournalOp` oneof: `file_upsert{path, manifest_hash, size, base_seq}` ·
`file_delete{path, base_seq}` · `rename{old_path, new_path, manifest_hash, base_seq}` ·
`lease_event{path, kind, device_id}` (informational).

### LeaseService
- `Acquire(AcquireRequest{tenant_id, project_id, path, device_id, ttl_ms=60000}) →
  AcquireResponse{token, expires_at}` — token from `projects.next_lease_token` (DB sequence,
  restart-safe)
- `Renew(RenewRequest{...token, ttl_ms}) → RenewResponse{expires_at}` (jittered client-side)
- `Release(ReleaseRequest{...token}) → Ack`
- `ListLeases(ListLeasesRequest{project_id}) → ListLeasesResponse{leases}`

### UploadService
- `BatchExists(BatchExistsRequest{tenant_id, chunk_hashes[] ≤10k}) → BatchExistsResponse{missing[]}`
  — bloom negative pre-filter ONLY; authoritative check is the chunks table; a bloom false
  positive can never skip an upload (property-tested).
- `CreateUploadSession(CreateUploadSessionRequest{tenant_id, device_id, missing[]}) →
  CreateUploadSessionResponse{session_id, puts: [{chunk_hash, url, expires_at}]}` — presigned
  PUT, `x-amz-checksum-sha256` required, TTL ≤1h, write-scoped, no list.
- `CompleteUpload(CompleteUploadRequest{session_id, receipts: [{chunk_hash, size, etag}]}) →
  CompleteUploadResponse{verified[], rejected[]}` — server HEAD-verifies 10% sample (100% for
  chunks >64MB), inserts chunks rows.

### DownloadService
- `GetDownloadUrl(GetDownloadUrlRequest{tenant_id, manifest_hash, path}) →
  GetDownloadUrlResponse{url, expires_at}` — signed, immutable, Range-capable, 1h TTL; on 403
  mid-stream the client re-signs and resumes.
- `GetManifest(GetManifestRequest{tenant_id, manifest_hash}) → ManifestObject`

### ProjectService (server side)
- `CreateProject / ListProjects / GetProject{fold_seq, next_lease_token}` — tenant-scoped; I3
  enforced at every lookup (every key/row carries tenant_id).

### SnapshotService (server side)
- `FoldNow(project_id)` (admin) — triggers journal fold → commit → CAS ref update.
- `GetRef / ListRefs` — refs CAS-updated at fold only.

---

## Versioning & compatibility policy

- `cairn.v4` is the package; wire `proto_version = 4`. Handshake negotiates
  `max(min(client_max, server_max), client_min)`; failure → `UNIMPLEMENTED`.
- Field numbers 100–199 are reserved in every message (future-error/feature annotations).
- Breaking changes to this file require: ADR, major version bump, and a migration note for the
  UI team. Additive changes require bumping the minor and regenerating stubs.
- All timestamps are UTC i64 millis, server-authoritative (client ts informational, I4).
- All paths UTF-8 NFC (original bytes preserved for display).
- Log policy: presigned/query strings NEVER logged (bearer credentials).

## Additive change note (post-freeze record)

The contract above is frozen; this section records how changes since M8 were
made WITHOUT breaking it (the recipe every future change must follow):

1. **New message fields** → append with field numbers > 200 (100–199 stay
   reserved); old clients ignore unknown fields. Existing field numbers/types
   are immutable. *(Used: none yet.)*
2. **New RPCs** → new methods on existing services or a `v4` extension service;
   clients feature-detect via `UNIMPLEMENTED` and degrade gracefully. Never
   repurpose an existing RPC's semantics.
3. **Error surface** → `ErrorDetail` codes are a closed enum per ADR-0010;
   new codes append to the registry with a retry class, never reuse a number.
4. **Data plane (out of this contract)** → the `CAIRN_S3_*` bucket-backend
   wiring (ADR-0005) changed only server internals and presigned URL targets;
   ctl message shapes, ports, and handshake are untouched. The 1h presign TTL
   cap and checksum-handling semantics are unchanged (SPEC §9).
5. **Process** → every additive change ships with: regenerated stubs, a truth
   table/round-trip test, a STATUS.md row, and a minor-version note here.
   Breaking changes additionally require an ADR + UI-team migration note.
