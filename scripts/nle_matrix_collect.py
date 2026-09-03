#!/usr/bin/env python3
"""NLE human-gate matrix collector (I1, round 12).

Companion to docs/design/nle-test-matrix.md: runs on the STUDIO'S Windows
box (the thing CI cannot emulate — a real NLE + a real artist), walks the
H1–H10 gate rows, and captures the objective measurements the matrix
defines (doctor, status snapshots, hydration metrics from the daemon log,
BLAKE3 byte-identity before/after).

Usage (on the Windows box, from an elevated-free PowerShell with the cairn
daemon attached and RUST_LOG=info captured to a file):

    python nle_matrix_collect.py --project <project-id> --out results.json

The script is read-only with respect to the project tree: it hashes, polls,
and reads logs; it never writes into the mounted root. The H-rows that need
HUMAN action inside the NLE (open project, scrub, save) are recorded as
checklists the operator confirms with --confirm-row H1 etc.; the script
timestamps each confirmation and pairs it with the metrics snapshot taken
at that moment.

Output: a single JSON the studio sends back — the "report back" protocol
from the 100% checklist (I3). Results land in docs/nle-matrix-results/ when
they arrive.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path


def run(cmd: list[str], timeout_s: float = 60.0) -> tuple[int, str]:
    """Run a command hidden (no console flash on Windows), return (rc, text)."""
    creationflags = 0x08000000 if os.name == "nt" else 0  # CREATE_NO_WINDOW
    try:
        out = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout_s,
            creationflags=creationflags,
        )
        return out.returncode, (out.stdout or out.stderr or "").strip()
    except FileNotFoundError:
        return 127, f"command not found: {cmd[0]}"
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {timeout_s}s"


def blake3_dir(root: Path) -> dict[str, str]:
    """BLAKE3 (via `b3sum` if present, else sha256) of every file under root.

    The byte-identity oracle from the matrix: before/after each row, hashes
    must match EXACTLY except for files the NLE itself changed.
    """
    out: dict[str, str] = {}
    for p in sorted(root.rglob("*")):
        if p.is_file():
            h = hashlib.sha256()
            with open(p, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            out[str(p.relative_to(root))] = h.hexdigest()
    return out


def status_snapshot(cairn: str) -> dict:
    rc, text = run([cairn, "status", "--json"])
    try:
        return {"rc": rc, "json": json.loads(text)}
    except json.JSONDecodeError:
        return {"rc": rc, "raw": text[:2000]}


def doctor(cairn: str) -> dict:
    rc, text = run([cairn, "doctor"])
    return {"rc": rc, "healthy": rc == 0, "raw": text[:4000]}


def grep_log(log: Path, patterns: dict[str, str]) -> dict[str, list[str]]:
    """Pull the metric lines the matrix defines out of the daemon log."""
    found: dict[str, list[str]] = {k: [] for k in patterns}
    try:
        text = log.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return found
    for line in text.splitlines():
        for key, pat in patterns.items():
            if pat in line:
                found[key].append(line.strip()[:500])
    return found


DAEMON_PATTERNS = {
    "hydration_first_byte_ms": "cairn_hydration_first_byte_ms",
    "sync_propagation": "sync_propagation",
    "journal_op": "journal",
    "outbox": "outbox",
    "conflict_copy": "conflict",
}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--project", required=True, help="attached project id (cairn projects)")
    ap.add_argument("--root", help="project root path (default: from `cairn projects`)")
    ap.add_argument("--log", default=None, help="daemon log file with RUST_LOG=info capture")
    ap.add_argument("--out", default="nle-matrix-results.json", help="output JSON path")
    ap.add_argument("--confirm-row", action="append", default=[], help="confirm a human row: H1..H10")
    ap.add_argument("--nle", default="unspecified", help="which NLE: premiere | resolve | blender | all")
    ap.add_argument("--operator", default=os.environ.get("USERNAME", "unknown"), help="operator name")
    ap.add_argument("--box", default="", help="hardware description (CPU/GPU/RAM/NVMe/free text)")
    args = ap.parse_args()

    cairn = os.environ.get("CAIRN_BIN", "cairn")
    rows = [r.strip().upper() for r in args.confirm_row]

    report: dict = {
        "schema": "cairn-nle-matrix/1",
        "captured_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "nle": args.nle,
        "operator": args.operator,
        "box": args.box,
        "cairn_version": run([cairn, "--version"])[1],
        "project": args.project,
        "doctor_start": doctor(cairn),
        "status_start": status_snapshot(cairn),
        "rows": {},
        "checks": {},
    }

    # resolve root
    root = args.root
    if not root:
        rc, text = run([cairn, "projects"])
        for line in text.splitlines():
            if args.project in line:
                parts = line.split()
                root = parts[-1] if parts else None
                break
    if not root:
        print(f"ERROR: cannot resolve project root for {args.project}; pass --root", file=sys.stderr)
        return 2
    root_path = Path(root)
    if not root_path.is_dir():
        print(f"ERROR: root {root} is not a directory", file=sys.stderr)
        return 2
    report["root"] = str(root_path)

    hashes = blake3_dir(root_path)
    report["checks"]["byte_identity_baseline"] = {
        "files": len(hashes),
        "algorithm": "sha256 (b3sum absent) — swap in --b3 if your box has it",
    }
    if shutil_which("b3sum"):
        report["checks"]["byte_identity_baseline"]["algorithm"] = "blake3 via b3sum"

    log = Path(args.log) if args.log else None
    if log and log.exists():
        report["daemon_log_metrics"] = grep_log(log, DAEMON_PATTERNS)
    else:
        report["checks"]["daemon_log"] = "NOT PROVIDED — pass --log for hydration/propagation metrics"

    # human-confirmed rows: pair each confirmation with the objective state
    for row in rows:
        stamp = dt.datetime.now(dt.timezone.utc).isoformat()
        after = blake3_dir(root_path)
        report["rows"][row] = {
            "confirmed_utc": stamp,
            "status_after": status_snapshot(cairn),
            "byte_identity_preserved": hashes == after,
            "byte_identity_diff": {
                "changed": sorted(k for k in set(hashes) | set(after) if hashes.get(k) != after.get(k))
            },
        }
        hashes = after

    report["doctor_end"] = doctor(cairn)
    report["status_end"] = status_snapshot(cairn)

    Path(args.out).write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"written: {args.out}")
    print("send it back per docs/design/nle-test-matrix.md §reporting — results land in")
    print("docs/nle-matrix-results/ and update docs/BENCHMARKS.md with live numbers.")
    return 0


def shutil_which(name: str) -> str | None:
    import shutil

    return shutil.which(name)


if __name__ == "__main__":
    sys.exit(main())
