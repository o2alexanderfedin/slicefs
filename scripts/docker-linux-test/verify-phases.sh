#!/bin/bash
# Live-mount checks for the v1.0 human-verification items (phases 03, 04, 05, 06).
# Runs inside the Docker container. Usage: verify-phases.sh <slicefs-binary>
B=$1; R=/tmp/vp; rm -rf $R; mkdir -p $R; F=0
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint> [extra args...]
  local s=$1 m=$2; shift 2; mkdir -p $m
  $B mount --store $s "$@" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do mountpoint -q $m && return 0; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1; return 1; }
umnt(){ # umnt <mountpoint>: unmount and require the daemon to exit
  timeout -s KILL 15 $B unmount $1 >/dev/null 2>&1 || fusermount -u $1 2>/dev/null
  for i in $(seq 20); do kill -0 $MP 2>/dev/null || return 0; sleep 0.5; done
  bad "daemon-exit $1" "(wchan=$(cat /proc/$MP/wchan 2>/dev/null); after 10s; $(tail -2 $R/mount.log | tr '\n' ' '))"
  for i in $(seq 120); do kill -0 $MP 2>/dev/null || { echo "    daemon exited $((10 + i/2))s after unmount"; return; }; sleep 0.5; done
  kill -9 $MP; }
cas(){ $B stats $1 2>/dev/null | awk -F': ' '/CAS bytes/{print $2}' | grep -oE '[0-9]+ bytes' | head -1 | cut -d' ' -f1; }

echo "== Phase 03: read-only mount (seeded tree, snapshot mounted read-only)"
mkdir -p $R/src/subdir; echo "Hello SliceFS" > $R/src/hello.txt; echo "Nested file" > $R/src/subdir/nested.txt
head -c 65536 /dev/urandom > $R/src/random.bin; chmod 640 $R/src/hello.txt
$B seed $R/s3 $R/src >/dev/null 2>&1 && $B snapshot create --name base $R/s3 >/dev/null 2>&1
if mnt $R/s3 $R/m3 --snapshot base; then
  step ls 10 bash -c "ls -la $R/m3 | grep -q hello.txt && ls $R/m3/subdir | grep -q nested.txt"
  step cat 10 bash -c "[ \"\$(cat $R/m3/hello.txt)\" = 'Hello SliceFS' ] && [ \"\$(cat $R/m3/subdir/nested.txt)\" = 'Nested file' ]"
  step stat 10 bash -c "[ \"\$(stat -c '%F %s %a' $R/m3/hello.txt)\" = 'regular file 14 640' ]"
  step diff 10 diff -r $R/src $R/m3
  if timeout 10 touch $R/m3/test 2>$R/err; then bad touch-erofs "(write succeeded)";
  elif grep -q "Read-only file system" $R/err; then ok touch-erofs; else bad touch-erofs "($(cat $R/err))"; fi
  umnt $R/m3; ok unmount
fi

echo "== Phase 04: end-to-end write session, survives remount"
$B seed $R/s4 $R/src >/dev/null 2>&1
if mnt $R/s4 $R/m4; then
  step echo-cat 10 bash -c "echo hello > $R/m4/test.txt && [ \"\$(cat $R/m4/test.txt)\" = hello ]"
  step mv 10 bash -c "mv $R/m4/test.txt $R/m4/test2.txt && [ \"\$(cat $R/m4/test2.txt)\" = hello ] && [ ! -e $R/m4/test.txt ]"
  step symlink 10 bash -c "ln -s test2.txt $R/m4/link && [ \"\$(readlink $R/m4/link)\" = test2.txt ] && [ \"\$(cat $R/m4/link)\" = hello ]"
  step hardlink 10 bash -c "ln $R/m4/test2.txt $R/m4/hard && [ \$(stat -c %h $R/m4/test2.txt) = 2 ]"
  step mkdir-rmdir 10 bash -c "mkdir $R/m4/dir && rmdir $R/m4/dir && [ ! -e $R/m4/dir ]"
  umnt $R/m4; ok unmount
  if mnt $R/s4 $R/m4; then
    step after-remount 10 bash -c "[ \"\$(cat $R/m4/link)\" = hello ] && [ \$(stat -c %h $R/m4/hard) = 2 ] && diff $R/src/random.bin $R/m4/random.bin"
    umnt $R/m4
  fi
fi

echo "== Phase 05: kill -9 during a large write, then remount"
$B seed $R/s5 $R/src >/dev/null 2>&1
head -c 4000000 /dev/urandom > $R/committed.bin
if mnt $R/s5 $R/m5; then   # session 1: write + clean unmount = committed state
  cp $R/committed.bin $R/m5/committed.bin; umnt $R/m5
