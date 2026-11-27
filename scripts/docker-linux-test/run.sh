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
#        scripts/docker-linux-test/run.sh harness <test-name>   (one ignored test of the stress harness in release, no mount: s3_store_lock, s5_gate_fairness, s7_budget_baseline (S7_SERIALIZE=0 for the parallel form, S7_FLUSHER=1 for the flusher form, S7_THREADS=n writers, default 4; S7_STORE_WAIT_MS=n shortens the store wait of 10 s, so a slow sync gives busy stores); HARNESS_FEATURES=lock-stats for lock rows; by hand, not in CI)
#        scripts/docker-linux-test/run.sh pool-bench   (POOL-05 bench: 16 clients at 1-16 workers, release; POOL_BENCH_QUICK=1, POOL_BENCH_N, POOL_BENCH_ROUNDS, POOL_BENCH_EXTRA; by hand, not in CI)
#        scripts/docker-linux-test/run.sh coverage   (TSAFE-05: line coverage of the tests and the FUSE smoke; COVERAGE_FLOOR gates the tests number; see coverage.sh)
#        scripts/docker-linux-test/run.sh index-bench [SIZES...]   (default sizes: 1000000 10000000)
#        scripts/docker-linux-test/run.sh index-bench-cold [SIZES...]   (cold page cache; privileged; by hand)
#        scripts/docker-linux-test/run.sh chunk-profiles-smoke   (chunk profile benchmark, SMOKE=1, no FUSE; by hand, not in CI)
#        scripts/docker-linux-test/run.sh prolly-bench-smoke   (prolly prototype smoke, SMOKE=1, no FUSE; by hand, not in CI)
#        OUT=<result dir> DATA=<boost data dir> scripts/docker-linux-test/run.sh prolly-layout   (Phase 25 layout and constants matrix, no FUSE; LAYOUT_SMOKE=1 needs no DATA; env CONFIGS LAYOUTS SYNTH_NS KS SYNTH_KS COMMITS SEED ROW_TIMEOUT RESUME; stores on the volume slicefs-prolly-work; by hand, not in CI)
# SLICEFS_WORKERS (for example 4) passes --workers to the mounts of the scripts.
# SLICEFS_TARGET_VOL names the cargo target volume (default slicefs-target), so
# two runs from different trees at the same time do not share build output.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
docker build -q -t slicefs-fusetest "$HERE" >/dev/null
RUN=(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
  -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry
  -v "$HERE":/dk:ro -e CARGO_TARGET_DIR=/target -e BW_KEEP -e BW_QUIET_REF -e SLICEFS_WORKERS
  -e POOL_BENCH_QUICK -e POOL_BENCH_N -e POOL_BENCH_ROUNDS -e POOL_BENCH_EXTRA
  -e COVERAGE_FLOOR -e COVERAGE_REPORT_ONLY -e CARGO_BUILD_JOBS slicefs-fusetest unshare -m --propagation private)
