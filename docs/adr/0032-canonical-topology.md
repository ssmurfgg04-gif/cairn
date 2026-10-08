# ADR-0032: The canonical Cairn runtime topology — one diagram, every mode mapped

Date: 2026-10-08 · Status: accepted · Scope: cairn-cli (daemon), cairn-server, cairn-p2p, cairn-review, cairn-tray, cairn-app

## Context

The review's deployment/topology finding (7.5/10): Cairn legitimately has
~10 runtime modes — local daemon, local server, WAN server, P2P swarm,
relay, review portal, dashboard, tray, native window, NLE panel — and
"every additional mode creates more combinations to reason about. What's
the canonical Cairn runtime topology? Right now that answer isn't quite
singular."

That ambiguity is not academic. It decides:

- **what a failure means** ("sync stalled" — is that the plane, the
  swarm, or the relay? the user can't tell, so neither can support);
- **what the tests must cover** (the matrix grows multiplicatively unless
  there is one reference shape the matrix reasons *from*);
- **what we promise** (BETA.md can only promise what the canonical shape
  guarantees; everything else is "supported deviation").

The code does not make the answer explicit anywhere — it is implied by
which flags the CLI takes.

## Decision

**The canonical topology is one daemon process per machine, loopback
console on top, two optional transports out:**

```text
                    ┌────────────────────────── one machine ──────────────────────┐
                    │                                                             │
  local folder  ←→  watcher/FUSE/CfAPI  ←→  cairn daemon (owns: ProjectManager,   │
                    │                              store + WAL + outbox, swarm)   │
                    │                                                             │
                    │   loopback-only surfaces (127.0.0.1, host/origin/token      │
                    │   gated, ADR-0009/0030):                                    │
                    │   ├── dashboard JSON + assets (browser)                     │
                    │   ├── review portal /r/:token (guests)                      │
                    │   ├── NLE marker bridge (Premiere panel)                    │
                    │   └── native window (cairn-app = viewport on the above)     │
                    │                                                             │
                    │   out-of-band UI: tray → CLI subprocess (ADR-0016/0029)     │
                    └──────────────┬────────────────────────────┬─────────────────┘
                                   │ optional leg 1             │ optional leg 2
                                   ▼                            ▼
                          cairn-server (plane)          P2P swarm (ADR-0017)
                          journal + CAS, authoritative   signal → STUN → punch →
                          acked durable state            relay fallback (QUIC, round 28)
```

Rules that make this *one* topology rather than ten:

1. **The daemon is the only thing that syncs.** Tray, window, panel,
   browser are clients of the daemon's loopback surfaces; the CLI is a
   thin gRPC client of the ctl contract. No second sync engine exists.
2. **Loopback surfaces are one trust zone** (ADR-0009 gates + ADR-0030
   service layer). Guest access crosses the zone only through the review
   portal's token routes — never through the dashboard.
3. **The server leg and the P2P leg are peers, not tiers.** Peer-first
   hydration with server fallback is the *same* store/journal/protocol
   through two transports (ADR-0017). A deployment runs 0..2 legs:
   plane-only is the supported minimal shape; swarm adds throughput.
4. **The relay is a swarm degradation, not a mode.** Nobody "deploys a
   relay"; a swarm that cannot punch uses one (round 28 QUIC uplink).
5. **The review portal is a loopback surface with a door.** `--review
   0.0.0.0` is the one documented way to widen it, token-gated per link,
   expiry+revoke enforced (mom-test P0); default is loopback-only.

**Deviations are named, not implied.** The supported set, each deviating
from the canonical shape by exactly one axis:

| Deviation | Axis | Where documented |
|---|---|---|
| `--review 0.0.0.0:17778` | widen loopback zone | ADR-0020, this §5 |
| `--swarm-signal` | add leg 2 | ADR-0017 |
| WAN server / relay VPS | relocate leg 1 | wan-p2p runbook |
| FUSE runner (CI) | daemon without UI legs | runbook-fuse-runner |

If a future PR adds a combination not expressible as one axis on this
table, that is a topology change and needs an ADR — that is the point.

## Consequences

**Good:** BETA.md and runbooks can say "the topology is X, except where
listed"; failure triage starts with "which leg broke" (loopback, plane,
swarm) instead of an echo chamber of flags; the test matrix grows
linearly (canonical × deviations) instead of multiplicatively.

**Costs:** none in code this round — the topology is descriptive of what
is already built; the cost is the discipline of the table.

**Verification:** each table row cites a runbook or ADR; `cairn daemon
--help` flags map 1:1 onto the deviation axes (checked this round).
