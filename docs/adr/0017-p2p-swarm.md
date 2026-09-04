# ADR-0017: P2P swarm transport — LAN-speed block sync, join-code gated

Date: 2026-09-04
Status: Accepted (implemented this round)
Supersedes: the SPEC §3 "No LAN/P2P sync" non-goal (user-mandated exception, amended there)
Related: SPEC §4 (three planes — the swarm is a four sub-plane that never REPLACES the cloud),
ADR-0011 (device identity — node identities are transport-level, distinct), ADR-0013 (zstd
dict cross-device — same distribution philosophy), THIRD_PARTY.md (orion, syncthing rows)

## Context

Every block hydrate today pays the object-store round trip, even when the
machine next to you — same editing bay, same 10 Gb switch — already holds the
block on disk. A 40 GB BRAW cache warming a second edit bay through the
bucket is latency for the editor and egress for the studio. Meanwhile
Frame.io-style review feedback lives in six tools that do not talk to each
other, and none of them reach the blocks.

What we refuse to become: a serverless chat network with no source of
truth. The cloud control plane stays authoritative for metadata (journal,
leases, cursors — §7/§8); the swarm is a transport overlay that makes
block acquisition fast and cheap, and nothing more.

## Decision

### 1. Architecture: one new crate, four moving parts (`crates/cairn-p2p`)

- **Signal server** (`signal.rs`) — a UDP rendezvous directory. Nodes
  register a "business card" (identity, X25519 public key, candidate
  addresses — never media bytes, never keys) and receive the other members'
  cards. Entries TTL out after 10 s; the 2 s re-register cadence keeps
  membership live. Runs wherever the host runs `cairn signal`.
- **NAT traversal** (`stun.rs` + swarm punch loop) — RFC 5389 binding
  client discovers the reflexive public address; the signal server's
  OBSERVED address doubles as a free second vantage; both peers fire
  simultaneous probes so home-router NATs open reciprocal mappings.
- **Relay fallback** (`relay.rs`) — for punch-proof firewalls: an
  encrypted pass-through that learns a route on first frame and forwards
  opaque authenticated ciphertext thereafter (it can route, never read).
  Idle routes are reaped atomically; `0.0.0.0` announcements are
  canonicalized to the observed IP (the bug that made relay datagrams
  unroutable).
- **Sessions** (`session.rs`) — after the public-key bootstrap HELLO, every
  datagram is XChaCha20-Poly1305 sealed under keys from an X25519 agreement
  over the two node identities, role-bound via BLAKE3 `derive_key` KDF
  contexts. Blocks stream `META → CHUNK[0..n] → EOF` with fragment
  reassembly, NAK retransmission, and paced sends (burst-safety without
  SO_RCVBUF tuning).
- **Swarm** (`swarm.rs`) — the orchestrator: rarest-first want assignment,
  Bloom-filter HAVE exchange (change-driven refresh, forced on HELLO and
  completion — the trio that keeps late joiners fed), receiver idle-NAK,
  want re-request at 750 ms, holder rotation on DENY or corrupt bytes.

### 2. Security: join-code admission (§7 of this ADR — the user's ask)

The swarm is a **private room, not a public lobby**:

- The host runs `cairn signal`; a **join code** is generated (or pinned
  via `--join-code`). The code is 18 CSPRNG bytes (144 bits) + CRC-16/ARC,
  Crockford Base32 — 32 symbols in 8 dash-separated groups, alphabet
  excluding `I`/`L`/`O`/`U`; input aliases `I→1 L→1 O→0`.
- The cluster key is **derived, not the code**:
  `blake3::derive_key("cairn-p2p-join/v1", code_bytes)`. The code itself
  never travels on the wire; registrations are HMAC-SHA256'd under the
  derived key.
