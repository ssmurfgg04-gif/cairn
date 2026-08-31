# Lease server restart runbook

Trigger: metadata server restart/failover with live NLE leases.

## Invariants that make this safe (SPEC §8)
- Lease tokens come from `projects.next_lease_token` — a DB sequence. Restart cannot reissue
  a lower token; fencing correctness never depends on the lease TABLE's liveness.
- Fencing is enforced at journal append: any append for a leased path must carry the current
  (device, token). A stale token is rejected with `STALE_LEASE` — the client surfaces it and
  re-acquires.
- TTL enforcement is a cleanup job; an expired lease row does not block appends (advisory
  model) until re-acquired.

## Procedure
1. Restart is crash-safe: `BEGIN IMMEDIATE` transactions mean every acknowledged append is
   durable; watch streams reconnect and re-cursor (watch is a HINT).
2. After boot: `SELECT COUNT(*) FROM leases WHERE expires_at < now` — cleanup job removes
   them; no operator action needed.
3. Clients: `cairn lease ls --project p1` to list live leases. If an editor reports
   STALE_LEASE after failover, they simply save again (re-acquire on open is automatic for
   NLE project files).

## Token rotation (enrollment signing keys)
Device tokens are PASETO v4.public; rotate the signing key by replacing
`keys/device-signing.key` (0600) and restarting — devices re-enroll via a new enrollment
code. Old tokens verify against the new key only if re-issued (enforced by kid/exp claims).
