#!/bin/sh
# Spike A benchmark suites (plan section 4). Run from spikes/arena with
# nothing else building. Raw JSONL in results/a/; tee the console to
# results/a.txt. Usage: ./run_a.sh [ab1|ab6|ab2|ab5|ab3|ab4|all]
set -e
B=./target/release/bench
R=results/a
mkdir -p $R
run() {
  out=$1; shift
  rm -f $R/$out.jsonl
  echo "== $out: $*"
  $B suite "$@" --out $R/$out.jsonl
  echo "lines: $(wc -l < $R/$out.jsonl)"
}
what=${1:-all}
if [ $what = ab1 ] || [ $what = all ]; then
  run ab1_k1  ab1 --variants V0,V1,V2,V3,RS --reps 5 --k 1  --n 2000
  run ab1_k8  ab1 --variants V0,V1,V2,V3,RS --reps 5 --k 8  --n 8000
  run ab1_k64 ab1 --variants V0,V1,V2,V3,RS --reps 5 --k 64 --n 32000
fi
if [ $what = ab6 ] || [ $what = all ]; then
  run ab6 ab6 --variants V0,V1,V2,V3,RS --reps 5
fi
if [ $what = ab2 ] || [ $what = all ]; then
  run ab2 ab2 --variants eof,sparse,prealloc,overwrite --reps 5 --iters 1000
fi
if [ $what = ab5 ] || [ $what = all ]; then
  run ab5 ab5 --variants V0,V1,V2,V3 --reps 3
fi
if [ $what = ab3 ] || [ $what = all ]; then
  run ab3 ab3 --variants V0,V1,V2,V3 --reps 1
fi
if [ $what = ab4 ] || [ $what = all ]; then
  for fill in 1 50 100; do
    run ab4_f${fill}_full   ab4 --variants V0,V1,V2,V3 --reps 5 --fill $fill --method full
    run ab4_f${fill}_window ab4 --variants V0,V1,V2,V3 --reps 5 --fill $fill --method window
    run ab4_f${fill}_hole   ab4 --variants V1 --reps 5 --fill $fill --method hole
    run ab4_f${fill}_idx    ab4 --variants V0,V1,V2,V3 --reps 5 --fill $fill --method idx
  done
fi
