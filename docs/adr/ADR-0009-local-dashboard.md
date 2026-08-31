# ADR-0009: Local diagnostics dashboard served by the daemon (loopback-only HTTP gateway)

Date: 2026-08-31 · Status: Accepted (user-mandated exception)

## Context
Spec §3/§11: no UI of any kind; CLI + localhost gRPC ctl only. The product owner later mandated,
in the same instruction stream: "install taste-skill (after review) and make the UI … ensure
everything necessary is wired and production ready". The two are in direct conflict; the
working agreement says ambiguity resolves toward I1–I4 and gets recorded.

## Decision
1. Build **one** UI artifact: a local diagnostics dashboard (`cairn` daemon web UI), served by
   the daemon at `http://127.0.0.1:17778` — read-mostly status/journal/leases/snapshots/pin/
   recall/doctor views over the SAME service layer the ctl gRPC exposes. No cloud, no auth
   surface beyond loopback, no multi-user, no remote UI.
2. The ctl gRPC contract remains primary and frozen (`docs/ctl-api.md`). The HTTP gateway is a
   thin local JSON projection of it (`/api/v1/...`), documented in the same file as optional.
3. Security policy: bind loopback only; read-only endpoints open to loopback; mutating
   endpoints require the same daemon bearer token as gRPC (the dashboard receives a short-lived
   first-party session cookie when served, valid only for loopback requests — same trust
   boundary as Docker Desktop / VS Code local servers).
4. Design taste per installed `taste-skill` repo (review passed): its minimalist profile — warm
   monochrome + spot pastels, extreme typographic contrast, 1px borders, transform/opacity-only
   motion — applied to a dark monitoring surface. No build toolchain: static HTML/CSS/JS
   embedded in the daemon binary.

## Consequences
- The "no UI" non-goal is violated ONLY by this local, loopback, read-mostly dashboard, at the
  product owner's explicit direction; every other UI remains out of scope.
- The UI team's future product UI still builds against the frozen ctl contract exactly as §11
  intended; this dashboard is a reference client, not the product UI.
- Gateway endpoints that move the contract require the same ADR + ctl-api.md sync as gRPC.
