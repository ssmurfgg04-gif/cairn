# ADR-0024: Round 21 — the Cloudflare memory lesson (scratch buffers + a CI-gated allocation budget)

Date: 2026-09-05
Status: accepted (Round 21)

## Context

Cloudflare published "How we saved 100 terabytes of memory by optimizing
1.1.1.1" (blog.cloudflare.com/dns-cache-memory-optimization-1111, Aug
2026): five Rust-level changes to a DNS cache's entry layout cut the
per-entry footprint 56% (953 → 420 bytes), freed ~100 TB across their
fleet, and made the cache FASTER (insert +43%, lookup −19%). The five
techniques, and the honest audit of each against cairn's actual memory
shapes:

| # | Cloudflare's change | Cairn audit |
|---|---|---|
| 1 | `Box<[T]>`/`Box<str>` instead of `Vec<T>`/`String` for entries that are frozen once stored (the capacity word + growth slack are pure waste) | **Considered, not applicable at scale.** Cairn's persistent indexes live in SQLite (files/blobs/dir_headers tables); the in-memory caches are either transient (serving headers) or hard-capped (presence map ≤ peers). The `HashMap<Vec<u8>, (Instant, Vec<u8>)>` presence key is a Vec, but at a 2–10 peer cap the Box conversion saves single-digit KB — not worth the API churn. |
| 2 | Merge parallel lists into one buffer + u16 offsets; pack bools into bitflags (alignment/padding wins) | **Considered, no parallel-list shape to merge.** `PeerMsg`'s sections are flat fields, not twin lists. |
| 3 | Drop the per-record owner when it equals the key (infer at read) | **Considered, small win only.** Presence events carry the project per event while the hub already knows the peer; at heartbeat cadence this is noise. |
| 4 | Box the rare large enum variants (a 144-byte NAPTR variant made every A record pay 120 bytes of padding) | **Audited with `size_of`: no bloat.** `PeerMsg`'s largest variant (Hello/Chunk/Nak) is 64 bytes — every variant carries `[u8; 32]` hashes, so all variants sit within one size class. Boxing would save 16 bytes on two variants and add a deref on the hot path. Rejected. |
| 5 | Store the bulk payload as one contiguous wire buffer built in a REUSABLE scratch (one allocation instead of per-record boxing; +13% insert from this alone) | **APPLIED — see §1.** |
| — | The methodology: a counting allocator over the system allocator, per-entry footprint measured on production-shaped traffic, budgets gated in CI | **APPLIED — see §2.** This is the transferable core: cairn is not a 250-billion-entry fleet, but "a memory regression should fail a test, not a user's machine" is scale-free. |

## Decisions

### §1 Scratch buffers on the session seal path (their §5, our hot path)

`PeerSession::seal` previously allocated per message: a fresh encode
`Vec` (growing 64 → 1233 bytes for CHUNK payloads — up to five reallocs),
a fresh ciphertext `vec![0; pt+16]`, plus the returned datagram. Now the
plaintext and ciphertext live in per-session scratches that persist
across messages (`PeerMsg::encode_into` + `pt_scratch`/`ct_scratch`);
the returned datagram remains the ONE fresh allocation per message (it
is handed to the socket). Steady-state reallocs on the send path: zero.

### §2 The allocation budget gate (`tests/mem_budget.rs`)

A test-only `#[global_allocator]` wraps the system allocator with a
counting shim (allocs counted while armed; LIVE is a signed always-on
delta so entries allocated inside the window and freed after disarm —
the draining send backlog — correctly decrement instead of reading as
phantom retention; that subtlety cost two calibration rounds to get
right and is documented inline).

The gate floods 2000 presence events through a REAL encrypted session
(broadcast → seal → UDP → open → decode → presence-map update → channel
fanout) and asserts:

- **allocations per event ≤ 32** (measured shape: 9–12, including the
  test's own `format!` — the budget is the regression tripwire, not a
  micro-opt target);
- **steady-state live-byte delta ≤ 64 KiB** after drain (measured:
  13–269 BYTES — the map is last-event-wins, a signal not a log; any
  per-event accumulation lands here loudly);
- the map holds ONE entry per peer and a late event won.

Completion is QUISCENCE, not "all N landed" — UDP loss and broadcast
lag are the channel's documented contract (e2e.rs
`presence_flood_stays_bounded`: "lag-skips allowed"), and the test must
not assert a stronger delivery guarantee than the design makes.
Measurement validity is unaffected: every broadcast pays its send-path
allocations whether or not the datagram lands.

## Consequences

- Block transfer (thousands of CHUNK seals per block) no longer pays
  per-message growth reallocs; presence heartbeats ride the same path.
- A future change that clones a big struct per presence event, appends
  to the presence map instead of replacing, or reintroduces per-message
  buffer allocation fails CI with a message pointing here.
- The rejected techniques are recorded above so the next reader does
  not re-audit them blind; if the store ever moves its hot index from
  SQLite into a large in-memory map, techniques #1–#4 become live
  candidates again and this ADR is the starting map.