fi
if mnt $R/s5 $R/m5 --commit-interval 1; then   # session 2: kill -9 the daemon in the middle of a 64 MB write
  head -c 1000000 /dev/urandom > $R/synced.bin   # written, fsynced and closed before the crash
  dd if=$R/synced.bin of=$R/m5/synced.bin bs=64k conv=fsync status=none
  ( head -c 64000000 /dev/urandom > $R/m5/partial.bin ) 2>/dev/null & W=$!
  sleep 3; kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5 2>/dev/null || umount -l $R/m5 2>/dev/null; kill -9 $W 2>/dev/null; wait $W 2>/dev/null
  ok "kill -9 mid-write"
  if mnt $R/s5 $R/m5; then ok "remount after crash"
    step committed-intact 30 cmp $R/committed.bin $R/m5/committed.bin
    step seeded-intact 10 diff $R/src/random.bin $R/m5/random.bin
    # synced.bin was written with fsync before the crash: the write log (#20)
    # or the commit timer keeps it.
    step closed-file-survives-crash 30 cmp $R/synced.bin $R/m5/synced.bin
    step partial-consistent 60 bash -c "[ ! -e $R/m5/partial.bin ] || [ \$(stat -c %s $R/m5/partial.bin) -eq \$(cat $R/m5/partial.bin | wc -c) ]"
    echo "    partial.bin after crash: $( [ -e $R/m5/partial.bin ] && stat -c '%s bytes' $R/m5/partial.bin || echo absent)"
    step write-after-crash 10 bash -c "echo again > $R/m5/new.txt && [ \"\$(cat $R/m5/new.txt)\" = again ]"
    umnt $R/m5
    step scrub 120 $B scrub $R/s5
  fi
fi

echo "== Phase 05b: fsync is durable without a commit (write log, #20)"
$B seed $R/s5b $R/src >/dev/null 2>&1
if mnt $R/s5b $R/m5b --commit-interval 0; then   # no commit: only the write log can keep these
  head -c 3000000 /dev/urandom > $R/a.bin; head -c 500000 /dev/urandom > $R/c.bin
  dd if=$R/a.bin of=$R/m5b/a.bin bs=64k conv=fsync status=none
  mkdir $R/m5b/dir && mv $R/m5b/a.bin $R/m5b/dir/moved.bin
  dd if=$R/c.bin of=$R/m5b/c.bin bs=64k conv=fsync status=none   # its fsync syncs the rename too
  kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5b 2>/dev/null || umount -l $R/m5b 2>/dev/null
  if mnt $R/s5b $R/m5b; then ok "remount after crash (replay)"
    grep -h "Replayed" $R/mount.log | tail -1 | sed 's/^/    /'
    step fsynced-file-survives 30 cmp $R/c.bin $R/m5b/c.bin
    step renamed-file-survives 30 cmp $R/a.bin $R/m5b/dir/moved.bin
    step old-name-gone 5 bash -c "[ ! -e $R/m5b/a.bin ]"
    umnt $R/m5b
    step scrub-after-replay 120 $B scrub $R/s5b
  fi
fi

echo "== FUSET-05: SIGTERM and SIGHUP end the mount cleanly (final commit, no mount.lock)"
for SIG in TERM HUP; do   # (a background job ignores SIGINT; ignored signals stay ignored)
  $B seed $R/ss$SIG $R/src >/dev/null 2>&1
  if mnt $R/ss$SIG $R/ms$SIG --commit-interval 0; then   # no timer: only the final commit keeps the file
    head -c 300000 /dev/urandom > $R/sig.bin; cp $R/sig.bin $R/ms$SIG/sig.bin
    kill -$SIG $MP
    for i in $(seq 30); do kill -0 $MP 2>/dev/null || break; sleep 0.5; done
    if kill -0 $MP 2>/dev/null; then bad "SIG$SIG-exits" "(still running after 15s)"; kill -9 $MP
    else ok "SIG$SIG-exits"; fi
    fusermount -uz $R/ms$SIG 2>/dev/null
    step "SIG$SIG-no-mount-lock" 5 bash -c "[ ! -e $R/ss$SIG/mount.lock ]"
    if mnt $R/ss$SIG $R/ms$SIG; then
      step "SIG$SIG-write-kept" 30 cmp $R/sig.bin $R/ms$SIG/sig.bin
      umnt $R/ms$SIG
    fi
  fi
done

echo "== Phase 06: two snapshots of the same state do not double-count storage"
$B seed $R/s6 $R/src >/dev/null 2>&1
mnt $R/s6 $R/m6 && { head -c 2000000 /dev/urandom > $R/m6/data.bin; umnt $R/m6; }
C0=$(cas $R/s6); $B snapshot create --name a $R/s6 >/dev/null 2>&1; C1=$(cas $R/s6)
$B snapshot create --name b $R/s6 >/dev/null 2>&1; C2=$(cas $R/s6)
echo "    CAS bytes: before=$C0 snapA=$C1 snapB=$C2"
if [ -n "$C0" ] && [ "$C0" = "$C1" ] && [ "$C1" = "$C2" ]; then ok no-double-count; else bad no-double-count; fi
step snapshot-list 10 bash -c "$B snapshot list $R/s6 | grep -q ' a' && $B snapshot list $R/s6 | grep -q ' b'"

echo "RESULT verify-phases: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
