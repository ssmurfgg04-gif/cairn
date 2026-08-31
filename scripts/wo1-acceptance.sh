#!/usr/bin/env bash
# WO1 AttachRoot walking-skeleton acceptance harness (scriptable, CI-runnable).
#
# Gates:
#   1. attach a big mixed folder  -> `cairn status` shows the project, N files synced
#   2. kill -9 daemon mid-scan    -> restart -> scan resumes, ZERO duplicate journal entries
#   3. second device attaches     -> pulls -> converges byte-identical
#   4. edit one file on device B  -> device A sees it <5s; ONLY changed chunks uploaded
#   5. `cairn doctor` green afterward
#
# Env knobs:
#   SIZE_MB   mixed-folder target size in MiB (default 500; CI may lower)
#   WORK      scratch dir (default ./.wo1-acceptance)
#   TIMEOUT   per-gate wait in seconds (default 300)
set -u
cd "$(dirname "$0")/.."
BIN="$PWD/target/release/cairn"
SIZE_MB="${SIZE_MB:-500}"
WORK="${WORK:-$PWD/.wo1-acceptance}"
TIMEOUT="${TIMEOUT:-300}"
SRV_HOME="$WORK/server"; A_HOME="$WORK/devA"; B_HOME="$WORK/devB"
ROOT_A="$WORK/rootA"; ROOT_B="$WORK/rootB"; ROOT_C="$WORK/rootC"
SRV="127.0.0.1:7443"; OBJ="127.0.0.1:7444"
DB="$SRV_HOME/meta.db"
PASS=0; FAIL=0; A_PID=""; B_PID=""; S_PID=""

