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
#        scripts/docker-linux-test/run.sh index-bench [SIZES...]   (default sizes: 1000000 10000000)
# SLICEFS_TARGET_VOL names the cargo target volume (default slicefs-target), so
# two runs from different trees at the same time do not share build output.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
docker build -q -t slicefs-fusetest "$HERE" >/dev/null
RUN=(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
  -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry
  -v "$HERE":/dk:ro -e CARGO_TARGET_DIR=/target -e BW_KEEP slicefs-fusetest unshare -m --propagation private)
case "${1:-all}" in
  smoke) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  test)  "${RUN[@]}" bash -c 'cargo test -q --workspace && rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib' ;;
  all)   "${RUN[@]}" bash -c 'cargo test -q --workspace && (rm -rf /tmp/fuser && cp -r vendor/fuser /tmp/fuser && cd /tmp/fuser && cargo test -q --lib) && cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  verify) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/verify-phases.sh /target/debug/slicefs' ;;
  pjdfstest) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/pjdfstest.sh /target/release/slicefs' ;;
  bigwrite) shift; "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/bigwrite.sh /target/release/slicefs "$@"' _ ${@+"$@"} ;;
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
  *) echo "usage: $0 [smoke|test|all|verify|pjdfstest] | bigwrite [size-MiB ...] (by hand; not in CI) | index-bench [SIZES...]"; exit 2 ;;
esac
