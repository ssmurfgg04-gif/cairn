#!/usr/bin/env bash
# Dogfood run (ADR-0020 §7): the full client-review loop, live, the way a
# studio runs it — publish -> guest link -> comment -> resolve ->
# export-markers for the NLE. Every assertion is a bug class the first
# dogfood pass caught (marker TC drift, guest 403 on missing proxy,
# odd-dimension proxy encode failure, hand-counted frames, 1080p "proxy"
# that didn't shrink).
#
# Usage: scripts/dogfood_review.sh   (requires ffmpeg/ffprobe on PATH)
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/debug/cairn
PORT=17799
ROOT=$(mktemp -d /tmp/cairn-dogfood.XXXXXX)
mkdir -p "$ROOT/cuts"
rm -f /tmp/dogfood-session.json /tmp/dogfood-comment.json /tmp/dogfood-range.bin

pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  ok: $*"; }
bad()  { fail=$((fail+1)); echo "FAIL: $*"; }
check() { # check <desc> <needle> <haystack-file>
  if rg -q -- "$2" "$3"; then ok "$1"; else bad "$1 (wanted '$2' in $3)"; fi
}

echo "== build =="
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p cairn-cli -q

echo "== 1. real media: 6 s 23.976 (1001-derived, the drift-prone case) + audio =="
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=size=1920x1080:rate=24000/1001" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t 6 -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
  "$ROOT/cuts/v1.mp4"
ok "media generated (6 s 23.976, audio)"

echo "== 2. publish (auto-probe fps/frames, auto-proxy) =="
OUT=$("$BIN" review publish --root "$ROOT" --title "Brand Film" \
  --media cuts/v1.mp4 --by editor-a 2>&1)
echo "$OUT" | sed 's/^/    /'
echo "$OUT" | rg -q "probed cuts/v1.mp4: 24000/1001 fps" \
  && ok "fps probed from media (no hand-counting)" \
  || bad "fps probe line missing"
echo "$OUT" | rg -q "proxy: .cairn/proxy-cache/" \
  && ok "proxy auto-generated" || bad "proxy generation missing"

echo "== 3. guest links (full stack + latest-only) =="
TOKEN=$("$BIN" review link --root "$ROOT" --role commenter --note "ACME client" --ttl-hours 48 | rg -o 'token: (\w+)' -r '$1')
[ -n "$TOKEN" ] && ok "guest link minted: ${TOKEN:0:8}…" || bad "no token"

echo "== 4. serve the portal (one-root harness, same router as the daemon) =="
cargo run -q -p cairn-review --example serve -- "$ROOT" 127.0.0.1:$PORT >/tmp/dogfood-portal.log 2>&1 &
PORTAL_PID=$!
trap 'kill $PORTAL_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do
  curl -sf --max-time 2 "http://127.0.0.1:$PORT/r/$TOKEN/api/session" >/dev/null 2>&1 && break
  sleep 0.2
done

echo "== 5. guest session =="
curl -sf --max-time 10 "http://127.0.0.1:$PORT/r/$TOKEN/api/session" -o /tmp/dogfood-session.json
check "session resolves" '"ok":true' /tmp/dogfood-session.json
check "version carries true rate 24000/1001" '"fps_num":24000' /tmp/dogfood-session.json
check "proxy is ready (file on disk)" '"proxy_ready":true' /tmp/dogfood-session.json
CODE=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/r/deadbeefdeadbeefdeadbeefdeadbeef/api/session")
[ "$CODE" = "404" ] && ok "unknown link fails closed (404)" || bad "unknown link returned $CODE"

echo "== 6. guest comments at a known frame =="
# frame 96 at 23.976 -> real 4.002 s; NDF display 00:00:04:00 (24 basis)
curl -sf --max-time 10 -X POST "http://127.0.0.1:$PORT/r/$TOKEN/api/comment" \
  -H 'content-type: application/json' \
  -d '{"version":1,"frame":96,"body":"tighten the cut here","author":"jane"}' \
  -o /tmp/dogfood-comment.json
check "comment ack carries frame-exact TC" '"tc":"00:00:04:00"' /tmp/dogfood-comment.json
CODE=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/r/$TOKEN/api/comment" \
  -H 'content-type: application/json' \
  -d '{"version":1,"frame":99999,"body":"x","author":"j"}')
[ "$CODE" = "400" ] && ok "frame beyond cut rejected" || bad "out-of-range frame returned $CODE"

