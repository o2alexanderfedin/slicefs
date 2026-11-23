#!/bin/bash
# Run SliceFS tests against Linux FUSE inside Docker (works on macOS via OrbStack/Docker Desktop).
# A hung mount dies with the container, so no stuck processes are left on the host.
# Commands run in a private mount namespace (unshare): OrbStack briefly holds
# mounts of brand-new containers, which otherwise keeps the FUSE connection
# alive after unmount and makes the daemon hang on exit. unshare alone is not
# enough for the first ~40 s of a container's life, so smoke/verify wait until the
# container is 45 s old before the first mount (the cargo build counts).
# Usage: scripts/docker-linux-test/run.sh [smoke|test|all|verify|pjdfstest]   (default: all)
#        scripts/docker-linux-test/run.sh bigwrite [size-MiB ...]   (#98 memory check; by hand, not in CI)
#        scripts/docker-linux-test/run.sh s1   (spike S1 control: the probe shows request overlap on Linux; by hand)
#        scripts/docker-linux-test/run.sh s4   (spike S4: FUSE queue depth during random writes and readahead; by hand, not in CI)
#        scripts/docker-linux-test/run.sh s6 [files]   (spike S6: small fsynced files per WAL strategy; by hand, not in CI)
#        scripts/docker-linux-test/run.sh harness <test-name>   (one ignored test of the stress harness in release, no mount: s3_store_lock, s5_gate_fairness, s7_budget_baseline (S7_SERIALIZE=0 for the parallel form, S7_FLUSHER=1 for the flusher form); HARNESS_FEATURES=lock-stats for lock rows; by hand, not in CI)
#        scripts/docker-linux-test/run.sh index-bench [SIZES...]   (default sizes: 1000000 10000000)
#        scripts/docker-linux-test/run.sh index-bench-cold [SIZES...]   (cold page cache; privileged; by hand)
# SLICEFS_WORKERS (for example 4) passes --workers to the mounts of the scripts.
# SLICEFS_TARGET_VOL names the cargo target volume (default slicefs-target), so
# two runs from different trees at the same time do not share build output.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
docker build -q -t slicefs-fusetest "$HERE" >/dev/null
RUN=(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
  -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry
  -v "$HERE":/dk:ro -e CARGO_TARGET_DIR=/target -e BW_KEEP -e BW_QUIET_REF -e SLICEFS_WORKERS slicefs-fusetest unshare -m --propagation private)