- **Joining requires the host's code**: `cairn daemon --swarm-signal
  <host> --swarm-join-code <code>` — no code, no registration, and a node
  that cannot register never appears in any member's signal table, so
  every peer fail-closes its session HELLO. Strangers cannot establish so
  much as a session, let alone fetch blocks.
- **Hosting creates your own code**: `cairn signal` prints the fresh code
  and the exact command others should run — share it only with the people
  who may join.
- **Two-layer validation**: the CRC catches typos locally, instantly, with
  an actionable message; a wrong-but-well-formed code is dropped
  SILENTLY by the server — no reply, no oracle, indistinguishable from
  "server down" for code-probers. Three consecutive registration failures
  log a loud diagnosis naming both causes (unreachable OR wrong code).
- **Revocation by rotation**: restart `cairn signal --join-code <new>`
  and every node holding the old code is locked out at its next
  registration. (Fine-grained per-device revocation stays a cloud-plane
  concern — device tokens, ADR-0011 — not a swarm concern.)
- The daemon persists the join code in the user-private home store
  (`swarm/join-code`, same pattern as `ctl/addr`) so restarts rejoin
  without re-passing flags; it is never logged.
- `--dev-key` / `--swarm-dev-key` keep a well-known smoke-test path on
  BOTH sides of the handshake; production flags refuse to combine them
  with real codes.

End-to-end invariants preserved: every fetched block is BLAKE3-verified
before landing (I2 — a corrupting peer cannot poison the CAS; it is
rotated off for that hash instead); block content is sealed under session
keys the relay cannot derive; the cloud plane remains the authority for
everything except raw block bytes.

### 3. Peer-first hydration (the point of it all)

`cairn-sync` grows a `PeerSource` trait (`peer.rs`):
`peer_may_have` (fast Bloom pre-check) → `fetch_peer_block` →
`warm_blocks` (pre-walk). `materialize_missing` consults peers BEFORE the
plane; `None` always means "fall back to the cloud" — a peer that merely
lacks the block answers fast. The warm pre-walk walks the manifest before
the sequential hydrate loop so parallel fetches overlap the walk.
Plane-only behavior is unchanged when the daemon has no swarm (sim, tests).

### 4. What the swarm is NOT

- Not a metadata plane: journal, leases, cursors, conflict copies stay on
  the cloud server. The swarm never decides editorial truth.
- Not a content store: it serves only blocks the local CAS already
  verifies; it never invents or trusts unverified bytes.
- Not a discovery free-for-all: no code, no membership, no session.

## Testing evidence

- `cairn-p2p`: 50 unit tests (signal HMAC/project isolation/relay grants,
  STUN codec round-trips IPv4+IPv6, join-code codec incl. every-position
  typo rejection + CRC known vectors, session seal/open/fragment/NAK
  matrix) + 7 e2e suites: three-node convergence, mesh effect (late joiner
  pulls from two holders), relay fallback under forced-relay, fetch-None
  contract, corrupt-peer rejection, STUN plumbing, and
  **join_code_gate_stranger_never_joins** (two members link + sync; a
  third node with a different valid code links nothing, learns nobody,
  fetches nothing — the private-room contract, executed).
- Real bugs the rebuild itself caught and fixed (now pinned by tests):
  register self-deadlock (signal replies starved when awaited in the main
  select arm), bare hellos unroutable through the relay, relays invisible
  outside their project (now infrastructure), handshaking with the relay
  node itself, missing relay-hello retry in the learning phase.
- `cairn-cli`/`cairn-sync`: peer-first hydrate integration exercised by
  the daemon path; sim stays plane-only by explicit `None`.

## Consequences

- New crate `cairn-p2p` (~3.9k lines incl. tests); zero new external
  dependencies (orion was already in the tree via pasetors; blake3/hmac/
  sha2/tokio are workspace staples — THIRD_PARTY.md updated).
- `cairn signal` and `cairn daemon --swarm-signal/--swarm-join-code`
  become the operational surface; runbook-beta.md carries the studio
  sequence.
- SPEC §3 non-goal amended (user exception, mirroring the ADR-0009 UI
  exception pattern); ADR index updated.
- UDP 17780 (signal) / 17781 (relay) join the port map; both are
  optional deployments — nothing about the cloud path changes when absent.
