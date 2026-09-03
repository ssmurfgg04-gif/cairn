# Cairn vs the field — the honest competitive ledger

Status: maintained (round 12). Every claim below links to evidence in this
repo. Where a competitor is better, we SAY SO — the doc is a decision tool,
not a brochure. (Competitor capabilities are from public docs as of
2026-09; corrections welcome via issues.)

## The field, one line each

- **LucidLink** (closest comparable): cloud-native "workspace" filesystem
  for creative teams; on-demand block fetch, big-brand studios.
- **Frame.io / Adobe Cloud (Project Candy)**: review+asset workflow SaaS
  that moved toward media mounts; Adobe ecosystem gravity.
- **Dropbox/OneDrive/Drive**: consumer cloud drives with sync clients;
  placeholder/hydration via their own filters, no timeline semantics.
- **NAS + VPN (Synology etc.)**: the in-house default; SMB latency, no
  dedup, no versioning discipline.
- **PostgreSQL-backed DAM/MAM (iconik, iconik-like)**: metadata-first,
  proxies-not-originals, browser-centric.

## Where Cairn is outright stronger (and the proof)

### 1. Timeline merge that no competitor ships at all

Two editors, same `.otio`/`.fcpxml`, both saved: LucidLink serializes you
behind a lock or hands you a conflict copy with a human pen. Cairn runs the
ADR-0015 three-way merge: an exact-rational (no float drift) diff against
the common ancestor, a four-rung identity ladder that survives renames and
moves, and a total C0–C10 classifier where every op pair maps to exactly
one verdict — trims+grades auto-merge, creative-parameter collisions go to
a human with a machine-readable report, never last-write-wins.

**Proof:** `crates/cairn-tl` (89 tests: golden corpus per class, two-editor
simulations, 200-case property suite with mirror-stability + no-silent-loss
invariants), `cairn tl-merge` exit-code contract (0/1/2/3), Kani harness
proving classifier totality, python-otio reference implementation as the
CI interop oracle.

**Honest limits:** vendor-native `.prproj`/`.drp` never merges (ADR-0014
rejects reverse-engineering vendor schemas — silent sequence corruption is
unsurvivable); merge is post-hoc three-way, not realtime co-editing; leases
(Phase 1–3) remain the correctness floor, merge only shortens the tail.

### 2. Crash integrity that is proven, not claimed

Every sync product says "reliable." Cairn's bar is the I2 invariant: **a
crash at any point — kill -9 at every step, replayed — never loses an
acknowledged save or corrupts a project file.** Not modeled, not fuzzed
lightly: an exhaustive fault-injection harness (real SIGKILL subprocesses at
every step), a 1,000-schedule deterministic simulation sweep, and 5 GB
soaks with kill-mid-upload → resume → byte-identical.

**Proof:** `crates/cairn-x` (crash matrix 6/6 steps zero-loss), `cairn-sim`
(1,000 schedules green in CI), M1–M3 rows in docs/STATUS.md with the exact
commands to re-run them.

### 3. FastCDC + content addressing: studio-scale dedup that rides autosaves

NLE autosave behavior (Premiere's 15-min full-file storm, Blender's
`.blend1/.blend2` rotations) is WHERE the bytes live. Cairn chunks with
FastCDC and dedups by content hash: measured 85–97% chunk-identity between
consecutive saves of the same project, 97.1% on a save-shaped mutation of a
real 372 MB film, 670 MiB/s ingest on real media.

**Proof:** docs/BENCHMARKS.md (FastCDC 1,254 MiB/s, BLAKE3 5,029 MiB/s,
ingest 859 MiB/s incl. fsync), docs/real-corpus-report.json (Blender open
movies + UCF101 — the same git-LFS shape studios use), M0 gate
chunk-reuse > 0.70 on synthetic save sequences.

**vs LucidLink:** their Filespaces do block-level on-demand too; their
public docs do not claim content-defined dedup across NLE autosave shapes,
and their strength is a global-name filesystem rather than the editor's
save loop. Where they win: mature macOS support, brand trust, existing
studio relationships. We say so.

