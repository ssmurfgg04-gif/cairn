# Runbook: WAN peer-to-peer (two studios across the internet)

Round 18 closed a wiring gap, not a design gap: the swarm has had STUN
discovery, UDP hole punching, and a relay fallback since round 15
(ADR-0017) — but the CLI never turned NAT discovery on (`SwarmConfig.stun`
was `None`). With defaults wired, this is the Mumbai ⇄ London path.

## Topology

```text
studio A (NAT₁)                      studio B (NAT₂)
  daemon ── swarm ── UDP ────────────── swarm ── daemon
    │            \                    /            │
    │             [signal+relay VPS]              │
    │               cairn signal :17780/:17781    │
    └── storage server (cloud, cold tier) ────────┘
```

The signal server is the rendezvous (ids, keys, candidates, relay
grants). The relay only carries frames when punching fails. The storage
server stays the durability tier either way.

## 1. The VPS (signal + relay)

```sh
# any ~$5/mo box, UDP 17780+17781 open in its firewall
cairn signal --bind 0.0.0.0:17780 --relay-bind 0.0.0.0:17781
# prints a fresh join code: PCJR-K7Q2-VN8X-4MT6  (share it out of band)
```

The join code is the admission credential (122 CSPRNG bits + CRC;
`blake3`-derived cluster key — ADR-0017 §7). Peers without it are
dropped silently, by design.

## 2. Each studio's daemon

```sh
cairn daemon --swarm-signal <vps-ip>:17780 \
             --swarm-join-code PCJR-K7Q2-VN8X-4MT6
```

STUN discovery now defaults ON (stun.cloudflare.com, then two more —
one UDP datagram per attempt, the same egress class as the signal
server). To pin or disable:

```sh
cairn daemon --swarm-stun stun.example.com:3478   # persisted in swarm/stun
cairn daemon --swarm-no-stun                      # signal-observed only
```

## 3. What to expect per NAT class

| NAT (both sides) | Path | Note |
| --- | --- | --- |
| full-cone / restricted-cone | **direct UDP** after the punch probes | the 200 Mbps-class path, zero cloud egress for blocks |
| port-restricted | usually direct (both peers probe) | the punch retries on a 250 ms cadence |
| symmetric (either side) | **relay-routed** (the lexicographically-lower node requests a grant) | throughput = VPS bandwidth; correctness unchanged |

Relay engagement is logged (`relay fallback engaged`), and the swarm
stats surface `direct_links` vs `relay_links` per node.

## 4. Verification

```sh
# on either studio machine — peers, links, blocks served/fetched
cairn status --json          # swarm section: peers, direct/relay links

# deterministic WAN simulation without a VPS (loopback STUN + relay):
cargo test -p cairn-p2p      # stun round-trips, punch, relay, e2e
```

## 5. Failure modes

* **No mDNS on WAN**: mDNS discovery is LAN-only (ADR-0019 §4) — WAN
  always takes `--swarm-signal` (or a VPN that flattens the LAN).
* **Both sides behind symmetric NATs**: expect relay; if the VPS is
  bandwidth-capped, meter it (the relay counts `forwarded` bytes).
* **Join code mistyped**: indistinguishable from an unreachable signal
  server, by design — the register retry warning names both causes.
