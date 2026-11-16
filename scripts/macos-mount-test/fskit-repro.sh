#!/bin/bash
# FUSE-T FSKit repro without SliceFS: the fuser `simple` example file system,
# then `cp -R` of a large tree. Reports the time of the first
# "Connection reset" error, if any.
#
# usage: fskit-repro.sh <simple binary> <source tree> <backend: fskit|nfs>
# env:   WORK (default: a new temp dir)
X=$1; SRC=$2; BK=${3:-fskit}; R=${WORK:-$(mktemp -d)}/repro-$BK
rm -rf $R; mkdir -p $R/data $R/mnt
FUSET_OPTS=backend=$BK $X --data-dir $R/data --mount-point $R/mnt >$R/fs.log 2>&1 & MP=$!
for i in $(seq 1 60); do mount | grep -q "$R/mnt" && break; sleep 0.5; done
mount | grep "$R/mnt" | cut -c1-160 || { echo "MOUNT FAILED"; tail -5 $R/fs.log; exit 1; }
start=$(date +%s); echo "start $(date +%T), source files: $(find $SRC -type f | wc -l | tr -d ' ')"
cp -R $SRC $R/mnt/ 2>$R/cp.err & CP=$!
first=""
while kill -0 $CP 2>/dev/null; do
  sleep 5
  if [ -z "$first" ] && grep -q "Connection reset" $R/cp.err; then
    first=$(( $(date +%s) - start )); echo "first reset after ${first}s"
  fi
done
wait $CP; rc=$?; echo "cp rc=$rc after $(( $(date +%s) - start ))s; errors: $(wc -l < $R/cp.err | tr -d ' ')"
head -2 $R/cp.err | sed "s|$R|<mnt>|g" | cut -c1-200
umount $R/mnt 2>/dev/null; sleep 2; kill $MP 2>/dev/null
echo "RESULT repro $BK: $([ -z "$first" ] && echo 'no reset' || echo "reset at ${first}s")"
[ -z "$first" ]
