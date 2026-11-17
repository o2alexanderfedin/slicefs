#!/bin/sh
# Spike D benchmark suites (GC by hole punching). Run from spikes/arena with
# nothing else building. Raw JSONL in results/d/; tee the console to
# results/d.txt. Usage: ./run_d.sh [dprobe|dgc|dchurn|all]
set -e
B=./target/release/bench
R=results/d
mkdir -p $R
run() {
  out=$1; shift
  rm -f $R/$out.jsonl
  echo "== $out: $*"
  $B suite "$@" --out $R/$out.jsonl
  echo "lines: $(wc -l < $R/$out.jsonl)"
}
what=${1:-all}
if [ $what = dprobe ] || [ $what = all ]; then
  run dprobe dprobe --variants grow,sparse,reserve,zeroed --reps 3
fi
if [ $what = dgc ] || [ $what = all ]; then
  for pat in file obj; do
    for dead in 0.1 0.3 0.5 0.7 0.9; do
      run dgc_${pat}_${dead} dgc --variants RS,A,D,D0 --reps 3 --pattern $pat --dead $dead --mib 256
    done
  done
fi
if [ $what = dchurn ] || [ $what = all ]; then
  for pat in file obj; do
    run dchurn_${pat} dchurn --variants RS,A,D --reps 3 --pattern $pat --rounds 6 --churn 0.25 --mib 192
  done
fi
