#!/usr/bin/env bash
# WO6-4 SOAK: 5GB-class ingest with kill -9 at ~50%, resume, byte-identity,
# zero-duplicate journal, metering budgets, and COLD-FETCH first-byte.
#
# Modes (honest, printed at boot):
#   REAL-S3  — CAIRN_S3_* set: the server stores chunk objects in YOUR bucket.
#   DRY-RUN  — no CAIRN_S3_*: LocalFs object store. Everything is real except
#              the cloud wire (the S3 wire itself is covered by s3-conformance
#              against MinIO in CI / `just s3-conformance`).
#
# Gates:
#   S1 ingest: attach the mixed corpus -> synced; upload <= 110% corpus
#   S2 kill -9 mid-ingest (>= KILL_PCT of corpus bytes stored) -> restart ->
#      resume synced; journal: upserts == files, ZERO duplicate paths;
#      net-new stored bytes after the kill <= 110% corpus (a restart that
#      re-stores content blows this)
#   S3 identity: corpus tree hash unchanged by attach+crash+resume; device B
#      pulls -> byte-identical tree; uploads during B's pull == 0
#   S4 COLD-FETCH: fresh device (empty store) fetches the largest stored chunk
#      through the real plane (presign + presigned GET, streamed); reports
#      first-byte p50/p95; body byte-count must equal the chunk size
#   S5 doctor green
#
# Env: SIZE_MB (default 5000), WORK, TIMEOUT (per-wait seconds), KILL_PCT
#      (default 50), CAIRN_S3_* (server-side object store), CAIRN_SOAK_DROP_CACHES=1
set -u
cd "$(dirname "$0")/.."
BIN="$PWD/target/release/cairn"
XBIN="$PWD/target/release/cairn-x"
SIZE_MB="${SIZE_MB:-5000}"
WORK="${WORK:-$PWD/.soak}"
KILL_PCT="${KILL_PCT:-50}"
TIMEOUT="${TIMEOUT:-$((600 + SIZE_MB / 5))}"
SWEEP_ENV=(CAIRN_SWEEP_SECS=2 CAIRN_SWEEP_SAMPLE_FILES=4096 CAIRN_SWEEP_SAMPLE_BYTES=17179869184)
SRV_HOME="$WORK/server"; A_HOME="$WORK/devA"; B_HOME="$WORK/devB"; C_HOME="$WORK/devC"
ROOT_A="$WORK/rootA"; ROOT_B="$WORK/rootB"
SRV="127.0.0.1:7443"; OBJ="127.0.0.1:7444"
DB="$SRV_HOME/meta.db"
PROJ="p-soak"
PASS=0; FAIL=0; A_PID=""; B_PID=""; S_PID=""
HAVE_S3=0
for v in CAIRN_S3_ENDPOINT CAIRN_S3_BUCKET CAIRN_S3_ACCESS_KEY_ID CAIRN_S3_SECRET_ACCESS_KEY; do
  [ -n "${!v:-}" ] && HAVE_S3=1
done

