# ADR-0011: Device tokens — PASETO v4.public (ed25519) via pasetors

Date: 2026-08-31 · Status: Accepted

## Context
Spec §13/§17: PASETO/JWT device tokens, 90d rotation, scopes `sync|admin`, revocation on unlink,
keychain storage; crate hint "ed25519/paseto".

## Decision
- Device tokens are **PASETO v4.public** (ed25519-signed, unverifiable-less): the server signs
  with its enrollment signing key; every metadata-plane call verifies the token, checks
  `exp ≤ now` (90d rotation window), `device_id`, `tenant_id`, and `scopes`, then checks the
  device row's `revoked` flag and `token_hash` (blake3 of the token) against `devices`.
- Enrollment: admin/ctl issues a single-use enrollment code → `cairn login --code` presents it
  with a generated device keypair → server returns a signed PASETO bound to the device row →
  stored in the OS keychain (`keyring` crate). Plaintext-file fallback only behind an explicit
  dev flag, never default.
- Library: `pasetors` (pure Rust, v4.public with implicit assertions for protocol version
  binding: `{"v":4,"pkg":"cairn"}`).

## Consequences
- Stateless verification (asymmetric) keeps metadata servers horizontally scalable; revocation
  is the single DB read on top.
- Key rotation: `kid` claim + key table change requires an ops runbook (docs/runbooks/lease-
  restart.md covers server restart; token key rotation documented alongside).