echo "== 7. media serving: range + proxy shape =="
curl -sf --max-time 10 -H 'Range: bytes=0-1023' "http://127.0.0.1:$PORT/r/$TOKEN/media/1" -o /tmp/dogfood-range.bin -D /tmp/dogfood-range.hdr
rg -q '206' /tmp/dogfood-range.hdr && ok "range request -> 206" || bad "no 206"
[ "$(stat -c%s /tmp/dogfood-range.bin)" = "1024" ] && ok "exactly 1024 bytes" || bad "wrong range size"
PROXY=$(python3 -c "import json;print(json.load(open('/tmp/dogfood-session.json'))['versions'][0]['proxy'])")
DIMS=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$ROOT/$PROXY")
[ "$DIMS" = "1280,720" ] && ok "proxy dims $DIMS (720p review profile, even, aspect true)" || bad "proxy dims $DIMS"
PCT=$(printf '%s\n' "$OUT" | rg -o '([0-9.]+)% of source' -r '$1' | head -1 || true)
[ -n "$PCT" ] && ok "proxy is ${PCT}% of source (streamable)" || bad "no proxy size report"

echo "== 8. resolve the note through the portal =="
ID=$(python3 -c "import json;print(json.load(open('/tmp/dogfood-comment.json'))['id'])")
curl -sf --max-time 10 -X POST "http://127.0.0.1:$PORT/r/$TOKEN/api/resolve" \
  -H 'content-type: application/json' \
  -d "{\"version\":1,\"id\":\"$ID\",\"status\":\"RESOLVED\"}" >/dev/null
curl -sf --max-time 10 "http://127.0.0.1:$PORT/r/$TOKEN/api/session" -o /tmp/dogfood-session2.json
check "note resolved (status visible)" '"status":"RESOLVED"' /tmp/dogfood-session2.json

echo "== 9. publish v2 (25 fps PAL) + latest-only link isolation =="
ffmpeg -y -hide_banner -loglevel error -f lavfi -i testsrc2=size=1280x720:rate=25 \
  -f lavfi -i "sine=frequency=300:sample_rate=48000" -t 4 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest "$ROOT/cuts/v2.mp4"
"$BIN" review publish --root "$ROOT" --media cuts/v2.mp4 --label "PAL pass" --by editor-a >/dev/null
LATEST=$("$BIN" review link --root "$ROOT" --role commenter --note "latest-only" --latest-only | rg -o 'token: (\w+)' -r '$1')
curl -sf --max-time 5 "http://127.0.0.1:$PORT/r/$LATEST/api/session" -o /tmp/dogfood-latest.json
NVERS=$(python3 -c "import json;print(len(json.load(open('/tmp/dogfood-latest.json'))['versions']))")
[ "$NVERS" = "1" ] && ok "latest-only link sees exactly one version" || bad "latest-only sees $NVERS versions"
CODE=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/r/$LATEST/media/1")
[ "$CODE" = "404" ] && ok "latest-only link cannot stream hidden v1 (404)" || bad "hidden v1 media returned $CODE"
CODE=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/r/$LATEST/api/comment" \
  -H 'content-type: application/json' -d '{"version":1,"frame":10,"body":"x","author":"j"}')
[ "$CODE" = "404" ] && ok "latest-only link cannot comment on hidden v1 (404)" || bad "hidden v1 comment returned $CODE"

echo "== 10. export-markers: the NLE round-trip (23.976) =="
"$BIN" review export-markers --root "$ROOT" --version 1 --out "$ROOT/markers.xml" >/dev/null
check "FCP7 timebase is the probed 24 basis" '<timebase>24</timebase>' "$ROOT/markers.xml"
check "FCP7 ntsc=TRUE for 23.976" '<ntsc>TRUE</ntsc>' "$ROOT/markers.xml"
check "marker start is the clicked frame 96" '<start>96</start>' "$ROOT/markers.xml"
"$BIN" review export-markers --root "$ROOT" --version 1 --otio --out "$ROOT/markers.otio" >/dev/null
check "OTIO marker rate is 23.976 (true rational on the wire)" '"rate": 23.976' "$ROOT/markers.otio"
check "OTIO marker value is frame 96" '"value": 96' "$ROOT/markers.otio"

echo "== 11. 25 fps regression (the 1.7 s/min drift bug) =="
curl -sf --max-time 10 -X POST "http://127.0.0.1:$PORT/r/$TOKEN/api/comment" \
  -H 'content-type: application/json' \
  -d '{"version":2,"frame":75,"body":"pal note","author":"bob"}' >/dev/null
"$BIN" review export-markers --root "$ROOT" --version 2 --out "$ROOT/markers25.xml" >/dev/null
check "25 fps export uses timebase 25 (was hardcoded 24)" '<timebase>25</timebase>' "$ROOT/markers25.xml"
check "25 fps export is ntsc=FALSE (PAL)" '<ntsc>FALSE</ntsc>' "$ROOT/markers25.xml"
check "25 fps marker at clicked frame 75" '<start>75</start>' "$ROOT/markers25.xml"

echo
echo "dogfood: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
