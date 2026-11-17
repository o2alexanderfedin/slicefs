#!/bin/sh
# Spike C benchmark suites (plan section 6). Run from spikes/arena with
# nothing else building. Raw JSONL in results/c/; the console goes to
# results/c/run.log. Usage: ./run_c.sh [cb1p|cb1s|cb2|cb3|crash|all]
#
# cb1p: C-B1 at P* size: 64 MiB arenas, 512 MiB of live files, 1.5x churn,
#       fills 50/70/85/95 %. At 85 % also C-B4 (--seqread: sequential read
#       after churn, and of the same files on a fresh store), and one run of
#       C2 (size classes) to check the simulator's dirty-arena count.
# cb1s: C-B1 scale model at 85 %: 16 MiB arenas, 512 MiB live, 2.5x churn
#       (same live/arena ratio as a 2 GiB store on 64 MiB arenas; more
#       churn to age the arenas). Space and write amplification only: its
#       commit is 1/2 of an arena, so its sync numbers are not P* numbers.
#       One rep (the byte counts are deterministic for a seed).
set -e
B=./target/release/bench
R=results/c
mkdir -p $R
run() {
  out=$1; shift
  rm -f $R/$out.jsonl
  echo "== $out: $*"
  $B suite "$@" --out $R/$out.jsonl
  echo "lines: $(wc -l < $R/$out.jsonl)"
}
what=${1:-all}
if [ $what = cb1p ] || [ $what = all ]; then
  run cb1p_f0.85 cb1 --variants RS,V1,C1,C4,C4d4 --reps 3 --fill 0.85 --live_mib 512 --size_mib 64 --churn 1.5 --seqread 1
  run cb1p_f0.85_c2 cb1 --variants C2 --reps 1 --fill 0.85 --live_mib 512 --size_mib 64 --churn 1.5
  for f in 0.5 0.7 0.95; do
    run cb1p_f$f cb1 --variants RS,V1,C1,C4,C4d4 --reps 3 --fill $f --live_mib 512 --size_mib 64 --churn 1.5
  done
fi
if [ $what = cb2 ] || [ $what = all ]; then
  for n in 1 2 4 8; do
    run cb2_n$n cb2 --variants full,fsync1 --reps 5 --n $n --iters 40
  done
fi
if [ $what = cb3 ] || [ $what = all ]; then
  run cb3 cb3 --variants C4,C4s --reps 3 --arenas 9
fi
if [ $what = crash ] || [ $what = all ]; then
  rm -f $R/crashloop.jsonl
  ./target/release/crashloop run --variant V1 --kills 50 --size_mib 1 --wait_ms 2500 | tee -a $R/crashloop.jsonl
  ./target/release/crashloop run --variant V1 --kills 50 --size_mib 1 --wait_ms 2500 --churn 1 --live_cap 300 | tee -a $R/crashloop.jsonl
  for v in C1 C3 C4 C4s C4d4; do
    ./target/release/crashloop run --variant $v --kills 50 --live_cap 300 --size_mib 1 --wait_ms 2500 | tee -a $R/crashloop.jsonl
  done
fi
if [ $what = cb1s ] || [ $what = all ]; then
  run cb1s_f0.85 cb1 --variants RS,V1,C1,C4,C4d4 --reps 1 --fill 0.85 --live_mib 512 --size_mib 16 --churn 2.5
fi
