#!/usr/bin/env bash
# Waveform peaks benchmark (CONTRACT-DEBT #4) — server-side, bounded,
# admission-gated peaks for the review portal.
#
# Builds the release example and runs the >=2 h acceptance bed (2 h 10 m,
# mono 48 kHz 16-bit ≈ 750 MB) plus a 10 m control, emitting a markdown
# table on stdout. The temp WAV + cache are deleted after each run; peak
# RSS comes from /proc VmHWM (the kernel's high-water mark), so it measures
# the process peak including the decode. Linux-only for the RSS column.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release -p cairn-review --example waveform_bench
BIN="target/release/examples/waveform_bench"

echo
echo "| audio bed | iters | wall/iter (median) | peak RSS (VmHWM Δ) | bins (codec) |"
echo "|---|---|---|---|---|"
"$BIN" --minutes 130 --iters 3 --rate-hz 8
"$BIN" --minutes 10 --iters 5 --rate-hz 8
