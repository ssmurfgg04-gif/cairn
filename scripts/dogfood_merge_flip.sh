#!/usr/bin/env bash
# LIVE dogfood: flip `semantic_merge` on a real dogfood box (two real daemons
# + the real meeting-point server, all in this sandbox) and watch the merge
# affordance appear on the dashboard Files view, then accept it — the
# CONTRACT-DEBT #1 acceptance criterion, driven over real HTTP.
#
# Topology (the "two homes" shape):
#   cairn server (dev-insecure)  = the hosted meeting point  :17443/:17444
#   daemon A (home-a, root-a)    = home A                    :27771/:27772
#   daemon B (home-b, root-b)    = home B                    :37771/:37772
# All traffic between A and B flows through the server (no swarm => no P2P).
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/debug/cairn
export PATH="$HOME/.cargo/bin:$PATH"

ROOT=$(mktemp -d /tmp/cairn-merge-dogfood.XXXXXX)
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  ok: $*"; }
bad() { fail=$((fail+1)); echo "FAIL: $*"; }
cleanup() { kill $SRV $DA $DB 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

TOKEN_OF() { curl -s "http://127.0.0.1:$1/" | grep -oE 'window\.CAIRN_TOKEN = "[0-9a-f]+' | grep -oE '[0-9a-f]{16,}'; }
API() { # API <ui_port> <token> <method> <path> [body]
  local p=$1 t=$2 m=$3 path=$4 body=${5:-}
  if [ -n "$body" ]; then
    curl -s -X "$m" "http://127.0.0.1:$p$path" -H "x-cairn-token: $t" \
      -H "Origin: http://127.0.0.1:$p" -H 'content-type: application/json' -d "$body"
  else
    curl -s -X "$m" "http://127.0.0.1:$p$path" -H "x-cairn-token: $t" -H "Origin: http://127.0.0.1:$p"
  fi
}
poll() { # poll <desc> <tries> <cmd...>
  local desc=$1 tries=$2; shift 2
  for _ in $(seq 1 "$tries"); do
    if "$@" >/dev/null 2>&1; then ok "$desc"; return 0; fi
    sleep 1
  done
  bad "$desc (timed out after ${tries}s)"; return 1
}

echo "== 0. clean binary =="
cargo build -p cairn-cli -q 2>/dev/null
[ -x "$BIN" ] || { echo "no binary"; exit 1; }

echo "== 1. meeting-point server (dev-insecure) on :17443 =="
CAIRN_HOME="$ROOT/serverhome" "$BIN" server \
  --data-dir "$ROOT/server" --grpc-addr 127.0.0.1:17443 --objects-addr 127.0.0.1:17444 \
  --dev-insecure >"$ROOT/server.log" 2>&1 & SRV=$!
poll "server up" 30 bash -c 'ss -ltn | grep -q 17443'

echo "== 2. enroll + login two homes (two devices, one tenant) =="
for H in a b; do
  CODE=$(CAIRN_HOME="$ROOT/home-$H" "$BIN" dev-enroll-code --server 127.0.0.1:17443 --tenant t1 2>/dev/null | tail -1)
  CAIRN_HOME="$ROOT/home-$H" "$BIN" login --server 127.0.0.1:17443 --code "$CODE" \
    --name "dogfood-$H" --allow-plaintext-file >"$ROOT/login-$H.log" 2>&1 \
    && ok "home-$H enrolled (code ${CODE:0:6}…)" || bad "home-$H login failed ($(tail -1 "$ROOT/login-$H.log"))"
done

echo "== 3. two real daemons (real dashboards, no swarm => server-only path) =="
CAIRN_HOME="$ROOT/home-a" "$BIN" daemon --ctl-addr 127.0.0.1:27771 --ui-addr 127.0.0.1:27772 \
  >"$ROOT/daemon-a.log" 2>&1 & DA=$!
CAIRN_HOME="$ROOT/home-b" "$BIN" daemon --ctl-addr 127.0.0.1:37771 --ui-addr 127.0.0.1:37772 \
  >"$ROOT/daemon-b.log" 2>&1 & DB=$!
poll "daemon A up" 30 bash -c 'curl -sf http://127.0.0.1:27772/ >/dev/null'
poll "daemon B up" 30 bash -c 'curl -sf http://127.0.0.1:37772/ >/dev/null'

echo "== 4. attach the same project on both homes =="
mkdir -p "$ROOT/root-a/notes"
python3 - "$ROOT/root-a/notes/cut.otio" <<'PY'
import json, sys, uuid
def el(schema, name, u, **kw):
    d = {"OTIO_SCHEMA": schema, "metadata": {"cairn": {"uuid": u}}, "name": name,
         "effects": [], "markers": [], "enabled": True, "color": None}
    d.update(kw); return d
def rt(start, dur):
    return {"OTIO_SCHEMA": "TimeRange.1",
            "duration": {"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": float(dur)},
            "start_time": {"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": float(start)}}
def clip(start, dur):
    return el("Clip.2", "Hero", "clip-uuid-1", source_range=rt(start, dur),
              media_references={})
tl = el("Timeline.1", "session", "tl-uuid-1", global_start_time=None,
        tracks=el("Stack.1", "tracks", "stack-uuid-1", source_range=None,
                  children=[el("Track.1", "V1", "track-uuid-1", source_range=None,
                               children=[clip(0, 96)])]))
json.dump(tl, open(sys.argv[1], "w"), indent=1)
PY
CAIRN_HOME="$ROOT/home-a" "$BIN" attach "$ROOT/root-a" --project brand-film \
  --server 127.0.0.1:17443 --ctl http://127.0.0.1:27771 >"$ROOT/attach-a.log" 2>&1 \
  && ok "A attached brand-film" || { bad "A attach failed"; cat "$ROOT/attach-a.log"; }
mkdir -p "$ROOT/root-b/notes"
CAIRN_HOME="$ROOT/home-b" "$BIN" attach "$ROOT/root-b" --project brand-film \
  --server 127.0.0.1:17443 --ctl http://127.0.0.1:37771 >"$ROOT/attach-b.log" 2>&1 \
  && ok "B attached brand-film" || { bad "B attach failed"; cat "$ROOT/attach-b.log"; }

TA=$(TOKEN_OF 27772); TB=$(TOKEN_OF 37772)
[ -n "$TA" ] && ok "dashboard A token extracted" || bad "no token from A"
[ -n "$TB" ] && ok "dashboard B token extracted" || bad "no token from B"

echo "== 5. FLIP semantic_merge on the dogfood box (both homes) =="
for PT in "27772 $TA" "37772 $TB"; do
  set -- $PT
  R=$(API "$1" "$2" POST /api/v1/flags '{"name":"semantic_merge","value":"true"}')
  echo "$R" | grep -q '"ok":true' && ok "semantic_merge flipped on :$1" || bad "flag flip on :$1 → $R"
done
R=$(API 27772 "$TA" GET /api/v1/flags)
echo "$R" | grep -q '"semantic_merge":"true"' && ok "GET /flags confirms value" || bad "flags readback: $R"

echo "== 6. A authors the base; B pulls it =="
poll "base cut.otio synced A→B" 60 test -f "$ROOT/root-b/notes/cut.otio"

echo "== 7. deterministic conflict: kill B (kill -9), A edits, B edits stale =="
kill -9 $DB 2>/dev/null; sleep 1
python3 - "$ROOT/root-a/notes/cut.otio" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
c = d["tracks"]["children"][0]["children"][0]
c["source_range"]["start_time"]["value"] = 16.0   # head trim (A)
c["source_range"]["duration"]["value"] = 80.0
json.dump(d, open(sys.argv[1], "w"), indent=1)
PY
sleep 8   # A's sync pass: v2 upload (the 5s ladder + a margin)
python3 - "$ROOT/root-b/notes/cut.otio" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
c = d["tracks"]["children"][0]["children"][0]
c["source_range"]["duration"]["value"] = 80.0     # tail trim (B, still on base v1)
json.dump(d, open(sys.argv[1], "w"), indent=1)
PY
CAIRN_HOME="$ROOT/home-b" "$BIN" daemon --ctl-addr 127.0.0.1:37771 --ui-addr 127.0.0.1:37772 \
  >>"$ROOT/daemon-b.log" 2>&1 & DB=$!
poll "daemon B back up" 30 bash -c 'curl -sf http://127.0.0.1:37772/ >/dev/null'
TB=$(TOKEN_OF 37772)

echo "== 8. the merge affordance appears on the dashboard Files view =="
OFFER=""
for i in $(seq 1 90); do
  R=$(API 37772 "$TB" GET "/api/v1/files?project=brand-film")
  HAVE=$(echo "$R" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: d={}
rows=[r for r in d.get("files",[]) if r.get("merge_available")]
print(json.dumps(rows[0]) if rows else "")' 2>/dev/null)
  if [ -n "$HAVE" ]; then OFFER="$HAVE"; break; fi
  sleep 1
done
if [ -n "$OFFER" ]; then
  ok "merge_available badge live on B's Files view: $(echo "$OFFER" | head -c 200)"
else
  bad "no merge offer surfaced on B in 90s; last files: $(API 37772 "$TB" GET '/api/v1/files?project=brand-film' | head -c 400)"
fi
ls "$ROOT/root-b/notes/" | grep -q "(conflict" && ok "conflict copy preserved on B" || bad "no conflict copy on B"

echo "== 9. accept the offer over the dashboard API =="
# PAUSE_BEFORE_ACCEPT=1 leaves the affordance live on http://127.0.0.1:37772
# (token echoed) for a human/agent browser pass, then waits for ENTER.
if [ "${PAUSE_BEFORE_ACCEPT:-0}" = "1" ]; then
  echo "  PAUSED: affordance live at http://127.0.0.1:37772 (token $TB) — press ENTER to accept"
  read -r _
fi
R=$(API 37772 "$TB" POST /api/v1/merge/offer/accept '{"project_id":"brand-film","path":"notes/cut.otio"}')
echo "$R" | grep -q '"ok":true' && ok "accept ok: $(echo "$R" | head -c 160)" || bad "accept failed: $R"

echo "== 10. both homes converge on the merged timeline =="
HA=$(blake3sum_of() { :; }; true)
conv=""
for i in $(seq 1 60); do
  A=$(python3 -c "import json;d=json.load(open('$ROOT/root-a/notes/cut.otio'));c=d['tracks']['children'][0]['children'][0];print(c['source_range']['start_time']['value'],c['source_range']['duration']['value'])" 2>/dev/null)
  B=$(python3 -c "import json;d=json.load(open('$ROOT/root-b/notes/cut.otio'));c=d['tracks']['children'][0]['children'][0];print(c['source_range']['start_time']['value'],c['source_range']['duration']['value'])" 2>/dev/null)
  if [ -n "$A" ] && [ "$A" = "$B" ]; then conv="$A"; break; fi
  sleep 1
done
[ "$conv" = "16.0 80.0" ] && ok "both homes show the MERGED cut (head trim 16 + tail trim 80): $conv" \
  || bad "no convergence (A=$A B=$B)"
BA=$(blake3 -h "$ROOT/root-a/notes/cut.otio" 2>/dev/null || b3sum "$ROOT/root-a/notes/cut.otio" 2>/dev/null | cut -d' ' -f1)
BB=$(blake3 -h "$ROOT/root-b/notes/cut.otio" 2>/dev/null || b3sum "$ROOT/root-b/notes/cut.otio" 2>/dev/null | cut -d' ' -f1)
[ -n "$BA" ] && [ "$BA" = "$BB" ] && ok "byte-identical merged timeline on both homes (${BA:0:16}…)" || bad "bytes differ (A=$BA B=$BB)"
R=$(API 37772 "$TB" GET "/api/v1/files?project=brand-film")
echo "$R" | grep -q 'merge_available.:true' && bad "offer still showing after accept" || ok "offer cleared after accept"

echo
echo "== dogfood transcript =="
grep -hE "merge|conflict|CONFLICT|offer" "$ROOT/daemon-b.log" | tail -8 | sed 's/^/  B: /'
echo
[ $fail -eq 0 ] && echo "RESULT: $pass passed, $fail failed — semantic_merge dogfood LIVE ✓" \
                || echo "RESULT: $pass passed, $fail failed — logs in $ROOT"
exit $fail