case "${1:-all}" in
  smoke) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  test)  "${RUN[@]}" bash -c 'cargo test -q --workspace && rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib' ;;
  all)   "${RUN[@]}" bash -c 'cargo test -q --workspace && (rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib) && cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  verify) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/verify-phases.sh /target/debug/slicefs' ;;
  pjdfstest) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/pjdfstest.sh /target/release/slicefs' ;;
  bigwrite) shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/bigwrite.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
  s1) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli --example s1_probe && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && WORK=/tmp/s1 /src/scripts/macos-mount-test/s1-overlap.sh /target/release/examples/s1_probe' ;;
  s4)
    # --privileged only if the fusectl mount fails with SYS_ADMIN alone (see s4-waiting.sh).
    shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/s4-waiting.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
  s6) shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/s6-smallfsync.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
  harness)
    # One ignored test of the stress harness (spikes S3, S5, S6, S7), in release.
    # This mode does not mount anything: no FUSE device, no SYS_ADMIN, no unshare.
    # HARNESS_FEATURES (for example lock-stats) is passed to cargo only when set;
    # the STRESS_*, S3_*, S5_*, S6_* and S7_* variables pass through from the host.
    # The recorder is process-global, so run one test per call, nothing else beside it.
    shift
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -e CARGO_TARGET_DIR=/target -e HARNESS_FEATURES \
      -e STRESS_SEED -e STRESS_THREADS -e STRESS_SECS -e STRESS_FILE_MIB -e STRESS_SERIALIZE -e STRESS_MODE \
      -e S3_THREADS -e S3_FILE_MIB -e S3_SERIALIZE -e S5_FILES -e S5_SECS -e S5_HOLD_MS \
      -e S6_FILES -e S7_FILE_MIB -e S7_WRITE_MIB -e S7_COMMIT_SECS -e S7_SERIALIZE -e S7_FLUSHER \
      slicefs-fusetest bash -c '
        rustc -V
        cargo test -q --release -p slicefs-cli ${HARNESS_FEATURES:+--features "$HARNESS_FEATURES"} --test stress_harness -- --ignored --exact --nocapture "$@"' harness ${@+"$@"} ;;
  index-bench)
    # The chunk index benchmark (#66). This mode does not mount anything: it needs
    # no FUSE device, no SYS_ADMIN and no unshare, so it does not use RUN.
    # The stores stay on the volume slicefs-index-bench and are used again.
    shift
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -v slicefs-index-bench:/bench -e CARGO_TARGET_DIR=/target slicefs-fusetest bash -c '
        grep MemAvailable /proc/meminfo && df -h /bench && cargo build -q --release --locked -p index-bench || exit 1
        for n in "$@"; do
          echo "# size $n, load before: $(cat /proc/loadavg)"
          /target/release/index-bench run --dir /bench/$n --objects $n || exit 1
          echo "# size $n, load after: $(cat /proc/loadavg)"
        done' index-bench ${*:-1000000 10000000} ;;
  index-bench-cold)
    # The chunk index with a cold page cache (by hand). --privileged, because
    # only then can root in the container write /proc/sys/vm/drop_caches; this
    # drops the cache of the whole VM, so run nothing else in Docker meanwhile.
    # For each size: create (if needed), settle (idle merge), warm opens and a
    # warm lookup, cold opens and a cold lookup (K = 1M), then cold lookups of
    # a small sample (K = N/1000), one phase per process, 3 seeds.
    # Each step has a time limit of 30 min.
    shift
    docker run --rm --privileged -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -v slicefs-index-bench:/bench -e CARGO_TARGET_DIR=/target slicefs-fusetest bash -c '
        grep MemAvailable /proc/meminfo && df -h /bench && uname -r && cargo build -q --release --locked -p index-bench || exit 1
        B="timeout 1800 /target/release/index-bench"
        for n in "$@"; do
          d=/bench/cold/$n; k=$((n / 1000))
          echo "# size $n, load before: $(cat /proc/loadavg)"
          [ -f $d/index-bench.txt ] || { echo "# create"; $B create --dir $d --objects $n || exit 1; }
          echo "# settle"; $B settle --dir $d || exit 1
          echo "# open warm-up"; $B open --dir $d || exit 1
          for r in 1 2 3; do echo "# open warm $r"; $B open --dir $d || exit 1; done
          echo "# lookup warm"; $B lookup --dir $d || exit 1
          for r in 1 2 3; do echo "# open cold $r"; $B open --dir $d --drop-caches || exit 1; done
          echo "# lookup cold"; $B lookup --dir $d --drop-caches || exit 1
          for s in 1 2 3; do for ph in hit miss put; do
            echo "# lookup cold small $ph $s"
            $B lookup --dir $d --drop-caches --hits $k --misses $k --seed $s --only $ph || exit 1
          done; done
          echo "# size $n, load after: $(cat /proc/loadavg)"
        done' index-bench ${*:-1000000 10000000} ;;
  *) echo "usage: $0 [smoke|test|all|verify|pjdfstest] | bigwrite [size-MiB ...] (by hand; not in CI) | s1 (by hand) | s4 (by hand; not in CI) | s6 [files] (by hand; not in CI) | harness <test-name> (by hand; not in CI; HARNESS_FEATURES=lock-stats for lock rows) | index-bench [SIZES...] | index-bench-cold [SIZES...]"; exit 2 ;;
esac
