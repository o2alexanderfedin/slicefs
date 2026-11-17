#!/bin/sh
# Spike E bench suites. Raw JSONL in results/e/. Run from spikes/arena.
# Usage: ./run_e.sh [tag]   (tag separates repeated runs of the suite)
set -e
B=./target/release/bench
R=results/e
T=${1:-r1}
V=alloc,pwbuf,mmap,pwbufV3,mmapV3
mkdir -p $R
for kind in random comp; do
  $B suite e1 --variants $V --reps 5 --kind $kind --sync none --mib 192 --out $R/e1_${kind}_none_$T.jsonl
  $B suite e1 --variants $V --reps 5 --kind $kind --sync n64  --mib 32  --out $R/e1_${kind}_n64_$T.jsonl
  $B suite e1 --variants $V --reps 5 --kind $kind --sync mib1 --mib 32  --out $R/e1_${kind}_mib1_$T.jsonl
done