### 4. Hydration latency with a public, falsifiable number

"I opened a 50 GB BRAW in Resolve — how long until I can scrub?" is I1.
Cairn publishes the measurement contract AND the number: header
first-byte p50 3 µs / p99 3.2 ms against the 50 ms gate, on stated
hardware, with the harness to re-run it. The NLE human-gate matrix
(docs/design/nle-test-matrix.md) + collector script is the open version of
the test the incumbents keep internal.

**Proof:** docs/BENCHMARKS.md, the `windows-cfapi-roundtrip` CI leg (real
CfAPI on windows-latest), `scripts/nle_matrix_collect.py` for the studio's
own box.

**Honest gap:** the interactive NLE matrix on physical Premiere/Resolve
hardware is studio-reported (the collector ships for exactly this); CI
cannot emulate an artist's week. STATUS.md marks that row 🟨, not ✅.

### 5. Cross-NLE, no lock-in: OTIO is the interchange spine

Timeline state lives in OpenTimelineIO (ASWF, schema-versioned) and FCPXML
through a ledgered bridge — not in a proprietary project database, not
 hostage to one NLE's vendor format. Your timeline's identity and history
is content-addressed in OUR CAS but expressible in the industry-neutral
format any tool can read.

**Proof:** `crates/cairn-tl` canonical serializer proven against
python-otio 0.18.x in CI; the FCPXML lossiness ledger is a tested fixture
(auditions/compounds/multicam named, out-of-ledger features refuse instead
of dropping).

### 6. Self-hostable and S3-compatible: your storage, your jurisdiction

SigV4-signed presigned paths to any S3-compatible bucket (R2, MinIO,
Wasabi, your own) — a studio can run the metadata plane and keep media
bytes in a bucket they control. The 5 GB soak and wire conformance ran
against REAL R2 and a REAL MinIO in CI, including a presign-conformance bug
we found and fixed on the real wire.

**Proof:** ADR-0005, `s3-conformance` just recipe, R2 soak row in
STATUS.md (kill -9 mid-ingest → resume → byte-identical, zero dup journal).

**vs LucidLink:** they host the backend; that is their business model, and
fine — but studios with data-residency contracts cannot use it. That is
our lane, stated without FUD.

### 7. Kill switches and determinism as product features

Chunk-input normalization, tiering, tray, merge-report-only-by-default:
flag-gated, live-flippable, no restart. Sync scheduling is deterministic
and simulated — the same schedule always converges the same way, which is
what makes 1,000-schedule CI sweeps meaningful.

**Proof:** kill-switch registry rows in STATUS.md; ADR-0008 (in-house
deterministic sim replacing madsim/shuttle).

## Where competitors are ahead (no spin)

- **LucidLink:** macOS maturity (our FileProvider leg compiles, Finder
  validation is hardware-lab 🟨); enterprise support/brand; global
  filesystem naming (one namespace everywhere vs per-project mounts).
- **Frame.io/Adobe:** review workflows, commenting, camera-to-cloud — we
  deliberately have none of it (the engine stays headless; the tray is a
  thin onboarding layer, ADR-0016).
- **Dropbox et al.:** consumer simplicity, selective sync UIs polished
  over a decade, mobile apps.
- **NAS:** zero subscription cost and LAN latency. For a single-room shop
  with a good NAS admin, it's a fine answer; Cairn's value is the
  multi-site / hybrid-cloud / autosave-dedup case.

## The one-sentence version

**Cairn is the only option where two editors who saved the same timeline
get an automatic, provably-safe merge instead of a coin flip — and it
rides an engine whose crash-safety, dedup, and hydration numbers are
published with the harnesses that falsify them.**

The moat is not any single feature; it is that every claim in this file
has a test you can run, and the two things competitors could copy (the
merge semantics and the honesty culture) are the two things hardest to
copy.
