# Cairn — content-addressed chunked sync & storage engine for professional video teams

> Git-style versioning, FastCDC chunking, BLAKE3 integrity, placeholder hydration for NLE media.
> Headless core: sync engine, storage server, local daemon, CLI. The ctl API is a deliverable.

**Name (ADR-0002):** a *cairn* is a deliberate stack of stones marking a route — every stone is
unique and immutable (content-addressed chunks), the stack marks the trail (journal + cursors),
each summit is a checkpoint (snapshots/refs), and it survives weather (crash safety, I2).

## Repository layout

```
crates/
  cairn-proto      wire protocol v4 (tonic/prost), package cairn.v4, fields 100-199 reserved
  cairn-tl         OTIO/FCPXML three-way timeline merge (ADR-0015): exact rationals,
                   identity ladder, C0-C10 classifier, canonical serializer, FCPXML bridge
  cairn-tray       Windows system tray (ADR-0016): thin onboarding layer over cairn-cli
  cairn-core       chunk/hash/manifest/compress/bloom/error-taxonomy (pure, heavily tested)
  cairn-store      local CAS + client SQLite (WAL) + outbox + header cache
  cairn-sync       sync engine: state machine, AIMD uploader, conflict copies, fold
  cairn-server     metadata plane + control-plane jobs + data-plane presigning
  cairn-cli        CLI + local daemon (ctl gRPC :17777, local dashboard :17778)
  cairn-sim        deterministic simulation suite (I2 enforcement)
  cairn-fs-linux   FUSE (fuser)
  cairn-fs-mac     File Provider shim (cfg(target_os = "macos"))
  cairn-fs-win     CfAPI via windows-rs (cfg(windows))
  cairn-x          e2e + fault-injection harness (kill -9 at every step), golden corpus
proto/cairn/v4     .proto source of truth
docs/              SPEC.md, adr/, ctl-api.md, runbooks/, STATUS.md
```

## Install (Windows, one command)

```powershell
irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
```

Installs the engine + the system tray (tray icon in the notification area:
connect a project folder, check status, open the project — no terminal
needed; ADR-0016). SHA-verified downloads, per-user autostart, no admin
rights. The CLI path below remains for servers and power users.

**What you're evaluating:** the merge is `cairn tl-merge --base b.otio
--ours a.otio --theirs b2.otio` (exit 0 clean / 1 notes / 2 conflicts / 3
refused). The honest competitive ledger — where we win, where LucidLink and
friends win — is [docs/COMPETITIVE.md](docs/COMPETITIVE.md).

## Quick start

```sh
just build        # cargo build --workspace
just test         # cargo nextest run --workspace
just clippy       # pedantic on core crates, -D warnings in CI
just sim          # deterministic simulation suite
just run-server   # metadata+data plane on :7443 (dev TLS off)
just run-daemon   # local ctl gRPC :17777 + dashboard :17778
just doctor       # end-to-end health check
```

## The two questions every design argument resolves against

- **I1:** "I opened a 50GB BRAW in Resolve — how long until I can scrub?" (<50ms cached header serve)
- **I2:** "A crash happened at any point — did we lose an acknowledged save or corrupt a project
  file?" (Answer must always be: **no**.)

Read `SPEC.md` first. Every deviation from it is a bug unless an ADR in `docs/adr/` says otherwise.
Ported/studied code provenance lives in `THIRD_PARTY.md`. The frozen control contract lives in
`docs/ctl-api.md`.

## License

Apache-2.0 for Cairn itself; see `THIRD_PARTY.md` for referenced implementations.
