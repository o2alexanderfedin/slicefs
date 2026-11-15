#!/bin/bash
# Run pjdfstest (POSIX compliance suite) against a SliceFS mount, as root.
# Usage: pjdfstest.sh <slicefs-binary>   (inside the Docker container)
B=$1; R=/tmp/pjd; rm -rf $R; mkdir -p $R/src $R/m
apt-get install -y -q autoconf automake >/dev/null 2>&1 || { apt-get update -q >/dev/null; apt-get install -y -q autoconf automake >/dev/null; }
git clone -q --depth 1 https://github.com/pjd/pjdfstest.git /opt/pjdfstest && cd /opt/pjdfstest \
  && autoreconf -ifs >/dev/null 2>&1 && ./configure -q >/dev/null && make -s pjdfstest >/dev/null 2>&1 || { echo "pjdfstest build failed"; exit 1; }
echo s > $R/src/s.txt; $B seed $R/store $R/src >/dev/null
$B mount --store $R/store --allow-other $R/m </dev/null >$R/mount.log 2>&1 & MP=$!
for i in $(seq 20); do mountpoint -q $R/m && break; sleep .5; done
mountpoint -q $R/m || { echo NOT MOUNTED; tail $R/mount.log; exit 1; }
# open/25.t writes 1 byte at offset 2 GiB+1; SliceFS still builds such a file
# in RAM (issues #1-#3), which takes many minutes, so it runs last with a limit.
cd $R/m && timeout -s KILL 3000 prove ${PROVE_ARGS:--v $(find /opt/pjdfstest/tests -name '*.t' ! -path '*/open/25.t' | sort)} > $R/prove.txt 2>&1
[ -z "$PROVE_ARGS" ] && { timeout -s KILL ${SLOW_LIMIT:-600} prove -v /opt/pjdfstest/tests/open/25.t > $R/prove-slow.txt 2>&1; echo "open/25.t (2 GiB sparse write) rc=$? (137 = killed at ${SLOW_LIMIT:-600}s): $(grep -E '^(Result|Failed)' $R/prove-slow.txt | tr '\n' ' ')"; }
cd /; echo "daemon alive after run: $(kill -0 $MP 2>/dev/null && echo yes || echo no)"; mountpoint $R/m
$B unmount $R/m >/dev/null 2>&1
[ -d /out ] && cp $R/prove*.txt /out/ && grep -v "\[FUSE\].*\(ENTRY\|EXIT.*-> ok\)" $R/mount.log | tail -300 > /out/mount-tail.log && tail -60 $R/mount.log > /out/mount-last.log
grep -E "^Failed [0-9]+/[0-9]+ subtests|^Files=|^Result" $R/prove.txt
echo "--- failing test files:"; grep -E "^/opt.*\(Wstat|Failed:? " $R/prove.txt | head -60
awk '/Test Summary Report/,0' $R/prove.txt | grep -E "^/opt" | sed 's#/opt/pjdfstest/tests/##' | head -80