case "${1:-all}" in
  smoke) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  test)  "${RUN[@]}" bash -c 'cargo test -q --workspace && rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib' ;;
  all)   "${RUN[@]}" bash -c 'cargo test -q --workspace && (rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib) && cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  verify) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/verify-phases.sh /target/debug/slicefs' ;;
  coverage) "${RUN[@]}" bash /dk/coverage.sh ;;
  pjdfstest) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/pjdfstest.sh /target/release/slicefs' ;;
  bigwrite) shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/bigwrite.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
  s1) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli --example s1_probe && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && WORK=/tmp/s1 /src/scripts/macos-mount-test/s1-overlap.sh /target/release/examples/s1_probe' ;;
  s4)
    # --privileged only if the fusectl mount fails with SYS_ADMIN alone (see s4-waiting.sh).
    shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/s4-waiting.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
  pool-bench) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/pool-bench.sh /target/release/slicefs' ;;
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
      -e S6_FILES -e S7_FILE_MIB -e S7_WRITE_MIB -e S7_COMMIT_SECS -e S7_SERIALIZE -e S7_FLUSHER -e S7_THREADS -e S7_STORE_WAIT_MS \
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
  chunk-profiles-smoke)
    # The smoke mode of the chunk profile benchmark (#67, PROF-03). It mounts
    # nothing: no FUSE device, no SYS_ADMIN and no unshare, so it does not use RUN.
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -e CARGO_TARGET_DIR=/target slicefs-fusetest bash -c '
        rustc -V
        timeout 1200 cargo build -q --release --locked -j 1 -p chunk-store --example chunk_profile_bench || exit 1
        echo "# load before: $(cat /proc/loadavg)"
        SMOKE=1 RUNS=1 PROFILES="4/8/32 16/64/256" WORK=/tmp/cp BIN=/target/release/examples/chunk_profile_bench \
          timeout 600 bash /src/scripts/chunk-profiles/bench.sh || exit 1
        echo "# load after: $(cat /proc/loadavg)"
        echo "# edits.csv rows: $(($(wc -l < /tmp/cp/edits.csv) - 1))"
        cat /tmp/cp/machine.txt' ;;
  prolly-bench-smoke)
    # The prolly tests (the golden root on Linux) and the smoke of the prolly
    # benchmark (#19, PROLLY-02). It mounts nothing: no FUSE device, no SYS_ADMIN
    # and no unshare, so it does not use RUN.
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -e CARGO_TARGET_DIR=/target slicefs-fusetest bash -c '
        set -o pipefail
        rustc -V || exit 1
        timeout 2400 cargo test -q --release --locked -j 2 -p prolly || exit 1
        timeout 600 cargo test --release --locked -j 2 -p prolly --lib -- --exact tests::golden_root 2>&1 | grep -E "^test |^test result" || exit 1
        timeout 2400 cargo build -q --release --locked -j 2 -p prolly-bench || exit 1
        echo "# load before: $(cat /proc/loadavg)" || exit 1
        SMOKE=1 RUNS=1 WORK=/tmp/pp BIN=/target/release/prolly-bench timeout 900 bash /src/scripts/prolly-bench/bench.sh || exit 1
        echo "# load after: $(cat /proc/loadavg)" || exit 1
        echo "# checks ok: $(grep -c ok=1 /tmp/pp/shape.txt)" || exit 1
        cat /tmp/pp/machine.txt || exit 1' ;;
  prolly-layout)
    # The layout and constants matrix of Phase 25 (scripts/prolly-bench/layout.sh).
    # It mounts nothing: no FUSE device, no SYS_ADMIN and no unshare, so it does not
    # use RUN. DATA (the Boost data dir) is mounted read only; OUT receives the CSV
    # and text files and rows/; the stores live on the disk-backed named volume
    # slicefs-prolly-work (no tmpfs) and stay there only with RESUME=1.
    # LAYOUT_SMOKE=1 runs a small generated matrix and needs no DATA.
    [ -n "${OUT:-}" ] || { echo "prolly-layout needs OUT=<result dir>" >&2; exit 2; }
    mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
    DATA_MOUNT=()
    if [ "${LAYOUT_SMOKE:-0}" != 1 ]; then
      [ -n "${DATA:-}" ] && [ -d "$DATA" ] || { echo "prolly-layout needs DATA=<Boost data dir> (or LAYOUT_SMOKE=1)" >&2; exit 2; }
      DATA_MOUNT=(-v "$(cd "$DATA" && pwd)":/data:ro)
    fi
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -v slicefs-prolly-work:/work -v "$OUT":/out ${DATA_MOUNT[@]+"${DATA_MOUNT[@]}"} -e CARGO_TARGET_DIR=/target \
      -e LAYOUT_SMOKE -e CONFIGS -e LAYOUTS -e SYNTH_NS -e KS -e SYNTH_KS -e COMMITS -e SEED -e ROW_TIMEOUT -e RESUME \
      slicefs-fusetest bash -c '
        set -o pipefail
        rustc -V || exit 1
        timeout 2400 cargo build -q --release --locked -j 2 -p prolly-bench || exit 1
        R=/work/run
        # The volume persists between runs: start clean unless the run is resumed.
        [ "${RESUME:-0}" = 1 ] || rm -rf "$R"
        echo "# load before: $(cat /proc/loadavg)"
        ARG=; [ "${LAYOUT_SMOKE:-0}" = 1 ] || ARG=/data
        rc=0
        WORK=$R BIN=/target/release/prolly-bench bash /src/scripts/prolly-bench/layout.sh $ARG || rc=$?
        echo "# load after: $(cat /proc/loadavg)"
        for f in layout.csv shape.csv build.csv checks.txt machine.txt; do [ -f "$R/$f" ] && cp "$R/$f" /out/; done
        [ -d "$R/rows" ] && { rm -rf /out/rows; cp -R "$R/rows" /out/rows; }
        [ "${RESUME:-0}" = 1 ] || rm -rf "$R"
        echo "# layout.sh rc=$rc"
        exit $rc' ;;
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
  *) echo "usage: $0 [smoke|test|all|verify|pjdfstest] | coverage | bigwrite [size-MiB ...] (by hand; not in CI) | s1 (by hand) | s4 (by hand; not in CI) | s6 [files] (by hand; not in CI) | pool-bench (by hand; not in CI) | harness <test-name> (by hand; not in CI; HARNESS_FEATURES=lock-stats for lock rows) | index-bench [SIZES...] | index-bench-cold [SIZES...] | prolly-layout (OUT=<dir> DATA=<dir> or LAYOUT_SMOKE=1; by hand; not in CI)"; exit 2 ;;
esac
