# ADR-0002: Project name — Cairn (replaces placeholder "Terra")

Date: 2026-08-31 · Status: Accepted

## Context
"Terra" is explicitly a placeholder. The brief requires a better, more matching name before any
code exists (cheap to rename now, expensive later).

## Decision
The project is named **Cairn**.

A cairn is a stack of stones raised as a landmark. The metaphor maps one-to-one onto the engine:
- stones = content-addressed chunks (each unique, immutable, deliberately placed);
- stacking = FastCDC chunking + manifests;
- trail-marking = the journal and per-device cursors;
- summits = snapshots/refs (checkpoints you can return to);
- weatherproof = crash safety (I2) — a cairn survives its builder.

Naming applies uniformly: CLI `cairn`, crates `cairn-*`, proto package `cairn.v4`, client state
`~/.cairn/`, metrics `cairn_*`, docs and tests. Tenant storage keys keep the documented
`t{tenant}/...` prefix shape (the `t` remains meaningful: tenant).

## Alternatives considered
- **Strata** — strong geological layering metaphor (versions as strata), but heavily used in
  infrastructure products; weak trail/journey semantics.
- **Slate** — strong film resonance (clapperboard slating a take), but a crowded commercial name
  space (CRM/fintech), high confusion cost for search.
- **Basalt** — stone-strong storage-engine feel, but says nothing about versioning or sync.
- **Quarry** — good for the chunk store, bad for the sync engine; feels like a tool, not a
  system.

## Consequences
- All identifiers rename now, before the wire protocol freezes; zero migration cost.
- The word "terra" must not appear in proto packages, metrics names, or user-facing strings.
