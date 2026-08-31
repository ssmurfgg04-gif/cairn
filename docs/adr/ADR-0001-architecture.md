# ADR-0001: Architecture overview — three planes, journal-as-database, headless core

Date: 2026-08-31 · Status: Accepted

## Context
Cairn v4 is a Git-style, content-addressed, chunked sync and storage engine for professional
video teams. Professional NLE workloads (Premiere, Resolve) demand scrub-ready hydration of
50GB-class camera files, crash-proof collaboration, and per-project correctness (leases). The
headless core (sync engine, storage server, local daemon, CLI) ships first; any UI builds later
against the frozen localhost ctl API.

## Decision
1. **Three planes.** Control plane (idempotent, resumable, kill-switchable background jobs),
   metadata plane (stateless gRPC, p99 <150ms, SQLite-compatible SQL, dialect-portable DDL, no
   stored procedures), data plane (client ↔ bucket direct; the API server never proxies blob
   bytes; presigned writes, signed immutable range-capable reads).
2. **The journal IS the database.** Server-linearized per-project append log with
   server-assigned seq is the source of truth; snapshots are folds of the journal; refs CAS only
   at fold. Never CAS a ref on a file save. Never push without cursors.
3. **Headless core.** No GUI. CLI + localhost gRPC ctl API (127.0.0.1:17777), contract versioned
   like the wire protocol. (The user-mandated local diagnostics dashboard exception is scoped in
   ADR-0009 and does not reopen this decision for anything else.)
4. **Hard invariants** I1 (hydration first byte <50ms cached / <500ms uncached, instrumented as
   `cairn_hydration_first_byte_ms`), I2 (crash at any point loses nothing, corrupts nothing —
   enforced by deterministic simulation), I3 (strict tenant scoping of every byte, hash lookup,
   lease, and metadata row; no cross-tenant dedup, ever), I4 (server clock only; client
   timestamps informational).

## Consequences
- Client and server share `cairn-proto`/`cairn-core` in one monorepo; wire changes are API
  changes and require ADR + ctl-api.md sync.
- The journal model makes conflict detection O(1) indexed lookups (`(tenant,project,path,seq)`)
  and makes folding a background concern instead of a save-path concern.
- LWW-per-path + conflict copies + leases is the entire consistency story: no CRDTs, no merge.
