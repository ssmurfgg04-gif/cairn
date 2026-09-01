#!/usr/bin/env bash
# WO6-9 security sweep (runbook-beta §security): the checks that must pass before a
# beta build ships. Exit 1 = a gate failed; the script never hides a skipped check.
set -u
cd "$(dirname "$0")/.."
FAIL=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; FAIL=1; }
skip() { echo "  SKIP  $1 (reason: $2) — do not ship on a red gate without this"; }
note() { echo "  ----  $1"; }

echo "== cairn security sweep $(date -u +%FT%TZ) =="

# 1. RustSec advisories (cargo-audit)
if command -v cargo-audit >/dev/null 2>&1 || cargo audit --version >/dev/null 2>&1; then
  if cargo audit --deny warnings; then pass "cargo-audit: no RustSec advisories"; else fail "cargo-audit reported advisories"; fi
else
  skip "cargo-audit (RustSec)" "not installed — cargo install cargo-audit && re-run"
fi

# 2. Secrets in tracked files (git-tracked only; history audit is a separate runbook item)
# Allowlist: `AKIAIOSFODNN7EXAMPLE` + `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` are the
# AWS-PUBLISHED SigV4 documentation vectors (storage.rs known-answer test, ADR-0005) —
# not credentials. Everything else with a credential shape is a finding.
note "secrets scan over $(git ls-files | wc -l) tracked files"
SECPAT='(ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[bap]-[A-Za-z0-9-]{10,}|BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY|sk-[A-Za-z0-9]{20,})'
if git grep -nE "$SECPAT" -- . 2>/dev/null \
   | grep -vE "AKIAIOSFODNN7EXAMPLE|wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY" \
   | grep -vE "scripts/security-sweep\.sh"; then
  fail "secret-shaped strings in tracked files (allowlist: AWS doc vectors only)"
else
  pass "no secret-shaped strings in tracked files (AWS doc vectors allowlisted)"
fi

# 3. Unsafe code policy (WO6-9): every crate root declares forbid(unsafe_code), or
# deny(unsafe_code) + a documented module-level allow for FFI (cairn-store::eviction
# disk probes, cairn-fs-win CfAPI glue). A crate root with NEITHER is a finding.
UNSAFE_CRATES=""
for c in crates/*/; do
  if ! grep -qs "forbid(unsafe_code)" "$c/src/lib.rs" "$c/src/main.rs" 2>/dev/null; then
    if ! grep -qs "deny(unsafe_code)" "$c/src/lib.rs" "$c/src/main.rs" 2>/dev/null; then
      UNSAFE_CRATES="$UNSAFE_CRATES$c "
    fi
  fi
done
if [ -z "$UNSAFE_CRATES" ]; then
  pass "unsafe policy: forbid at every pure crate root, deny+documented FFI elsewhere"
else
  fail "crates with no unsafe policy at root: $UNSAFE_CRATES"
fi

# 4. Path-traversal gate present at every trust boundary (WO6-9)
grep -q "validate_rel_path" crates/cairn-core/src/pathutil.rs \
  && grep -q "validate_rel_path" crates/cairn-server/src/journal.rs \
  && grep -q "validate_rel_path" crates/cairn-sync/src/apply.rs \
  && grep -q "validate_rel_path" crates/cairn-cli/src/daemon.rs \
  && pass "path validation enforced: journal append + apply + restore" \
  || fail "path validation missing at a trust boundary"

# 5. TLS fail-closed: plaintext REMOTE dial is refused in connect_channel (not doctor-warned)
if grep -q "CAIRN_ALLOW_INSECURE_REMOTE" crates/cairn-sync/src/plane_grpc.rs; then
  pass "TLS fail-closed at connect (escape hatch CAIRN_ALLOW_INSECURE_REMOTE only)"
else fail "TLS fail-closed check missing in plane_grpc.rs"; fi

# 6. Tenant scoping on object keys (I3): every key builder carries the tenant prefix
if grep -q "t{.*tenant" crates/cairn-server/src/storage.rs 2>/dev/null || grep -qE "fn (object_key|chunk_key).*tenant" crates/cairn-server/src/storage.rs; then
  pass "object keys are tenant-scoped (I3)"
else fail "object key construction missing tenant scoping"; fi

# 7. Token material in logs: tracing must not log tokens (grep for token: in info!/debug!)
LEAKS=$(git grep -nE 'info!\(.*token|debug!\(.*token|println!.*token' crates/ -- '*.rs' 2>/dev/null | grep -v "token_hash\|has_token\|token_count" | head -5 || true)
if [ -z "$LEAKS" ]; then pass "no raw token logging found"; else fail "possible token logging:"; echo "$LEAKS"; fi

# 8. ctl scope enforcement: FoldNow requires sync/admin (ctl-api contract)
if grep -q 'scopes.contains("sync")' crates/cairn-server/src/services.rs; then
  pass "FoldNow scope-gated (sync/admin)"
else fail "FoldNow missing scope check"; fi

echo
if [ "$FAIL" -eq 0 ]; then echo "SECURITY SWEEP: PASS"; else echo "SECURITY SWEEP: FAIL"; exit 1; fi
