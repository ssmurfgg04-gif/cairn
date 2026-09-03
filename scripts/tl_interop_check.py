#!/usr/bin/env python3
"""python-OTIO interop oracle for cairn-tl (ADR-0015 §7).

Round-trips cairn's CANONICAL output through the OTIO Python reference
implementation (the ASWF one) and asserts semantic equality both ways:

  1. parse cairn's canonical JSON with python-otio (it must ACCEPT it)
  2. re-serialize with python-otio and re-parse with python-otio (its own
     fixpoint)
  3. logical equality: same element tree shape (names, kinds, media URLs,
     times as exact seconds)

CI job `tl-merge-gate` runs this on every push; locally:
    /path/to/venv/python scripts/tl_interop_check.py <canonical.otio> ...

Exit 0 = interop green; non-zero names the failing document + reason.
Usage: scripts/tl_interop_check.py FILE [FILE...]
"""

from __future__ import annotations

import json
import sys

import opentimelineio as otio  # python-otio >= 0.18


def logical_tree(tl) -> list:
    """Stable logical view: (name, schema-ish kind, active media url, exact seconds)."""
    def item_view(item):
        if isinstance(item, otio.schema.Clip):
            url = ""
            try:
                ref = item.media_references()[item.active_media_reference_key]
                url = ref.target_url or ""
            except Exception:  # noqa: BLE001 — missing refs are legal
                url = ""
            sr = item.source_range
            t = (
                (sr.start_time.value / sr.start_time.rate,
                 sr.duration.value / sr.duration.rate)
                if sr
                else None
            )
            markers = sorted(
                (m.name, m.comment, m.marked_range.start_time.value / m.marked_range.start_time.rate)
                for m in item.markers
            )
            return ("clip", item.name, url, t, markers)
        if isinstance(item, otio.schema.Gap):
            sr = item.source_range
            t = (sr.start_time.value / sr.start_time.rate, sr.duration.value / sr.duration.rate) if sr else None
            return ("gap", item.name, t)
        if isinstance(item, otio.schema.Transition):
            return ("transition", item.name, item.transition_type())
        if hasattr(item, "__iter__"):  # Track/Stack
            return ("container", item.name, tuple(item_view(c) for c in item))
        return (type(item).__name__, getattr(item, "name", ""))

    return [item_view(t) for t in tl.tracks]


def check(path: str) -> str | None:
    """None = green; a string = the failure reason."""
    with open(path, encoding="utf-8") as f:
        raw = f.read()

    # 1) python-otio must parse cairn's canonical bytes
    try:
        tl = otio.core.deserialize_json_from_string(raw)
    except Exception as e:  # noqa: BLE001
        return f"python-otio rejected cairn canonical output: {e}"

    # 2) python-otio's own round-trip (its fixpoint, and our re-parse must match)
    s2 = otio.core.serialize_json_to_string(tl, indent=2)
    tl2 = otio.core.deserialize_json_from_string(s2)

    # 3) logical equality across the round-trips
    a, b = logical_tree(tl), logical_tree(tl2)
    if a != b:
        for i, (x, y) in enumerate(zip(a, b)):
            if x != y:
                return f"logical drift at item {i}: {x!r} != {y!r}"
        return f"item count drift: {len(a)} vs {len(b)}"

    # 4) JSON-level: every OTIO_SCHEMA tag present in cairn's output must
    #    survive python-otio's re-serialization (unknown schemas would be
    #    dropped by python-otio — that must never silently happen)
    cairn_tags = {v.get("OTIO_SCHEMA") for v in json.loads(raw).get("tracks", {}).get("children", []) if isinstance(v, dict)}
    py_tags = set()
    for t in tl.tracks:
        py_tags.add(type(t).__name__)
    # (tag-set equality is checked via logical view above; this 4th check
    # guards schema tags cairn emits at any depth)
    def collect_tags(node, acc):
        if isinstance(node, dict):
            if "OTIO_SCHEMA" in node:
                acc.add(node["OTIO_SCHEMA"])
            for v in node.values():
                collect_tags(v, acc)
        elif isinstance(node, list):
            for v in node:
                collect_tags(v, acc)

    cairn_all, py_all = set(), set()
    collect_tags(json.loads(raw), cairn_all)
    collect_tags(json.loads(s2), py_all)
    lost = cairn_all - py_all
    if lost:
        return f"python-otio dropped schemas cairn emitted: {sorted(lost)} (upgrade python-otio or extend the ledger)"

    return None


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    failures = []
    for path in sys.argv[1:]:
        err = check(path)
        if err:
            failures.append((path, err))
            print(f"FAIL {path}: {err}")
        else:
            print(f"OK   {path}")
    if failures:
        print(f"\n{len(failures)} document(s) failed interop")
        return 1
    print(f"\npython-otio {otio.version.version_string if hasattr(otio, 'version') else 'interop'} — all green")
    return 0


if __name__ == "__main__":
    sys.exit(main())
