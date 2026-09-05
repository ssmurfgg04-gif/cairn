#!/usr/bin/env bash
# WO6-UI dashboard smoke: boot the REAL stack (server + daemon), exercise every
# dashboard endpoint and action through the loopback HTTP gateway.
#
# The one assertion that caught the round-6 mystery: a snapshot created AFTER files
# synced MUST carry snapshot_seq >= 1 (fold reads MAX(seq) FROM journal). A snapshot
# folded from an EMPTY journal legitimately carries seq 0 (journal cursor at fold
# time — the unit test fold_materializes_and_cas_updates pins the non-empty case).
set -u
cd "$(dirname "$0")/.."
BIN="$PWD/target/debug/cairn"
WORK="${WORK:-$PWD/.dashboard-smoke}"
SRV="127.0.0.1:7443"; OBJ="127.0.0.1:7444"
CTL="127.0.0.1:17777"; UI="127.0.0.1:17778"
SRV_HOME="$WORK/server"; A_HOME="$WORK/devA"; ROOT="$WORK/rootA"
UI_PORT=17778
PASS=0; FAIL=0; S_PID=""; D_PID=""
say(){ echo "[dash-smoke] $*"; }
chk(){ if [ "$1" = ok ]; then PASS=$((PASS+1)); echo "[dash-smoke]   PASS $2"; else FAIL=$((FAIL+1)); echo "[dash-smoke]   FAIL $2 ($3)"; fi }
cleanup(){ for p in "$S_PID" "$D_PID"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
wait_port(){ for _ in $(seq 1 60); do python3 -c "import socket;s=socket.socket();s.settimeout(0.4);exit(0 if s.connect_ex(('127.0.0.1',$1))==0 else 1)" && return 0; sleep 0.25; done; return 1; }
jget(){ python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
path=sys.argv[1].split('.')
v=d
for p in path:
    if p.endswith(']'):
        idx=int(p[p.index('[')+1:-1]); p=p[:p.index('[')]
        v=v.get(p,[]) if isinstance(v,dict) else v
        v=v[idx] if isinstance(v,list) and len(v)>idx else {}
    else:
        v=v.get(p) if isinstance(v,dict) else None
    if v is None: break
print(v if not isinstance(v,(dict,list)) else json.dumps(v))
" "$1"; }

[ -x "$BIN" ] || { say "missing $BIN — cargo build -p cairn-cli first"; exit 2; }
rm -rf "$WORK"; mkdir -p "$SRV_HOME" "$A_HOME" "$ROOT"

# fixture: 6 small files (2 media-ish binaries, 2 xml, 2 text)
python3 - "$ROOT" <<'PY'
import sys, random, os
root = sys.argv[1]; random.seed(7)
os.makedirs(f"{root}/seq", exist_ok=True)
data = bytes(random.getrandbits(8) for _ in range(4096)) * 1024
open(f"{root}/seq/master.braw","wb").write(data)
open(f"{root}/seq/proxy.mov","wb").write(data[:1<<20])
open(f"{root}/scene.prproj.xml","w").write('<project>'+''.join(f'<c id="{i}"/>' for i in range(500))+'</project>')
open(f"{root}/seq/timeline.prproj","w").write('<timeline/>'*100)
open(f"{root}/notes.txt","w").write("dashboard smoke\n"*100)
open(f"{root}/seq/edl.txt","w").write("1\n2\n3\n")
PY

# ---------- boot ----------
say "booting server + daemon"
"$BIN" server --data-dir "$SRV_HOME" --grpc-addr "$SRV" --objects-addr "$OBJ" --dev-insecure >"$WORK/server.log" 2>&1 &
S_PID=$!
wait_port 7443 || { say "server did not listen"; exit 1; }
CAIRN_HOME="$A_HOME" "$BIN" daemon --ctl-addr "$CTL" --ui-addr "$UI" >"$WORK/daemon.log" 2>&1 &
D_PID=$!
wait_port 17777 && wait_port $UI_PORT || { say "daemon did not listen"; exit 1; }

CODE=$(CAIRN_HOME="$A_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$A_HOME" "$BIN" login --server "$SRV" --code "$CODE" --name smoke-A >>"$WORK/daemon.log" 2>&1

B="http://127.0.0.1:$UI_PORT"
# ---------- UI served ----------
HTML=$(curl -sf "$B/" | head -c 4000)
echo "$HTML" | grep -q "cairn" && chk ok "UI index served" || chk fail "UI index served" "no cairn in html"
curl -sf -o /dev/null "$B/assets/app.css" && chk ok "css served" || chk fail "css served" "—"
curl -sf -o /dev/null "$B/assets/app.js" && chk ok "js served" || chk fail "js served" "—"

# ---------- attach via the DASHBOARD itself ----------
R=$(curl -sf -X POST "$B/api/v1/attach" -H 'Content-Type: application/json' \
     -d "{\"root_path\": \"$ROOT\"}")
echo "$R" | grep -q '"ok":true' && chk ok "attach via dashboard" || chk fail "attach via dashboard" "$R"
PROJECT=$(echo "$R" | jget "project_id")
say "project: $PROJECT"

# wait for initial sync to settle (6 files)
SYNCED=0
for _ in $(seq 1 60); do
  SYNCED=$(curl -sf "$B/api/v1/projects" | jget "projects[0].files_synced" 2>/dev/null || echo 0)
  [ "$SYNCED" = "6" ] && break
  sleep 1
done
[ "$SYNCED" = "6" ] && chk ok "6 files synced" || chk fail "6 files synced" "got $SYNCED"

# ---------- read endpoints ----------
curl -sf "$B/api/v1/status" | grep -q '"healthy":true' && chk ok "status healthy" || chk fail "status healthy" "—"
curl -sf "$B/api/v1/feed" | grep -q '"activity":\[' && chk ok "feed reachable" || chk fail "feed reachable" "—"
curl -sf "$B/api/v1/leases" | grep -q '"leases":\[' && chk ok "leases reachable" || chk fail "leases reachable" "—"
S=$(curl -sf "$B/api/v1/storage")
echo "$S" | grep -q '"blobs"' && chk ok "storage endpoint" || chk fail "storage endpoint" "$S"
BLOBS=$(echo "$S" | jget "blobs.count")
[ "${BLOBS:-0}" -gt 0 ] && chk ok "storage shows $BLOBS blobs" || chk fail "storage blobs > 0" "got $BLOBS"

# ---------- round 27: volumes + file quick-actions ----------
echo "$S" | grep -q '"volumes":\[' && chk ok "storage lists volumes" || chk fail "storage lists volumes" "$S"
VOL=$(echo "$S" | jget "volumes[0].total_bytes")
[ -n "$VOL" ] && [ "$VOL" -gt 0 ] 2>/dev/null && chk ok "volume telemetry is real ($VOL bytes)" || chk fail "volume telemetry" "got ${VOL:-none}"
R=$(curl -sf "$B/api/v1/pick-folder")
echo "$R" | grep -q '"ok":true' && chk ok "pick-folder answers (linux: unsupported)" || chk fail "pick-folder answers" "$R"
R=$(curl -sf -X POST "$B/api/v1/file/open" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"path\": \"notes.txt\"}")
# ok:true needs xdg-open (present on a desktop, absent on a headless runner):
# the GATE is that the endpoint resolves the path and answers honest JSON
echo "$R" | grep -q '"ok":' && chk ok "file/open answers (reveal is desktop-dependent)" || chk fail "file/open" "$R"
# download: bytes must round-trip exactly
D=$(curl -sf "$B/api/v1/file/download?project=$PROJECT&path=notes.txt" | md5sum | cut -d' ' -f1)
M=$(md5sum "$ROOT/notes.txt" | cut -d' ' -f1)
[ -n "$D" ] && [ "$D" = "$M" ] && chk ok "file/download round-trips" || chk fail "file/download round-trips" "$D vs $M"
# traversal refusal: ../ must be refused
CODE_HTTP=$(curl -s -o /dev/null -w "%{http_code}" "$B/api/v1/file/download?project=$PROJECT&path=..%2F..%2Fetc%2Fpasswd")
[ "$CODE_HTTP" = "400" ] && chk ok "download refuses traversal" || chk fail "download refuses traversal" "HTTP $CODE_HTTP"
R=$(curl -sf -X POST "$B/api/v1/file/duplicate" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"path\": \"notes.txt\"}")
echo "$R" | grep -q '"ok":true' && chk ok "file/duplicate copies" || chk fail "file/duplicate" "$R"
DUP=$(echo "$R" | jget "path")
[ -f "$ROOT/$DUP" ] && chk ok "duplicate landed beside the original ($DUP)" || chk fail "duplicate on disk" "$ROOT/$DUP"

# ---------- doctor + flags ----------
curl -sf "$B/api/v1/doctor" | grep -q '"checks":\[' && chk ok "doctor endpoint" || chk fail "doctor endpoint" "—"
# bootstrap the acting device as OWNER (the smoke's flag-flip + restore
# legs are owner-gated by design; the pre-round-27 script failed them
# because a default editor cannot ManageFlags/Restore — RBAC working
# AS DESIGNED, the fixture was wrong, not the engine)
DEV=$(curl -sf "$B/api/v1/team" | jget "projects[0].my_device")
if [ -n "$DEV" ] && [ "$DEV" != "None" ]; then
  mkdir -p "$ROOT/.cairn"
  python3 - "$ROOT/.cairn/members.json" "$DEV" <<'PY'
import json, sys
path, dev = sys.argv[1], sys.argv[2]
doc = {"members": {dev: {"device_id": dev, "name": "smoke-owner",
                          "role": "owner", "added_at_ms": 1, "added_by": "bootstrap"}}}
open(path, "w").write(json.dumps(doc, indent=2))
PY
  chk ok "owner members.json bootstrapped ($DEV)"
else
  chk fail "owner bootstrap" "team endpoint returned no device"
fi
R=$(curl -sf -X POST "$B/api/v1/flags" -H 'Content-Type: application/json' -d '{"name":"normalize_containers","value":"false"}')
echo "$R" | grep -q '"ok":true' && chk ok "flag flip endpoint" || chk fail "flag flip endpoint" "$R"

# ---------- snapshots: THE seq assertion ----------
R=$(curl -sf -X POST "$B/api/v1/snapshots" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"label\": \"dash-smoke\"}")
echo "$R" | grep -q '"commit_hash"' && chk ok "snapshot created" || chk fail "snapshot created" "$R"
SL=$(curl -sf "$B/api/v1/snapshots?project=$PROJECT")
SEQ=$(echo "$SL" | jget "snapshots[0].snapshot_seq")
if [ -n "$SEQ" ] && [ "$SEQ" -ge 1 ] 2>/dev/null; then
  chk ok "snapshot_seq >= 1 after sync (got $SEQ)"
else
  chk fail "snapshot_seq >= 1 after sync" "got seq=${SEQ:-<none>} — fold saw an EMPTY journal"
fi

# ---------- pins + recall ----------
R=$(curl -sf -X POST "$B/api/v1/pins" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"path\": \"seq/master.braw\"}")
echo "$R" | grep -q '"ok":true' && chk ok "pin via dashboard" || chk fail "pin via dashboard" "$R"
curl -sf "$B/api/v1/pins?project=$PROJECT" | grep -q 'master.braw' && chk ok "pin listed" || chk fail "pin listed" "—"
R=$(curl -sf -X POST "$B/api/v1/recall" -H 'Content-Type: application/json' -d "{\"project_id\": \"$PROJECT\"}")
JOB=$(echo "$R" | jget "job_id")
[ -n "$JOB" ] && [ "$JOB" != "None" ] && chk ok "recall started ($JOB)" || chk fail "recall started" "$R"
JSTATE=""
for _ in $(seq 1 30); do
  JSTATE=$(curl -sf "$B/api/v1/recall/$JOB" | jget "state" 2>/dev/null)
  { [ "$JSTATE" = "completed" ] || [ "$JSTATE" = "failed" ]; } && break
  sleep 1
done
[ "$JSTATE" = "completed" ] && chk ok "recall completed" || chk fail "recall completed" "state=$JSTATE"
R=$(curl -sf -X POST "$B/api/v1/pins/unpin" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"path\": \"seq/master.braw\"}")
echo "$R" | grep -q '"ok":true' && chk ok "unpin via dashboard" || chk fail "unpin via dashboard" "$R"

# ---------- restore ----------
COMMIT=$(curl -sf "$B/api/v1/snapshots?project=$PROJECT" | jget "snapshots[0].commit_hash")
R=$(curl -sf -X POST "$B/api/v1/snapshots/restore" -H 'Content-Type: application/json' \
     -d "{\"project_id\": \"$PROJECT\", \"commit_hash\": \"$COMMIT\"}")
echo "$R" | grep -q '"ok":true' && chk ok "restore via dashboard" || chk fail "restore via dashboard" "$R"

say "RESULT: $PASS pass, $FAIL fail"
[ "$FAIL" -eq 0 ]