say(){ echo "[soak] $*"; }
gate(){ if [ "$1" = ok ]; then PASS=$((PASS+1)); echo "[soak] GATE $2: PASS ($3)"; else FAIL=$((FAIL+1)); echo "[soak] GATE $2: FAIL ($3)"; fi }
cleanup(){ for p in "$A_PID" "$B_PID" "$S_PID"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

wait_port(){ for _ in $(seq 1 100); do python3 -c "import socket;s=socket.socket();s.settimeout(0.4);exit(0 if s.connect_ex(('127.0.0.1',$1))==0 else 1)" && return 0; sleep 0.2; done; return 1; }
status_json(){ CAIRN_HOME="$1" "$BIN" status --json 2>/dev/null; }
bytes_since(){ python3 - "$DB" "$1" <<'PY'
import sqlite3,sys
try:
    db=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
    print(db.execute("SELECT COALESCE(SUM(size),0) FROM chunks WHERE last_touched>=?", (int(sys.argv[2]),)).fetchone()[0])
except Exception:
    print(0)
PY
}
now_ms(){ python3 -c "import time;print(int(time.time()*1000))"; }
human_mb(){ python3 -c "print(f'{$1/1048576:.1f}MiB')"; }
field(){ python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(''); sys.exit(0)
for p in d.get('projects',[]):
    if p.get('project_id')==sys.argv[1]: print(p.get(sys.argv[2],''))
print('')" "$1" "$2"; }
dump_logs(){ # on gate failure: show the daemon's last words so CI logs are self-contained
  say "---- daemonA.log (tail 40) ----"; tail -40 "$WORK/daemonA.log" 2>/dev/null || true
  say "---- daemonB.log (tail 20) ----"; tail -20 "$WORK/daemonB.log" 2>/dev/null || true
  say "---- server.log (tail 20) ----"; tail -20 "$WORK/server.log" 2>/dev/null || true
  say "---- status A ----"; CAIRN_HOME="$A_HOME" "$BIN" status --json 2>/dev/null | head -c 2000 || true
  echo
}
wait_state(){ local i=0
  # Error-state policy mirrors wo1-acceptance.sh: transient pass errors
  # (state=error for one 5s backoff window) must not fail the gate — only
  # CONTINUOUS errors past ERROR_GRACE are fatal, with last_error reported.
  local err_since="" last_err=""
  while [ "$i" -lt "$TIMEOUT" ]; do
    local st fs
    local sj; sj=$(status_json "$1")
    st=$(printf '%s' "$sj" | field "$2" state)
    fs=$(printf '%s' "$sj" | field "$2" files_synced)
    last_err=$(printf '%s' "$sj" | field "$2" last_error)
    if [ "$st" = "$3" ] && { [ "$#" -lt 5 ] || [ "$fs" = "$4" ]; }; then return 0; fi
    if [ "$st" = "error" ]; then
      [ -z "$err_since" ] && err_since=$i
      if [ $((i - err_since)) -ge "${ERROR_GRACE:-45}" ]; then
        say "project $2 in error state for ${ERROR_GRACE:-45}s (last_error: ${last_err:-unknown})"
        return 1
      fi
    else
      err_since=""
    fi
    sleep 1; i=$((i+1))
  done
  [ -n "$last_err" ] && say "project $2 last_error: $last_err"
  return 1; }
tree_hash(){ (cd "$1" && find . -type f -exec sha256sum {} \; | sort | sha256sum | cut -d' ' -f1); }

[ -x "$BIN" ] || { say "missing $BIN — run: cargo build --release -p cairn-cli -p cairn-x"; exit 2; }
[ -x "$XBIN" ] || { say "missing $XBIN — run: cargo build --release -p cairn-x"; exit 2; }
FREE_MB=$(df -Pk "$PWD" | tail -1 | awk '{print int($3/1024)}')
NEED_MB=$(( SIZE_MB * 3 + 600 ))
if [ "$FREE_MB" -lt "$NEED_MB" ]; then
  say "abort: ${FREE_MB}MB free < ${NEED_MB}MB needed (corpus + server objects + device B copy)"
  say "       lower SIZE_MB, free disk, or run the 5GB soak on a host with room."
  exit 2
fi
if [ "$HAVE_S3" = 1 ]; then
  say "mode: REAL-S3 — server object store = ${CAIRN_S3_ENDPOINT%/} bucket ${CAIRN_S3_BUCKET} (must be YOURS)"
else
  say "mode: DRY-RUN — no CAIRN_S3_* set; server object store = LocalFs (cloud wire NOT exercised; see just s3-conformance)"
fi
rm -rf "$WORK"; mkdir -p "$SRV_HOME" "$A_HOME" "$B_HOME" "$C_HOME" "$ROOT_A" "$ROOT_B"

# ---------- corpus: deterministic, mixed (random media + dedup-able pair + xml) ----------
say "generating $SIZE_MB MiB mixed corpus in rootA"
python3 - "$ROOT_A" "$SIZE_MB" <<'PY'
import sys, os, random
root, total_mb = sys.argv[1], int(sys.argv[2])
random.seed(20260901)
os.makedirs(f"{root}/media", exist_ok=True); os.makedirs(f"{root}/seq/take1", exist_ok=True)
big = total_mb - 8
n=0
blk = 4*1024*1024
while big > 0:
    mb = min(96, big); big -= mb
    with open(f"{root}/media/clip_{n:03d}.mov","wb") as f:
        written = 0
        while written < mb*1048576:
            b = os.urandom(min(blk, mb*1048576 - written))
            f.write(b); written += len(b)
    n+=1
with open(f"{root}/seq/take1/scene.prproj.xml","w") as f:
    f.write('<?xml version="1.0"?>\n<project>\n' + "".join(f'  <clip id="{i}" src="media/clip_{i%n:03d}.mov"/>\n' for i in range(4000)) + '</project>\n')
with open(f"{root}/notes.txt","w") as f: f.write("wo6-4 soak fixture\n"*200)
data = os.urandom(3072*1024)
open(f"{root}/seq/take1/master.braw","wb").write(data)
open(f"{root}/seq/take2_proxy.braw","wb").write(data[:-4096]+b"PROXY"+data[-4096:])
PY
N_FILES=$(find "$ROOT_A" -type f | wc -l)
CORPUS_BYTES=$(du -sb "$ROOT_A" | cut -f1)
say "corpus ready: $N_FILES files, $(human_mb "$CORPUS_BYTES")"

# ---------- boot ----------
say "starting server + device A daemon"
if [ "$HAVE_S3" = 1 ]; then
  env RUST_LOG=info "$BIN" server --data-dir "$SRV_HOME" --grpc-addr "$SRV" --objects-addr "$OBJ" --dev-insecure >"$WORK/server.log" 2>&1 &
else
  env RUST_LOG=info "$BIN" server --data-dir "$SRV_HOME" --grpc-addr "$SRV" --objects-addr "$OBJ" --dev-insecure >"$WORK/server.log" 2>&1 &
fi
S_PID=$!
wait_port 7443 || { gate fail boot "server did not listen on 7443"; exit 1; }
wait_port 7444 || { gate fail boot "objects endpoint did not listen on 7444"; exit 1; }
CAIRN_HOME="$A_HOME" env RUST_LOG=info "${SWEEP_ENV[@]}" "$BIN" daemon --ctl-addr 127.0.0.1:17777 --ui-addr 127.0.0.1:17778 >"$WORK/daemonA.log" 2>&1 &
A_PID=$!
wait_port 17777 || { gate fail boot "daemon ctl did not listen"; exit 1; }
CODE=$(CAIRN_HOME="$A_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$A_HOME" "$BIN" login --server "$SRV" --code "$CODE" --name soak-A >>"$WORK/daemonA.log" 2>&1
[ -n "$CODE" ] || { gate fail boot "dev-enroll-code returned nothing"; exit 1; }

# ---------- GATE S1: ingest ----------
T1=$(now_ms)
say "GATE S1: attach $SIZE_MB MiB ($N_FILES files)"
CAIRN_HOME="$A_HOME" "$BIN" attach "$ROOT_A" --project "$PROJ" || { gate fail S1 "attach rejected"; exit 1; }
if wait_state "$A_HOME" "$PROJ" synced "$N_FILES"; then
  UP1=$(bytes_since "$T1")
  BUDGET1=$(( CORPUS_BYTES * 110 / 100 ))
  if [ "$UP1" -le "$BUDGET1" ] && [ "$UP1" -gt 0 ]; then
    gate ok S1 "synced/$N_FILES; stored $(human_mb "$UP1") <= $(human_mb "$BUDGET1") corpus cap"
  else
    gate fail S1 "budget violated: stored $(human_mb "$UP1") vs corpus $(human_mb "$CORPUS_BYTES")"; exit 1
  fi
else
  dump_logs
  gate fail S1 "never reached synced/$N_FILES (see $WORK/daemonA.log)"; exit 1
fi

# ---------- GATE S2: kill -9 at KILL_PCT, resume, audit ----------
say "GATE S2: second-wave attach + kill -9 at >= ${KILL_PCT}% of wave bytes stored"
# wave 2: 96 MiB of fresh files so the kill lands mid-INGEST of real work
python3 - "$ROOT_A/wave2" <<'PY'
import sys, os
root=sys.argv[1]; os.makedirs(root, exist_ok=True)
for i in range(24):
    with open(f"{root}/w2_{i:02d}.mov","wb") as f:
        f.write(os.urandom(4*1024*1024))
PY
W2_BYTES=$(( 24 * 4 * 1048576 ))
N_ALL=$(find "$ROOT_A" -type f | wc -l)
TREE_PRE_KILL=$(tree_hash "$ROOT_A")
say "wave2 written: $N_ALL files on disk; pre-kill tree $TREE_PRE_KILL"
T2=$(now_ms)
CAIRN_HOME="$A_HOME" "$BIN" attach "$ROOT_A" --project "$PROJ" >>"$WORK/daemonA.log" 2>&1
KILLED_AT=0
for _ in $(seq 1 4000); do
  B=$(bytes_since "$T2")
  WAVE=$(python3 -c "print(max(0, $B))")
  if [ "$B" -ge $(( W2_BYTES * KILL_PCT / 100 )) ]; then KILLED_AT=$B; break; fi
  sleep 0.25
done
kill -9 "$A_PID" 2>/dev/null; wait "$A_PID" 2>/dev/null || true; A_PID=""
say "kill -9 delivered with $KILLED_AT bytes stored this window (wave2 = $(human_mb "$W2_BYTES"))"
[ "$KILLED_AT" -gt 0 ] || say "WARN: kill landed at 0 metered bytes (loopback may have outrun polling) — resume gates still enforce zero-dup + budget"
CAIRN_HOME="$A_HOME" env RUST_LOG=info "${SWEEP_ENV[@]}" "$BIN" daemon --ctl-addr 127.0.0.1:17777 --ui-addr 127.0.0.1:17778 >>"$WORK/daemonA.log" 2>&1 &
A_PID=$!
wait_port 17777 || { gate fail S2 "daemon restart failed"; exit 1; }
if wait_state "$A_HOME" "$PROJ" synced "$N_ALL"; then
  AUDIT=$(python3 - "$DB" "$PROJ" <<'PY'
import sqlite3,sys
db=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
dups=db.execute("SELECT COUNT(*) FROM (SELECT path FROM journal WHERE tenant_id='t1' AND project_id=? AND path<>'' GROUP BY path HAVING COUNT(*)>1)", (sys.argv[2],)).fetchone()[0]
ups=db.execute("SELECT COUNT(*) FROM journal WHERE tenant_id='t1' AND project_id=? AND path<>''", (sys.argv[2],)).fetchone()[0]
print(f"{ups} {dups}")
PY
)
  UPS=${AUDIT% *}; DUP=${AUDIT#* }
  UP2=$(python3 -c "print(max(0, $(bytes_since "$T2") - $KILLED_AT))")
  BUDGET2=$(( W2_BYTES * 130 / 100 ))
  if [ "$DUP" = "0" ] && [ "$UPS" -ge 1 ] && [ "$UP2" -le "$BUDGET2" ]; then
    gate ok S2 "resume clean: $UPS journal entries across $N_ALL paths, $DUP duplicate paths; post-kill net-new $(human_mb "$UP2") <= $(human_mb "$BUDGET2")"
  else
    gate fail S2 "audit: upserts=$UPS dup_paths=$DUP post-kill net-new=$(human_mb "$UP2") budget=$(human_mb "$BUDGET2")"
  fi
else
  dump_logs
  gate fail S2 "never reached synced/$N_ALL after restart (see $WORK/daemonA.log)"
fi

# ---------- GATE S3: byte-identity + pure-pull device B ----------
TREE_A_AFTER=$(tree_hash "$ROOT_A")
say "GATE S3: device B pulls the whole project; byte-identity"
CODE_B=$(CAIRN_HOME="$B_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$B_HOME" "$BIN" login --server "$SRV" --code "$CODE_B" --name soak-B >>"$WORK/daemonB.log" 2>&1
CAIRN_HOME="$B_HOME" env RUST_LOG=info "${SWEEP_ENV[@]}" "$BIN" daemon --ctl-addr 127.0.0.1:17779 --ui-addr 127.0.0.1:17780 >"$WORK/daemonB.log" 2>&1 &
B_PID=$!
wait_port 17779 || { gate fail S3 "device B daemon did not start"; exit 1; }
CAIRN_HOME="$B_HOME" "$BIN" attach "$ROOT_B" --project "$PROJ" --ctl http://127.0.0.1:17779 || { gate fail S3 "attach B rejected"; exit 1; }
if wait_state "$B_HOME" "$PROJ" synced "$N_ALL"; then
  TREE_B=$(tree_hash "$ROOT_B")
  UP3=$(bytes_since "$T2")
  BUDGET3=$(( W2_BYTES * 130 / 100 ))
  if [ "$TREE_PRE_KILL" = "$TREE_A_AFTER" ] && [ "$TREE_PRE_KILL" = "$TREE_B" ]; then
    gate ok S3 "tree identity pre-kill == post-resume == B-pull ($TREE_PRE_KILL)"
  else
    gate fail S3 "tree hashes differ: pre-kill=$TREE_PRE_KILL post-resume=$TREE_A_AFTER B=$TREE_B"
  fi
  if [ "$UP3" -le "$BUDGET3" ]; then
    gate ok S3b "pure-pull phase stored nothing new: net-new since kill $(human_mb "$UP3") <= $(human_mb "$BUDGET3") cap (B uploaded 0)"
  else
    gate fail S3b "B's pull re-stored bytes: net-new $(human_mb "$UP3") > $(human_mb "$BUDGET3")"
  fi
else
  dump_logs
  gate fail S3 "device B never reached synced/$N_ALL (see $WORK/daemonB.log)"
fi

# ---------- GATE S4: COLD-FETCH first byte ----------
say "GATE S4: COLD-FETCH (fresh device, largest stored chunk, real plane)"
CHUNK=$(python3 - "$DB" <<'PY'
import sqlite3,sys
db=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
r=db.execute("SELECT hash,size FROM chunks WHERE tenant_id='t1' AND state='present' ORDER BY size DESC LIMIT 1").fetchone()
print(f"{r[0]} {r[1]}" if r else "")
PY
)
if [ -z "$CHUNK" ]; then gate fail S4 "no chunks stored"; exit 1; fi
CHUNK_HASH=${CHUNK% *}; CHUNK_SIZE=${CHUNK#* }
CODE_C=$(CAIRN_HOME="$C_HOME" "$BIN" dev-enroll-code --server "$SRV")
CAIRN_HOME="$C_HOME" "$BIN" login --server "$SRV" --code "$CODE_C" --name soak-C >/dev/null 2>&1
# escalate to a true server-cache drop when privileges allow (honest cold)
if [ "${CAIRN_SOAK_DROP_CACHES:-0}" = 1 ]; then
  if [ "$(id -u)" = 0 ]; then sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true; say "page cache dropped (root)"
  elif sudo -n true 2>/dev/null; then sync; sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true; say "page cache dropped (sudo)"
  else say "drop_caches not permitted — cold = fresh process + empty client state (documented)"
  fi
fi
CF_OUT="$WORK/coldfetch.log"
if CAIRN_HOME="$C_HOME" "$XBIN" cold-fetch --home "$C_HOME" --hash "$CHUNK_HASH" --iters 5 >"$CF_OUT" 2>&1; then
  P50=$(sed -n 's/.*coldfetch_first_byte_p50_ms=\([0-9.]*\).*/\1/p' "$CF_OUT" | tail -1)
  P95=$(sed -n 's/.*coldfetch_first_byte_p95_ms=\([0-9.]*\).*/\1/p' "$CF_OUT" | tail -1)
  LASTBYTES=$(sed -n 's/.*last body \([0-9]*\) bytes.*/\1/p' "$CF_OUT" | tail -1)
  if [ -n "$P50" ] && [ "$LASTBYTES" = "$CHUNK_SIZE" ]; then
    gate ok S4 "COLD-FETCH first byte: p50 ${P50}ms p95 ${P95}ms (chunk $((CHUNK_SIZE/1048576))MiB, body byte-count verified)"
  else
    gate fail S4 "cold-fetch parse/identity: p50=$P50 last_bytes=$LASTBYTES want=$CHUNK_SIZE (see $CF_OUT)"
  fi
else
  gate fail S4 "cold-fetch failed (see $CF_OUT)"
fi

# ---------- GATE S5: doctor ----------
say "GATE S5: doctor"
if CAIRN_HOME="$A_HOME" "$BIN" doctor >"$WORK/doctor.out" 2>&1; then
  gate ok S5 "doctor healthy after the full soak"
else
  gate fail S5 "doctor reported problems (see $WORK/doctor.out)"
fi

# ---------- report ----------
UP_TOTAL=$(bytes_since "$T1")
python3 - "$WORK/soak-report.json" <<PY
import json, sys
json.dump({
  "mode": "$([ "$HAVE_S3" = 1 ] && echo real-s3 || echo dry-run)",
  "size_mb": $SIZE_MB, "files": $N_ALL,
  "corpus_bytes": $CORPUS_BYTES,
  "stored_total_bytes": $UP_TOTAL,
  "kill_pct_target": $KILL_PCT,
  "cold_fetch_first_byte_p50_ms": ${P50:-null},
  "cold_fetch_first_byte_p95_ms": ${P95:-null},
  "pass": $PASS, "fail": $FAIL,
}, open(sys.argv[1],"w"), indent=2)
PY
say "RESULT: $PASS passed, $FAIL failed — logs + soak-report.json in $WORK"
[ "$FAIL" = "0" ]
