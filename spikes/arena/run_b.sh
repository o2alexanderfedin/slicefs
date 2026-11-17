#!/bin/sh
# Spike B benchmark suites (plan section 5). Run from spikes/arena with
# nothing else building. Data sets are built once (prep) under $ARENA_BENCH_DATA
# (outside the repo, same APFS volume). Raw JSONL in results/b/.
# Usage: ./run_b.sh [prep|noise|bb2|bb3|bb1|bb4|bb5|bb6|bb7|all]
set -e
B=./target/release/bench
R=results/b
: "${ARENA_BENCH_DATA:?set ARENA_BENCH_DATA to a directory for the data sets}"
export ARENA_BENCH_DATA
mkdir -p $R
run() {
  out=$1; shift
  rm -f $R/$out.jsonl
  echo "== $out: $*"
  $B suite "$@" --out $R/$out.jsonl
  echo "lines: $(wc -l < $R/$out.jsonl)"
}
what=${1:-all}
if [ $what = prep ]; then
  for set in rand z2; do
    run prep_$set bprep --variants V1,RS --reps 1 --set $set --mib 1024
  done
  run prep7 bprep7 --variants V1,RS --reps 1 --arenas 2000
fi
if [ $what = noise ] || [ $what = all ]; then
  run noise bget --variants V1,V1b --reps 3 --set rand --order random --state warm_mapped
fi
if [ $what = bb2 ] || [ $what = all ]; then
  for set in rand z2; do
    for order in random seq; do
      run bb2_${set}_${order} bget --variants RS,V1,V1M --reps 5 --set $set --order $order --state warm_mapped
    done
  done
fi
if [ $what = bb3 ] || [ $what = all ]; then
  for state in warm_fresh cold; do
    run bb3_$state bget --variants RS,V1,V1M --reps 3 --set rand --order random --state $state
  done
fi
if [ $what = bb1 ] || [ $what = all ]; then
  for order in random offset; do
    run bb1_$order bb1 --variants pread,mmap --reps 5 --order $order
  done
fi
if [ $what = bb4 ] || [ $what = all ]; then
  for t in 1 4 16; do
    run bb4_t$t bb4 --variants V1,V1M --reps 3 --threads $t
    run bb4_t${t}_gate bb4 --variants V1,V1M --reps 3 --threads $t --gate 1
    run bb4_t${t}_lookup bb4 --variants V1,V1M --reps 3 --threads $t --lookup 1
  done
fi
if [ $what = bb5 ] || [ $what = all ]; then
  for order in random seq; do
    run bb5_$order bget --variants V1M,V1R --reps 3 --set rand --order $order --state cold
  done
  if sudo -n true 2>/dev/null; then echo "fs_usage: root available (not used)"; else echo "fs_usage: needs root, not run"; fi
fi
if [ $what = bb6 ] || [ $what = all ]; then
  run bb6 bb6 --variants V1,V1M --reps 1 --set rand
fi
if [ $what = bb7 ] || [ $what = all ]; then
  run bb7 bb7 --variants RS,V1,V1M --reps 1 --nofile 256
fi
