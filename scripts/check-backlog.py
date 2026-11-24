#!/usr/bin/env python3
"""CI gate for the review backlog (review #103).

Fails when:
  * docs/REVIEW-BACKLOG.md does not list every item 1..108 exactly once,
  * a row's status is not Done / Decided / Partial / Open / Blocked,
  * a Decided row has no written "Decision:" rationale,
  * an Open / Partial / Blocked row has no "Acceptance" (or, for Blocked,
    no stated blocker), i.e. an open item silently turned into prose,
  * a CONTRACT-DEBT.md item that is not marked CLOSED lacks an
    **Acceptance:** line.
Usage: python3 scripts/check-backlog.py
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
errors = []

rows = {}
for line in (ROOT / "docs/REVIEW-BACKLOG.md").read_text(encoding="utf-8").splitlines():
    m = re.match(r"^\| (\d+) \| (.*?) \| (\w+) \| (.*?) \| (.*) \|$", line)
    if not m:
        continue
    n, _title, status, _where, ev = int(m.group(1)), m.group(2), m.group(3), m.group(4), m.group(5)
    if n in rows:
        errors.append(f"item #{n} listed twice")
    rows[n] = (status, ev)
missing = sorted(set(range(1, 109)) - set(rows))
if missing:
    errors.append(f"items missing from the tracker: {missing}")
for n, (status, ev) in sorted(rows.items()):
    if status not in {"Done", "Decided", "Partial", "Open", "Blocked"}:
        errors.append(f"#{n}: unknown status {status!r}")
    elif status in {"Open", "Partial"} and "Acceptance" not in ev and "decision" not in ev:
        errors.append(f"#{n} is {status} but has no acceptance line")
    elif status == "Blocked" and not re.search(r"needs|requires", ev):
        errors.append(f"#{n} is Blocked but does not say what it needs")
    elif status == "Decided" and "Decision:" not in ev:
        errors.append(f"#{n} is Decided but gives no Decision: rationale")
    elif status == "Done" and not ev.strip():
        errors.append(f"#{n} is Done with no evidence")

debt = (ROOT / "docs/CONTRACT-DEBT.md").read_text(encoding="utf-8")
for m in re.finditer(r"^## (\d+)\. (.+?)$(.*?)(?=^## |\Z)", debt, re.M | re.S):
    n, title, body = m.group(1), m.group(2), m.group(3)
    if "CLOSED" in title:
        continue
    if "**Acceptance" not in body:
        errors.append(f"CONTRACT-DEBT #{n} is open but has no **Acceptance:** line")

if errors:
    print("backlog check FAILED:")
    for e in errors:
        print(f"  - {e}")
    sys.exit(1)
counts = {}
for status, _ in rows.values():
    counts[status] = counts.get(status, 0) + 1
print(f"backlog check OK: 108 items tracked {counts}")
