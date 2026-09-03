#!/usr/bin/env python3
"""Extract real timeline files from Premiere / Resolve projects (Round 13).

The merge engine is payload-agnostic: what it needs for 100% real-world
coverage is timelines that REAL NLEs PRODUCED. The NLE's own 2-click export
(File > Export > Final Cut Pro XML in Premiere; File > Export Timeline >
OpenTimelineIO in Resolve) is the loss-free extraction path -- this tool does
everything around those two clicks so the artist never opens a terminal:

  1. INSPECT the project container (.prproj = gzip/zip-wrapped XML,
     .drp = multi-entry zip): reports shape, embedded sequence names, and
     entry inventory WITHOUT modifying anything (read-only);
  2. DISCOVER exported timelines (.fcpxml / .otio) in the project folder,
     a folder you point at, or the Downloads folder;
  3. VALIDATE each discovered timeline with `cairn tl-capture`
     (parse + identity stamp + canonicalize + sidecar) and, when
     python-otio is importable, the reference-implementation oracle;
  4. STAGE everything into a corpus bundle (files + manifest.json with
     sha256s) ready to drop into scripts/real_timeline_corpus.py pins or
     to send back per docs/design/nle-test-matrix.md.

Exit codes: 0 validated at least one timeline · 1 nothing found/validated ·
2 usage error.

Usage:
    python scripts/extract_timeline_from_project.py --project C:\\Proj\\cut.prproj
    python scripts/extract_timeline_from_project.py --find C:\\Proj
    python scripts/extract_timeline_from_project.py --find %USERPROFILE%\\Downloads \\
        --stage C:\\cairn-corpus-stage
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path

CAIRN_DEFAULT = str(Path(__file__).resolve().parent.parent / "target" / "debug" / "cairn")


def run(cmd: list[str], timeout_s: float = 120.0) -> tuple[int, str]:
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout_s,
                           creationflags=0x08000000 if os.name == "nt" else 0)
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


def sniff(path: Path) -> str:
    with open(path, "rb") as f:
        head = f.read(16)
    if head.startswith(b"\x1f\x8b"):
        return "gzip"
    if head.startswith(b"PK\x03\x04"):
        return "zip"
    if head.startswith(b"BLENDER"):
        return "blender-raw"
    return "opaque"


def inspect_prproj(path: Path) -> dict:
    """Premiere .prproj: gzip (older) or zip-wrapped XML. Read-only probe.

    The Premiere-native XML is NOT a merge substrate -- the loss-free path is
    File > Export > Final Cut Pro XML. We still inspect it because the
    container shape is what the sync engine chunks, and the sequence names
    tell the artist which export to make.
    """
    info: dict = {"file": str(path), "bytes": path.stat().st_size, "kind": sniff(path)}
    try:
        if info["kind"] == "gzip":
            with gzip.open(path, "rt", encoding="utf-8", errors="replace") as f:
                xml = f.read(8 << 20)
        elif info["kind"] == "zip":
            names = []
            with zipfile.ZipFile(path) as z:
                names = z.namelist()
                blob = next((n for n in names if n.lower().endswith((".xml", ".json"))), None)
                xml = z.read(blob).decode("utf-8", "replace") if blob else ""
            info["zip_entries"] = len(names)
        else:
            xml = ""
        if xml:
            seqs = re.findall(r'<Sequence[^>]*name="([^"]+)"', xml)[:50]
            info["sequence_names"] = seqs
            info["owner_project"] = "premiere"
    except (OSError, zipfile.BadZipFile, gzip.BadGzipFile) as e:
        info["inspect_error"] = str(e)[:200]
    return info


def inspect_drp(path: Path) -> dict:
    """Resolve .drp: multi-entry zip (project DB export). Opaque to us BY
    DESIGN (zip-arm normalization refuses loudly). We inventory entries only."""
    info: dict = {"file": str(path), "bytes": path.stat().st_size, "kind": sniff(path)}
    try:
        with zipfile.ZipFile(path) as z:
            entries = z.namelist()
            info["zip_entries"] = len(entries)
            info["entry_names"] = entries[:50]
            # the project name usually rides a JSON entry
            for n in entries:
                if n.lower().endswith(".json"):
                    blob = z.read(n).decode("utf-8", "replace")
                    m = re.search(r'"[Pp]roject[Nn]ame"\s*:\s*"([^"]+)"', blob)
                    if m:
                        info["project_name"] = m.group(1)
                        break
        info["owner_project"] = "resolve"
    except (OSError, zipfile.BadZipFile) as e:
        info["inspect_error"] = str(e)[:200]
    return info


def discover_timelines(roots: list[Path]) -> list[Path]:
    out: list[Path] = []
    for root in roots:
        if root.is_file():
            if root.suffix.lower() in (".fcpxml", ".otio"):
                out.append(root)
            continue
        for pat in ("*.fcpxml", "*.otio"):
            out.extend(sorted(root.rglob(pat)))
    return out


def validate_timeline(cairn: str, path: Path, work: Path) -> dict:
    """tl-capture the discovered export; report pass/fail + timing."""
    scratch = work / path.name
    scratch.mkdir(parents=True, exist_ok=True)
    local = scratch / path.name
    shutil.copy2(path, local)
    t0 = time.time()
    rc, out = run([cairn, "tl-capture", str(local)])
    return {
        "file": str(path),
        "bytes": path.stat().st_size,
        "sha256": sha256_of(path),
        "capture_exit": rc,
        "capture_ms": round((time.time() - t0) * 1e3, 1),
        "capture_out": out.strip().splitlines()[-1][:200] if out.strip() else "",
    }


def otio_oracle(path: Path) -> dict:
    probe = (
        "import opentimelineio as otio; "
        f"tl = otio.adapters.read_from_file({str(path)!r}); "
        "print('ok', type(tl).__name__, len(tl.tracks))"
    )
    rc, out = run(["python", "-c", probe], timeout_s=60)
    if "No module named" in out:
        return {"skipped": "python-otio not installed (pip install opentimelineio==0.18.1)"}
    return {"rc": rc, "ok": rc == 0,
            "out": out.strip().splitlines()[-1][:120] if out.strip() else ""}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--project", help="project file to inspect (.prproj or .drp)")
    ap.add_argument("--find", action="append", default=[],
                    help="folder(s) to scan for exported .fcpxml/.otio (repeatable)")
    ap.add_argument("--stage", help="stage validated timelines + manifest.json here")
    ap.add_argument("--cairn", default=CAIRN_DEFAULT)
    args = ap.parse_args()

    if not args.project and not args.find:
        # default: scan the project's own folder + Downloads
        print("nothing to do: pass --project <file> and/or --find <folder> "
              "(default scan = project folder + Downloads)", file=sys.stderr)
        return 2

    report: dict = {"captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "projects": [], "timelines": [], "staged": []}
    work = Path(os.environ.get("TEMP", "/tmp")) / "cairn-extract"
    work.mkdir(parents=True, exist_ok=True)

    roots: list[Path] = [Path(p) for p in args.find]
    if args.project:
        ppath = Path(args.project)
        if not ppath.exists():
            print(f"FAIL no such project: {ppath}", file=sys.stderr)
            return 2
        if ppath.suffix.lower() == ".prproj":
            info = inspect_prproj(ppath)
        elif ppath.suffix.lower() == ".drp":
            info = inspect_drp(ppath)
        else:
            info = {"file": str(ppath), "kind": sniff(ppath),
                    "note": "unknown project suffix; inspect only"}
        report["projects"].append(info)
        print(json.dumps(info, indent=2))
        print("\nNEXT (the 2 clicks, in the NLE):")
        if info.get("owner_project") == "premiere":
            print("  Premiere: File > Export > Final Cut Pro XML -> save into "
                  + str(ppath.parent))
        elif info.get("owner_project") == "resolve":
            print("  Resolve:  File > Export Timeline > OpenTimelineIO (.otio) -> "
                  "save into " + str(ppath.parent))
        if not args.find:
            roots.append(ppath.parent)
        dl = Path.home() / "Downloads"
        if dl.is_dir():
            roots.append(dl)  # NLE default export landing spots

    timelines = discover_timelines(roots)
    print(f"\ndiscovered {len(timelines)} exported timeline(s)")
    for t in timelines:
        v = validate_timeline(args.cairn, t, work)
        canon = Path(str(work / t.name / t.name).rsplit(".", 1)[0] + ".canonical.otio")
        v["interop"] = otio_oracle(canon) if canon.exists() else "no canonical (capture failed)"
        report["timelines"].append(v)
        print(f"  {'OK ' if v['capture_exit'] == 0 else 'REF'} {t} "
              f"({v['capture_ms']}ms, exit {v['capture_exit']})")

    good = [v for v in report["timelines"] if v["capture_exit"] == 0]
    if args.stage and good:
        stage = Path(args.stage)
        stage.mkdir(parents=True, exist_ok=True)
        manifest = []
        for v in good:
            src = Path(v["file"])
            dst = stage / (src.stem + "-" + v["sha256"][:8] + src.suffix)
            shutil.copy2(src, dst)
            manifest.append({"staged": str(dst), "origin": str(src),
                             "sha256": v["sha256"], "bytes": v["bytes"]})
            report["staged"].append(str(dst))
        (stage / "manifest.json").write_text(json.dumps(manifest, indent=2),
                                              encoding="utf-8")
        print(f"\nstaged {len(good)} timeline(s) + manifest.json into {stage}")
        print("pin them into scripts/real_timeline_corpus.py CORPUS (url can be a "
              "file:// or an uploaded location) to grow the real-world gate")

    out_json = Path("extract-report.json")
    out_json.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"report: {out_json.resolve()}")
    return 0 if good else 1


if __name__ == "__main__":
    sys.exit(main())
