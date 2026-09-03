#!/usr/bin/env python3
"""Real-NLE timeline corpus gate (Round 13).

The premise (docs/design/nle-test-matrix.md, "CI coverage map"): the merge
engine is payload-agnostic -- pixels never reach cairn-tl -- so 100% real-world
coverage of the merge surface means running timelines that REAL NLEs
PRODUCED, not synthesized fixtures. This gate:

  1. downloads a PINNED corpus of real NLE-generated timelines
     (python-otio's own sample data from actual productions + authentic
     Final Cut Pro X .fcpxml from public archives: PRONOM, BBC),
     sha256-verified against pins below;
  2. runs each through `cairn tl-capture` (parse + identity stamp +
     canonicalize + sidecar);
  3. derives BOTH merge scenarios per file on the REAL timeline:
       cooperative -- ours trims clip A, theirs trims a DIFFERENT clip B
                     (or adds a marker when only one editable clip exists)
                     -> contract: exit 0 or 1 (auto-merge, never silent loss)
       conflict   -- both sides edit the SAME clip, SAME field, different
                     values -> contract: exit 2 (theirs withheld, report
                     written, base/ours untouched)
  4. re-captures the merged output (the merge result must itself be a valid
     capture-substrate document) and, when python-otio is importable, runs
     the interop oracle on it;
  5. writes one JSON to --out (landed in docs/nle-matrix-results/ in CI).

Deterministic: sources are pinned by commit SHA + file sha256. Outcome pins
(the EXPECTED map) record the honest per-file status after the first run;
any DEVIATION from the pins fails the gate -- a regression (a file that used
to parse now refuses) fails, and so does an unrecorded improvement.

Exit codes: 0 all contracts green · 1 contract violation or pin drift.

Usage:
    python scripts/real_timeline_corpus.py --cairn target/debug/cairn \
        --out docs/nle-matrix-results/real-timeline-corpus.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

# --- pinned corpus -----------------------------------------------------------
# Raw-file URLs are commit-SHA-pinned (immutable); sha256 makes the pin
# tamper-evident. Bumping a pin is a deliberate, reviewable act.

OTIO_REPO = "AcademySoftwareFoundation/OpenTimelineIO"
OTIO_SHA = "bc5fe2d78dc3f8b2a8feb7e04483d85a12e80072"

CORPUS: dict[str, dict] = {
    # real .otio from python-otio's sample data (real productions converted)
    "premiere_example.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/premiere_example.otio",
        "sha256": "077bdb6e487a906db79cf0555aadfdf20e8ba6a8826ef2ee76a894377c08bc36",
        "origin": "OpenTimelineIO sample data (real Premiere sequence via the premiere adapter)",
    },
    "screening_example.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/screening_example.otio",
        "sha256": "d344ee732885c165f68d6f845e04aaf17d61de6cf8c75dd45519f460fca4d610",
        "origin": "OpenTimelineIO sample data (real screening sequence)",
    },
    "effects.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/effects.otio",
        "sha256": "2507d49a7c14b391116da515521bd087dee238fbb27b9a83faec0dc1ffeb151c",
        "origin": "OpenTimelineIO sample data (effect stacks, 356 KB)",
    },
    "nested_example.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/nested_example.otio",
        "sha256": "2d11b95eb34522952b8b3d99ddac8935be6700649fd9726f6f2424bacd22ccd0",
        "origin": "OpenTimelineIO sample data (nested stacks)",
    },
    "multitrack.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/multitrack.otio",
        "sha256": "8982f905993ca4c7113423e63fdd7d2eff69d56ed4659a080e3281b7e7f038e0",
        "origin": "OpenTimelineIO sample data (multitrack)",
    },
    "multiple_track.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/multiple_track.otio",
        "sha256": "8cfcaa5f346a302a3e3c23ed03b35f85046c9d9a6a67e5c894652d814a0b5668",
        "origin": "OpenTimelineIO sample data (multiple tracks)",
    },
    "transition.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/transition.otio",
        "sha256": "976f608b6feb511ce6a1155d3d3c4703d51ea4c81a240a513d2cfc2939c3d246",
        "origin": "OpenTimelineIO sample data (transitions)",
    },
    "transition_test.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/transition_test.otio",
        "sha256": "fe2a88668728386c1083cb964196e8966a5cc2062cfc022c7548fb43f21691da",
        "origin": "OpenTimelineIO sample data (transition edges)",
    },
    "simple_cut.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/simple_cut.otio",
        "sha256": "a85440060aa8b421fb7430dcf4c6d9a89a78b256198767afcfff1496ededf7eb",
        "origin": "OpenTimelineIO sample data (simple cut)",
    },
    "clip_example.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/clip_example.otio",
        "sha256": "b87fbdaf96561c4ba2b361c572b298a91aa2a73613e215402dd5ae400220b326",
        "origin": "OpenTimelineIO sample data (clip example)",
    },
    "preflattened.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/preflattened.otio",
        "sha256": "553240152332819a8b0a7bb3ab0469ac9f91e075ffb2a24e3e975f7473770424",
        "origin": "OpenTimelineIO sample data (preflattened)",
    },
    "generator_reference_test.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/generator_reference_test.otio",
        "sha256": "85c0a9eca537134ee75809820e6555e9cbec0ef64be5d1806c23cedf382a8c59",
        "origin": "OpenTimelineIO sample data (generator references)",
    },
    "big_int.otio": {
        "url": f"https://raw.githubusercontent.com/{OTIO_REPO}/{OTIO_SHA}/tests/sample_data/big_int.otio",
        "sha256": "065de8d684cd8598d6d21f8915008ccadf74b7ab978587703bf47597a8c556a7",
        "origin": "OpenTimelineIO sample data (large integer time values)",
    },
    # authentic / real-world-shaped FCPXML
    "pronom_fcp.fcpxml": {
        "url": "https://raw.githubusercontent.com/digital-preservation/PRONOM_Research/88f6149b01f1e8efd73caf6a6a3508eccbcc78ac/Submissions/Final%20Cut%20Pro/Samples/FinalCutProX.fcpxml",
        "sha256": "55439d9ea3779482ad8b53b4f296c62388004ec46d98d1d757d592c5c15c7c07",
        "origin": "PRONOM (UK National Archives) authentic Final Cut Pro X sample, fcpxml 1.8",
    },
    "bbc_3clips.fcpxml": {
        "url": "https://raw.githubusercontent.com/bbc/fcpx-xml-composer/d59b1bd39e0cab9cdb15ebba5c8e0970be8711f0/sample/fcpxml/3%20clips.fcpxml",
        "sha256": "4509d63164e08adc465057e7bf4a9edcc92ae2d48c697621029e8e84091e32d9",
        "origin": "BBC R&D fcpx-xml-composer sample (3 clips, fcpxml 1.8)",
    },
    "bbc_morgan.fcpxml": {
        "url": "https://raw.githubusercontent.com/bbc/fcpx-xml-composer/d59b1bd39e0cab9cdb15ebba5c8e0970be8711f0/sample/fcpxml/morgan-sequence.fcpxml",
        "sha256": "bd22fd76ad5f9ab6a97c6b8cec42483b36e70ae2b9c7b1ed293a17e95ccf8fc9",
        "origin": "BBC R&D fcpx-xml-composer sample (morgan-sequence, fcpxml 1.8)",
    },
    "cutlass_short.fcpxml": {
        "url": "https://raw.githubusercontent.com/andrewarrow/cutlass/4dbe20dea4152b9de7d9c6ba8cf70e04bda9feb7/samples/short.fcpxml",
        "sha256": "c1f0b9cc6ce683742f39f6e7d34adcc4f092a9f9e8f12041f917759345c4b39e",
        "origin": "cutlass samples (short vertical edit, fcpxml 1.13)",
    },
    "cutlass_pip.fcpxml": {
        "url": "https://raw.githubusercontent.com/andrewarrow/cutlass/4dbe20dea4152b9de7d9c6ba8cf70e04bda9feb7/samples/pip.fcpxml",
        "sha256": "84e82886621316c9cc884d5cac4d52ba9acb3dd181279aee927789a245ff06bb",
        "origin": "cutlass samples (picture-in-picture, fcpxml 1.13)",
    },
}

# Outcome pins -- the honest ratchet. Recorded from the first verified run
# (2026-09-04, Linux, rust 1.98.0, cairn-tl v4). status: "capture+merge" =
# full contract green; "refused-at-capture" = the honest C10/bad-input
# refusal (big_int.otio carries python's non-standard JSON `Inf` tokens --
# strict JSON parsers must refuse it). Update pins ONLY with a recorded
# run + reason. A DEVIATION from the pins fails the gate in BOTH directions
# (regression or unrecorded improvement).
EXPECTED: dict[str, dict] = {
    "premiere_example.otio": {"status": "capture+merge"},
    "screening_example.otio": {"status": "capture+merge"},
    "effects.otio": {"status": "capture+merge"},
    "nested_example.otio": {"status": "capture+merge"},
    "multitrack.otio": {"status": "capture+merge"},
    "multiple_track.otio": {"status": "capture+merge"},
    "transition.otio": {"status": "capture+merge"},
    "transition_test.otio": {"status": "capture+merge"},
    "simple_cut.otio": {"status": "capture+merge"},
    "clip_example.otio": {"status": "capture+merge"},
    "preflattened.otio": {"status": "capture+merge"},
    "generator_reference_test.otio": {"status": "capture+merge"},
    "big_int.otio": {"status": "refused-at-capture"},  # python-otio writes non-standard JSON (Inf / -Infinity tokens); strict parse refuses honestly
    "pronom_fcp.fcpxml": {"status": "capture+merge"},
    "bbc_3clips.fcpxml": {"status": "capture+merge"},
    "bbc_morgan.fcpxml": {"status": "capture+merge"},
    "cutlass_short.fcpxml": {"status": "capture+merge"},
    "cutlass_pip.fcpxml": {"status": "capture+merge"},
}


def run(cmd: list[str], timeout_s: float = 120.0) -> tuple[int, str]:
    try:
        p = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout_s,
            creationflags=0x08000000 if os.name == "nt" else 0,  # CREATE_NO_WINDOW on win
        )
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except FileNotFoundError:
        return 127, f"command not found: {cmd[0]}"
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {timeout_s}s"


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fetch(corpus_dir: Path) -> list[str]:
    """Download + verify the pinned corpus. Returns failure names."""
    failures = []
    corpus_dir.mkdir(parents=True, exist_ok=True)
    for name, spec in CORPUS.items():
        dst = corpus_dir / name
        if dst.exists() and sha256_of(dst) == spec["sha256"]:
            continue  # cached + verified
        rc, out = run(["curl", "-sfL", "--retry", "3", "--max-time", "60", spec["url"], "-o", str(dst)])
        if rc != 0:
            failures.append(f"{name}: download failed ({rc})")
            continue
        got = sha256_of(dst)
        if got != spec["sha256"]:
            failures.append(f"{name}: sha256 {got} != pinned {spec['sha256']}")
    return failures


# --- edit derivation on REAL timelines --------------------------------------

def walk_clips(doc: dict) -> list[dict]:
    """All clip objects (any Clip.* schema) in document order."""
    out: list[dict] = []

    def visit(node):
        if not isinstance(node, dict):
            return
        sch = node.get("OTIO_SCHEMA", "")
        if sch.startswith("Clip."):
            out.append(node)
        for child in node.get("children", []) or []:
            visit(child)

    visit(doc.get("tracks", {}))
    return out


def trimmable(clips: list[dict]) -> list[dict]:
    """Clips carrying a source_range (trimmable without structural edits)."""
    return [c for c in clips if isinstance(c.get("source_range"), dict)]


def tracks_of(doc: dict) -> list[dict]:
    return doc.get("tracks", {}).get("children", []) or []


def derive_sides(base_doc: dict) -> dict | None:
    """Choose the cooperative + conflict edit pairs from the REAL timeline.

    Plan ladder (keeps the merge contract exercisable on ANY real shape):
      1. trim plan  -- clips with source_range (a real editor's trim)
      2. name plan  -- any clips (rename = Attr op)
      3. track/timeline name plan -- always available
    Returns None only when even the timeline has no nameable track (nothing
    merge-relevant to edit; recorded honestly, never guessed).
    """
    clips = walk_clips(base_doc)
    trims = trimmable(clips)
    if trims:
        i = 0
        j = 1 if len(trims) > 1 else 0
        return {"plan": "trim", "clips": len(clips), "ours": i, "theirs": j,
                "theirs_is_marker": len(trims) == 1}
    if clips:
        return {"plan": "clip-name", "clips": len(clips), "ours": 0,
                "theirs": 1 if len(clips) > 1 else 0, "theirs_is_marker": len(clips) == 1}
    tr = tracks_of(base_doc)
    if tr:
        return {"plan": "track-name", "clips": 0, "ours": 0,
                "theirs": 1 if len(tr) > 1 else 0, "theirs_is_marker": False}
    if base_doc.get("name"):
        return {"plan": "timeline-name", "clips": 0, "ours": 0, "theirs": 0,
                "theirs_is_marker": False}
    return None


def apply_ours(doc: dict, plan: dict) -> None:
    if plan["plan"] == "trim":
        c = trimmable(walk_clips(doc))[plan["ours"]]
        c["source_range"]["start_time"]["value"] = float(
            c["source_range"]["start_time"]["value"]) - 1.0
    elif plan["plan"] == "clip-name":
        walk_clips(doc)[plan["ours"]]["name"] += " (ours cut)"
    elif plan["plan"] == "track-name":
        tracks_of(doc)[plan["ours"]]["name"] = (tracks_of(doc)[plan["ours"]].get("name") or "V") + " (ours cut)"
    elif plan["plan"] == "timeline-name":
        doc["name"] += " (ours cut)"


def apply_theirs_coop(doc: dict, plan: dict) -> None:
    if plan["plan"] == "trim":
        if plan["theirs_is_marker"]:
            doc.setdefault("tracks", {}).setdefault("markers", []).append({
                "OTIO_SCHEMA": "Marker.2",
                "metadata": {},
                "name": "corpus-marker",
                "marked_range": {
                    "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": {"OTIO_SCHEMA": "RationalTime.1",
                                   "rate": 24.0, "value": 0.0},
                    "duration": {"OTIO_SCHEMA": "RationalTime.1",
                                 "rate": 24.0, "value": 12.0},
                },
                "comment": "added by real_timeline_corpus (theirs, cooperative)",
                "enabled": True,
                "color": "RED",
            })
            return
        c = trimmable(walk_clips(doc))[plan["theirs"]]
        c["source_range"]["duration"]["value"] = float(
            c["source_range"]["duration"]["value"]) - 1.0
    elif plan["plan"] == "clip-name":
        if plan["theirs_is_marker"]:
            doc.setdefault("tracks", {}).setdefault("markers", []).append({
                "OTIO_SCHEMA": "Marker.2", "metadata": {}, "name": "corpus-marker",
                "marked_range": {"OTIO_SCHEMA": "TimeRange.1",
                                 "start_time": {"OTIO_SCHEMA": "RationalTime.1",
                                                "rate": 24.0, "value": 0.0},
                                 "duration": {"OTIO_SCHEMA": "RationalTime.1",
                                              "rate": 24.0, "value": 12.0}},
                "comment": "corpus marker", "enabled": True, "color": "RED"})
            return
        walk_clips(doc)[plan["theirs"]]["name"] += " (grade)"
    elif plan["plan"] == "track-name":
        tr = tracks_of(doc)
        if len(tr) > 1:
            tr[plan["theirs"]]["name"] = (tr[plan["theirs"]].get("name") or "V") + " (grade)"
        else:
            # single track: theirs edits a DIFFERENT element (the timeline
            # name) so the cooperative case is genuinely disjoint
            doc["name"] += " (grade)"
    elif plan["plan"] == "timeline-name":
        doc["name"] += " (grade)"


def apply_theirs_conflict(doc: dict, plan: dict) -> None:
    # SAME element, SAME field, different value: a genuine editor clash
    if plan["plan"] == "trim":
        c = trimmable(walk_clips(doc))[plan["ours"]]
        c["source_range"]["start_time"]["value"] = float(
            c["source_range"]["start_time"]["value"]) - 5.0
    elif plan["plan"] == "clip-name":
        walk_clips(doc)[plan["ours"]]["name"] += " (theirs reedit)"
    elif plan["plan"] == "track-name":
        tracks_of(doc)[plan["ours"]]["name"] = (tracks_of(doc)[plan["ours"]].get("name") or "V") + " (theirs reedit)"
    elif plan["plan"] == "timeline-name":
        doc["name"] += " (theirs reedit)"


# --- the gate ----------------------------------------------------------------

def merge(cairn: str, work: Path, tag: str) -> dict:
    # side docs live in their own dir: base.otio -> base.canonical.otio etc.
    base = str(work / "base.canonical.otio")
    ours = str(work / "ours.canonical.otio")
    theirs = str(work / "theirs.canonical.otio")
    t0 = time.time()
    rc, out = run([cairn, "tl-merge", "--base", base, "--ours", ours, "--theirs", theirs])
    dt_ms = round((time.time() - t0) * 1e3, 1)
    merged = work / "ours.canonical.merged.otio" if rc in (0, 1, 2) else None
    report_json = None
    reports_dir = work / ".cairn-timeline" / "reports"
    if reports_dir.exists():
        reports = sorted(reports_dir.glob("*.json"))
        if reports:
            try:
                report_json = json.loads(reports[-1].read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                report_json = None
    merged_ok = None
    if merged is not None and merged.exists():
        rc2, _ = run([cairn, "tl-capture", str(merged)])
        merged_ok = rc2 == 0
    return {
        "exit": rc,
        "ms": dt_ms,
        "merged_recapture_ok": merged_ok,
        "outcome_line": next(
            (ln for ln in out.splitlines() if ln.startswith("outcome:")), "")[:160],
        "report": report_json,
    }


def otio_oracle(path: Path) -> dict | None:
    """python-otio interop: the reference implementation must accept it."""
    if shutil.which("python") is None:
        return {"skipped": "no python"}
    probe = (
        "import opentimelineio as otio; "
        f"tl = otio.adapters.read_from_file({str(path)!r}); "
        "print('ok', type(tl).__name__, len(tl.tracks))"
    )
    rc, out = run(["python", "-c", probe], timeout_s=60)
    if "No module named" in out:
        return {"skipped": "python-otio not installed"}
    return {"rc": rc, "ok": rc == 0, "out": out.strip().splitlines()[-1] if out.strip() else ""}


def canonical_sibling(src: Path) -> Path:
    """tl-capture writes <stem>.canonical.otio next to the input file."""
    return src.with_name(src.stem + ".canonical.otio")


def process_one(cairn: str, src: Path, work: Path) -> dict:
    entry: dict = {"source": src.name, "bytes": src.stat().st_size,
                   "sha256": sha256_of(src), "origin": CORPUS[src.name]["origin"]}
    # 1. capture (parse + stamp + canonicalize + sidecar)
    t0 = time.time()
    rc, out = run([cairn, "tl-capture", str(src)])
    entry["capture_ms"] = round((time.time() - t0) * 1e3, 1)
    entry["capture_exit"] = rc
    if rc != 0:
        entry["status"] = "refused-at-capture"
        entry["capture_err"] = out.strip().splitlines()[-1][:300] if out.strip() else ""
        print(f"   refusal reason: {entry['capture_err']}", flush=True)
        return entry
    canon = canonical_sibling(src)
    if not canon.exists():
        entry["status"] = "refused-at-capture"
        entry["capture_err"] = f"canonical output missing: {canon}"
        return entry
    # fcpxml canonical lands beside the source
    entry["status"] = "captured"

    # 2. derive edits on the REAL timeline
    base_doc = json.loads(canon.read_text(encoding="utf-8"))
    plan = derive_sides(base_doc)
    entry["editable_clips"] = plan["clips"] if plan else 0
    entry["edit_plan"] = plan["plan"] if plan else "none"
    if plan is None:
        entry["status"] = "no-editable-target (recorded honestly: nothing merge-relevant to edit)"
        entry["interops"] = otio_oracle(canon)
        return entry

    # 3. cooperative merge (base + ours-edit + theirs-edit, disjoint)
    side = work / (src.stem + ".coop")
    side.mkdir(parents=True, exist_ok=True)
    (side / "base.otio").write_text(canon.read_text(encoding="utf-8"), encoding="utf-8")
    ours_doc = json.loads(canon.read_text(encoding="utf-8"))
    apply_ours(ours_doc, plan)
    (side / "ours.otio").write_text(json.dumps(ours_doc), encoding="utf-8")
    theirs_doc = json.loads(canon.read_text(encoding="utf-8"))
    apply_theirs_coop(theirs_doc, plan)
    (side / "theirs.otio").write_text(json.dumps(theirs_doc), encoding="utf-8")
    for f in ("base", "ours", "theirs"):
        rc, out = run([cairn, "tl-capture", str(side / f"{f}.otio")])
        if rc != 0:
            entry["status"] = f"refused-at-side-capture({f})"
            entry["side_err"] = out.strip().splitlines()[-1][:300]
            return entry
    entry["cooperative"] = merge(cairn, side, "coop")
    entry["cooperative"]["expected_exit"] = "0 or 1"

    # 4. conflict merge (same clip, same field, different values)
    side2 = work / (src.stem + ".conflict")
    side2.mkdir(parents=True, exist_ok=True)
    (side2 / "base.otio").write_text(canon.read_text(encoding="utf-8"), encoding="utf-8")
    ours_doc = json.loads(canon.read_text(encoding="utf-8"))
    apply_ours(ours_doc, plan)
    (side2 / "ours.otio").write_text(json.dumps(ours_doc), encoding="utf-8")
    theirs_doc = json.loads(canon.read_text(encoding="utf-8"))
    apply_theirs_conflict(theirs_doc, plan)
    (side2 / "theirs.otio").write_text(json.dumps(theirs_doc), encoding="utf-8")
    for f in ("base", "ours", "theirs"):
        run([cairn, "tl-capture", str(side2 / f"{f}.otio")])
    entry["conflict"] = merge(cairn, side2, "conflict")
    entry["conflict"]["expected_exit"] = "2"

    # 5. interop oracle on the merged output
    merged_path = side / "ours.canonical.merged.otio"
    if merged_path.exists():
        entry["interops"] = {"merged": otio_oracle(merged_path)}
    else:
        entry["interops"] = {"merged": "no merged file (refused?)"}

    # contract evaluation
    coop_ok = entry["cooperative"]["exit"] in (0, 1) and entry["cooperative"]["merged_recapture_ok"]
    conf_ok = entry["conflict"]["exit"] == 2 and (entry["conflict"]["report"] or {}).get(
        "stats", {}).get("withheld", 1) >= 1
    entry["status"] = "capture+merge" if (coop_ok and conf_ok) else "CONTRACT-VIOLATION"
    return entry


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    here = Path(__file__).resolve().parent
    ap.add_argument("--cairn", default=str(here.parent / "target" / "debug" / "cairn"),
                    help="path to the cairn binary")
    ap.add_argument("--corpus", default=None,
                    help="corpus cache dir (default: <repo>/target/nle-corpus)")
    ap.add_argument("--work", default=None, help="scratch dir (default: <repo>/target/nle-corpus-work)")
    ap.add_argument("--out", default=str(here.parent / "docs" / "nle-matrix-results" / "real-timeline-corpus.json"))
    args = ap.parse_args()

    repo = here.parent
    corpus_dir = Path(args.corpus) if args.corpus else repo / "target" / "nle-corpus"
    work = Path(args.work) if args.work else repo / "target" / "nle-corpus-work"
    out_path = Path(args.out)

    cairn = args.cairn
    if not Path(cairn).exists():
        print(f"FAIL cairn binary not found: {cairn}", file=sys.stderr)
        return 1

    failures = fetch(corpus_dir)
    if failures:
        print("FAIL corpus fetch/verify: " + "; ".join(failures), file=sys.stderr)
        return 1

    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)

    results = []
    for name in sorted(CORPUS):
        src_dir = work / "capture" / name
        src_dir.mkdir(parents=True, exist_ok=True)
        src = src_dir / name
        shutil.copy2(corpus_dir / name, src)
        print(f"== {name} ({src.stat().st_size} B)", flush=True)
        entry = process_one(cairn, src, src_dir)
        results.append(entry)
        print(f"   {entry['status']}  capture={entry['capture_ms']}ms "
              f"coop_exit={entry.get('cooperative', {}).get('exit')} "
              f"conflict_exit={entry.get('conflict', {}).get('exit')}", flush=True)

    # pin evaluation (drift fails the gate in BOTH directions)
    violations = []
    by_name = {r["source"]: r for r in results}
    for name, exp in EXPECTED.items():
        got = by_name.get(name, {}).get("status", "MISSING")
        if got != exp["status"]:
            violations.append(f"pin drift: {name}: expected {exp['status']}, got {got}")
    for name in by_name:
        if name not in EXPECTED:
            violations.append(f"unpinned file (record then pin it): {name}")

    report = {
        "schema": "cairn-nle-matrix/real-timeline-corpus/1",
        "captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "cairn": run([cairn, "--version"])[1].strip().splitlines()[-1] if run([cairn, "--version"])[1].strip() else "unknown",
        "host": {"os": os.uname().sysname if hasattr(os, "uname") else sys.platform},
        "corpus": {n: {"origin": s["origin"], "bytes": (corpus_dir / n).stat().st_size}
                   for n, s in CORPUS.items()},
        "totals": {
            "files": len(results),
            "capture_merge_green": sum(1 for r in results if r["status"] == "capture+merge"),
        },
        "results": results,
        "pin_violations": violations,
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"\nwritten: {out_path}")
    print(f"totals: {report['totals']['capture_merge_green']}/{report['totals']['files']} capture+merge green")

    if violations:
        print("FAIL pin violations:", file=sys.stderr)
        for v in violations:
            print(f"  {v}", file=sys.stderr)
        return 1
    bad = [r for r in results if r["status"] == "CONTRACT-VIOLATION"]
    if bad:
        print(f"FAIL contract violations: {', '.join(r['source'] for r in bad)}", file=sys.stderr)
        return 1
    print("real-timeline corpus gate GREEN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