say(){ echo "[wo1] $*"; }
gate(){ if [ "$1" = ok ]; then PASS=$((PASS+1)); echo "[wo1] GATE $2: PASS ($3)"; else FAIL=$((FAIL+1)); echo "[wo1] GATE $2: FAIL ($3)"; fi }
cleanup(){ for p in "$A_PID" "$B_PID" "$S_PID"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

wait_port(){ for _ in $(seq 1 50); do python3 -c "import socket;s=socket.socket();s.settimeout(0.4);exit(0 if s.connect_ex(('127.0.0.1',$1))==0 else 1)" && return 0; sleep 0.2; done; return 1; }
status_json(){ CAIRN_HOME="$1" "$BIN" status --json 2>/dev/null; }
field(){ python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(''); sys.exit(0)
for p in d.get('projects',[]):
    if p.get('project_id')==sys.argv[1]: print(p.get(sys.argv[2],''))
print('')" "$1" "$2"; }
wait_state(){ # home project want_state [want_files]
  local i=0
  while [ "$i" -lt "$TIMEOUT" ]; do
    local st fs
    st=$(status_json "$1" | field "$2" state)
    fs=$(status_json "$1" | field "$2" files_synced)
    if [ "$st" = "$3" ] && { [ "$#" -lt 5 ] || [ "$fs" = "$4" ]; }; then return 0; fi
    if [ "$st" = "error" ]; then say "project $2 entered error state"; return 1; fi
    sleep 1; i=$((i+1))
  done
  return 1; }

[ -x "$BIN" ] || { say "missing $BIN — build release first (cargo build --release -p cairn-cli)"; exit 2; }
FREE_MB=$(df -Pk "$PWD" | tail -1 | awk '{print int($3/1024)}')
NEED_MB=$(( SIZE_MB * 3 + 600 ))
[ "$FREE_MB" -lt "$NEED_MB" ] && { say "abort: ${FREE_MB}MB free < ${NEED_MB}MB needed (corpus + 2 stores + server objects)"; exit 2; }
rm -rf "$WORK"; mkdir -p "$SRV_HOME" "$A_HOME" "$B_HOME" "$ROOT_A" "$ROOT_B" "$ROOT_C"

# ---------- fixture: deterministic mixed folder (media-ish + xml + text + dup pair) ----------
say "generating $SIZE_MB MiB mixed corpus in rootA"
python3 - "$ROOT_A" "$SIZE_MB" <<'PY'
import sys, os, random
root, total_mb = sys.argv[1], int(sys.argv[2])
random.seed(20260901)
os.makedirs(f"{root}/media", exist_ok=True); os.makedirs(f"{root}/seq/take1", exist_ok=True)
big = total_mb - 8
n=0
while big > 0:
    mb = min(96, big); big -= mb
    with open(f"{root}/media/clip_{n:03d}.mov","wb") as f:
        chunk = bytes(random.getrandbits(8) for _ in range(1024))*1024
        for _ in range(mb): f.write(chunk)
    n+=1
with open(f"{root}/seq/take1/scene.prproj.xml","w") as f:
    f.write('<?xml version="1.0"?>\n<project>\n' + "".join(f'  <clip id="{i}" src="media/clip_{i%n:03d}.mov"/>\n' for i in range(4000)) + '</project>\n')
with open(f"{root}/notes.txt","w") as f: f.write("wo1 acceptance fixture\n"*200)
data = bytes(random.getrandbits(8) for _ in range(1024))*3072
open(f"{root}/seq/take1/master.braw","wb").write(data)
open(f"{root}/seq/take2_proxy.braw","wb").write(data[:-4096]+b"PROXY"+data[-4096:])
PY
N_FILES=$(python3 -c "
import os
c=0
for dp,_,fns in os.walk('$ROOT_A'):
    c+=len(fns)
print(c)")
say "corpus ready: $N_FILES files"

# ---------- boot ----------
say "starting server + device A daemon"
env RUST_LOG=info "$BIN" server --data-dir "$SRV_HOME" --grpc-addr "$SRV" --objects-addr "$OBJ" --dev-insecure >"$WORK/server.log" 2>&1 &
S_PID=$!
wait_port 7443 || { gate fail boot "server did not listen on 7443"; exit 1; }
wait_port 7444 || { gate fail boot "objects endpoint did not listen on 7444"; exit 1; }
CAIRN_HOME="$A_HOME" env RUST_LOG=info "$BIN" daemon --ctl-addr 127.0.0.1:17777 --ui-addr 127.0.0.1:17778 >"$WORK/daemonA.log" 2>&1 &
A_PID=$!
wait_port 17777 || { gate fail boot "daemon ctl did not listen"; exit 1; }

CODE=$(CAIRN_HOME="$A_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$A_HOME" "$BIN" login --server "$SRV" --code "$CODE" --name device-A >>"$WORK/daemonA.log" 2>&1
[ -n "$CODE" ] || { gate fail boot "dev-enroll-code returned nothing"; exit 1; }

# ---------- GATE 1 ----------
say "GATE 1: attach rootA ($SIZE_MB MiB, $N_FILES files)"
CAIRN_HOME="$A_HOME" "$BIN" attach "$ROOT_A" --project p-main || { gate fail 1 "attach rejected"; exit 1; }
if wait_state "$A_HOME" p-main synced "$N_FILES"; then
  N=$(status_json "$A_HOME" | field p-main files_synced)
  gate ok 1 "status shows p-main synced, files_synced=$N"
else
  gate fail 1 "status never reached synced/$N_FILES (see $WORK/daemonA.log)"; exit 1
fi

# ---------- GATE 2 ----------
say "GATE 2: kill -9 daemon mid-scan (p-crash), restart, audit journal"
python3 - "$ROOT_C" <<'PY'
import sys, os, random
root=sys.argv[1]; random.seed(7)
os.makedirs(f"{root}/d1", exist_ok=True)
for i in range(48):
    with open(f"{root}/d1/f{i:02d}.mov","wb") as f:
        base=bytes(random.getrandbits(8) for _ in range(1024))*1024
        for _ in range(6): f.write(base)
PY
CAIRN_HOME="$A_HOME" "$BIN" attach "$ROOT_C" --project p-crash >>"$WORK/daemonA.log" 2>&1
sleep 0.4
kill -9 "$A_PID"; wait "$A_PID" 2>/dev/null || true; A_PID=""
say "daemon killed mid-scan (kill -9); restarting"
CAIRN_HOME="$A_HOME" env RUST_LOG=info "$BIN" daemon --ctl-addr 127.0.0.1:17777 --ui-addr 127.0.0.1:17778 >>"$WORK/daemonA.log" 2>&1 &
A_PID=$!
wait_port 17777 || { gate fail 2 "daemon restart failed"; exit 1; }
if wait_state "$A_HOME" p-crash synced 48; then
  AUDIT=$(python3 - "$DB" <<'PY'
import sqlite3,sys
db=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
dups=db.execute("SELECT COUNT(*) FROM (SELECT path FROM journal WHERE tenant_id='t1' AND project_id='p-crash' GROUP BY path HAVING COUNT(*)>1)").fetchone()[0]
ups=db.execute("SELECT COUNT(*) FROM journal WHERE tenant_id='t1' AND project_id='p-crash' AND path<>''").fetchone()[0]
print(f"{ups} {dups}")
PY
)
  UPS=${AUDIT% *}; DUP=${AUDIT#* }
  if [ "$DUP" = "0" ] && [ "$UPS" = "48" ]; then gate ok 2 "crash-resume clean: $UPS upserts, $DUP duplicate paths"
  else gate fail 2 "journal audit: upserts=$UPS dup_paths=$DUP (want 48/0)"; fi
else
  gate fail 2 "p-crash never synced after restart (see $WORK/daemonA.log)"
fi

# ---------- GATE 3 ----------
say "GATE 3: device B attaches empty folder for p-main, pulls, converge"
CODE_B=$(CAIRN_HOME="$B_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$B_HOME" "$BIN" login --server "$SRV" --code "$CODE_B" --name device-B >>"$WORK/daemonB.log" 2>&1
CAIRN_HOME="$B_HOME" env RUST_LOG=info "$BIN" daemon --ctl-addr 127.0.0.1:17779 --ui-addr 127.0.0.1:17780 >"$WORK/daemonB.log" 2>&1 &
B_PID=$!
wait_port 17779 || { gate fail 3 "device B daemon did not start"; exit 1; }
CAIRN_HOME="$B_HOME" "$BIN" attach "$ROOT_B" --project p-main --ctl http://127.0.0.1:17779 || { gate fail 3 "attach B rejected"; exit 1; }
if wait_state "$B_HOME" p-main synced "$N_FILES"; then
  HA=$(cd "$ROOT_A" && find . -type f -exec sha256sum {} \; | sort | sha256sum | cut -d' ' -f1)
  HB=$(cd "$ROOT_B" && find . -type f -exec sha256sum {} \; | sort | sha256sum | cut -d' ' -f1)
  if [ "$HA" = "$HB" ]; then gate ok 3 "second device converged byte-identical ($HA)"
  else gate fail 3 "tree hashes differ: A=$HA B=$HB"; fi
else
  gate fail 3 "device B never reached synced/$N_FILES (see $WORK/daemonB.log)"
fi

# ---------- GATE 4 ----------
say "GATE 4: edit one file on B; measure A visibility + upload delta"
TARGET="$ROOT_B/media/clip_000.mov"
TOTAL=$("$BIN" chunk-count "$TARGET")
T0=$(python3 -c "import time;print(int(time.time()*1000))")
dd if=/dev/urandom of="$TARGET" bs=65536 count=4 oflag=append conv=notrunc 2>/dev/null
HASH_B=$(sha256sum "$TARGET" | cut -d' ' -f1)
HASH_A="pending"
for _ in $(seq 1 60); do
  sleep 0.5
  if [ -f "$ROOT_A/media/clip_000.mov" ]; then
    HASH_A=$(sha256sum "$ROOT_A/media/clip_000.mov" | cut -d' ' -f1)
    if [ "$HASH_A" = "$HASH_B" ]; then break; fi
  fi
done
T1=$(python3 -c "import time;print(int(time.time()*1000))")
ELAPSED=$((T1-T0))
NEW_CHUNKS=$(python3 - "$DB" "$T0" <<'PY'
import sqlite3,sys
db=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
print(db.execute("SELECT COUNT(*) FROM chunks WHERE tenant_id='t1' AND last_touched>=?", (int(sys.argv[2]),)).fetchone()[0])
PY
)
if [ "$HASH_A" = "$HASH_B" ] && [ "$ELAPSED" -le 8000 ]; then
  gate ok 4a "A saw B's edit in ${ELAPSED}ms (hash-verified)"
else
  gate fail 4a "A visibility ${ELAPSED}ms (or hash mismatch: A=$HASH_A B=$HASH_B)"
fi
if [ "$NEW_CHUNKS" -ge 1 ] && [ "$NEW_CHUNKS" -lt "$TOTAL" ]; then
  gate ok 4b "metering: $NEW_CHUNKS new server chunks < $TOTAL file chunks (delta-only upload)"
else
  gate fail 4b "metering: new_chunks=$NEW_CHUNKS total=$TOTAL (want 1 <= new < total)"
fi

# ---------- GATE 5 ----------
say "GATE 5: doctor"
if CAIRN_HOME="$A_HOME" "$BIN" doctor >"$WORK/doctor.out" 2>&1; then
  gate ok 5 "doctor healthy (device A, after all gates)"
else
  gate fail 5 "doctor reported problems (see $WORK/doctor.out)"
fi

say "RESULT: $PASS passed, $FAIL failed — logs in $WORK"
[ "$FAIL" = "0" ]
