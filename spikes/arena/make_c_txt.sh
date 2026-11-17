#!/bin/sh
# Build results/c.txt from results/c_notes.txt and the raw JSONL of spike C.
cd "$(dirname "$0")"
S=./target/release/summarize
F=put_mb_s,put_cpu_s_per_gib,sync_ms_mean,sync_ms_p50,sync_ms_p99,commit_full_syncs_mean,commit_full_syncs_max,gc_full_syncs,amp_pre,amp_pre_max,amp_post,wa,wa_padding,wa_copies,wa_idx,wa_dirty,dirty_writes,checkpoints,gc_wall_s,gc_cpu_s,arenas,seq_mb_s,seq_cpu_s_per_gib,fresh_seq_mb_s,fresh_seq_cpu_s_per_gib
{
  cat results/c_notes.txt; echo; cat results/c_decision.txt
  echo
  echo "== C-sim: registry corpus (results/c/sim_corpus.*) =="
  head -1 results/c/sim_corpus.err
  cat results/c/sim_corpus.md
  echo
  echo "== C-sim: synthetic trace (results/c/sim_synth.*) =="
  head -1 results/c/sim_synth.err
  cat results/c/sim_synth.md
  echo
  echo "== C-B1 / C-B4 (64 MiB arenas) =="
  for f in 0.5 0.7 0.85 0.95; do $S table results/c/cb1p_f$f.jsonl --base V1 --fields $F; done
  $S table results/c/cb1p_f0.85_c2.jsonl --base C2 --fields $F
  echo "== C-B1 scale model (16 MiB arenas, 2.5x churn) =="
  $S table results/c/cb1s_f0.85.jsonl --base V1 --fields $F
  echo "== C-B2 =="
  for n in 1 2 4 8; do $S table results/c/cb2_n$n.jsonl --base full --fields sync_ms_p50,sync_ms_mean,sync_ms_p99,cpu_ms_per_sync; done
  echo "== C-B3 (reopen, warm cache; one line per reopen) =="
  $S table results/c/cb3.jsonl --base C4 --fields wall_s,cpu_s,scanned,scan_mib,objects
  echo "== C-B5 (raw) =="
  cat results/c/cb5.jsonl
  echo
  echo "== Crash loop (raw) =="
  cat results/c/crashloop.jsonl
} > results/c.txt
