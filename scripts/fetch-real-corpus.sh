#!/usr/bin/env bash
# Fetch a REAL studio-grade media corpus (disk-bounded) and produce honest ingest numbers.
# Sources: Blender Foundation open movies (studio-produced, CC-BY) + a Hugging Face sample.
# The raw files are DELETED after measurement — only the JSON report survives.
set -eu
DIR="${1:-/tmp/cairn-real-corpus}"
OUT="${2:-real-corpus-report.json}"
BUDGET_MB="${BUDGET_MB:-450}"
mkdir -p "$DIR"
cd "$DIR"

fetch(){ # url output min_bytes
  local url="$1" out="$2" min="$3"
  [ -f "$out" ] && [ "$(stat -c %s "$out")" -ge "$min" ] && { echo "[corpus] cached $out"; return 0; }
  echo "[corpus] fetching $out"
  curl -sSL --retry 3 -o "$out" "$url" || { echo "[corpus] SKIP $out (fetch failed)"; rm -f "$out"; return 0; }
  local sz; sz=$(stat -c %s "$out" 2>/dev/null || echo 0)
  [ "$sz" -ge "$min" ] || { echo "[corpus] SKIP $out (too small: $sz)"; rm -f "$out"; }
}

# --- Blender Foundation open movies (real studio footage, CC-BY) ---
fetch "https://download.blender.org/durian/trailer/sintel_trailer-720p.mp4"              "sintel_720.mp4"      3000000 || true
fetch "https://download.blender.org/demo/movies/ToS/tears_of_steel_720p.mov"             "tears_720.mov"      20000000 || true
# --- Hugging Face LFS-hosted real dataset (UCF101 subset; the same git-LFS shape studios use) ---
fetch "https://huggingface.co/datasets/sayakpaul/ucf101-subset/resolve/main/UCF101_subset.tar.gz" "hf_ucf101.tar.gz" 100000000 || true
if [ -f "$DIR/hf_ucf101.tar.gz" ]; then
  tar -tzf "$DIR/hf_ucf101.tar.gz" 2>/dev/null | head -40 > /tmp/ucf-list || true
  tar -xzf "$DIR/hf_ucf101.tar.gz" -C "$DIR" --wildcards 'UCF101_subset/UCF-101/*.avi' 2>/dev/null || \
    tar -xzf "$DIR/hf_ucf101.tar.gz" -C "$DIR" 2>/dev/null || true
  find "$DIR" -name "*.avi" -size +5M | head -2
  find "$DIR" -name "*.avi" ! -size +5M -delete 2>/dev/null || true
  rm -f "$DIR/hf_ucf101.tar.gz"
fi

# fit the disk budget: drop largest files while over
while [ "$(du -sm "$DIR" | cut -f1)" -gt "$BUDGET_MB" ]; do
  BIG=$(ls -S "$DIR" | head -1)
  [ -z "$BIG" ] && break
  echo "[corpus] budget: dropping $BIG"
  rm -f "$DIR/$BIG"
done

MUT="$DIR/tears_720.mov"; [ -f "$MUT" ] || MUT="$(find "$DIR" -type f -size +40M | head -1)"
[ -n "${MUT:-}" ] && [ -f "$MUT" ] || { echo "[corpus] nothing fetched"; exit 1; }

cd /home/z/my-project/cairn
cargo build --release -p cairn-x 2>/dev/null
./target/release/cairn-x corpus-real --dir "$DIR" --out "$OUT" --mutation-file "$MUT"
echo "[corpus] raw files removed; report at $OUT"
rm -rf "$DIR"
