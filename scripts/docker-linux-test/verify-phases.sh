#!/bin/bash
# Live-mount checks for the v1.0 human-verification items (phases 03, 04, 05, 06).
# Runs inside the Docker container. Usage: verify-phases.sh <slicefs-binary>
B=$1; PRE=(); R=/tmp/vp; rm -rf $R; mkdir -p $R; F=0
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint> [extra args...]
  # SLICEFS_WORKERS=N adds --workers N when the caller gives no --workers; the
  # Workers line of that mount must then be "Workers: N (set)".
  local s=$1 m=$2 n=0 w=(); shift 2; mkdir -p $m
  case " $* " in *" --workers "*|*" --workers="*) ;; *) [ -n "${SLICEFS_WORKERS:-}" ] && w=(--workers "$SLICEFS_WORKERS") ;; esac
  [ -f $R/mount.log ] && n=$(wc -l < $R/mount.log)
  "${PRE[@]}" $B mount --store $s "$@" "${w[@]}" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do mountpoint -q $m && { wline $n ${#w[@]} $m; return 0; }; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1; return 1; }
wline(){ # wline <log lines before the mount> <count of added args> <mountpoint>
  local L; L=$(tail -n +$(($1 + 1)) $R/mount.log | grep -m1 -o 'Workers: [0-9]* ([a-z]*)')
  echo "    workers-line: ${L:-none}"
  if [ "$2" -gt 0 ] && [ "$L" != "Workers: $SLICEFS_WORKERS (set)" ]; then bad "workers-line $3" "(${L:-none})"; fi; }
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

echo "== Phase 05c: a rename is durable after an fsync of the directory only (write log, #20)"
$B seed $R/s5c $R/src >/dev/null 2>&1
# flush-on-fsync: records stay in RAM until a sync, so only the fsync of the
# directory can make the rename durable. (Per-op keeps a rename even with no
# sync; the control below checks that this test can fail.)
if mnt $R/s5c $R/m5c --commit-interval 0 --wal-strategy flush-on-fsync; then   # no commit: only the write log can keep these
  mkdir $R/m5c/dir; head -c 2000000 /dev/urandom > $R/d.bin
  dd if=$R/d.bin of=$R/m5c/dir/d.bin bs=64k conv=fsync status=none   # file and mkdir now durable
  mv $R/m5c/dir/d.bin $R/m5c/dir/renamed.bin
  # The only sync after the rename: fsync of the directory (coreutils 9
  # `sync FILE` fsyncs the path; the kernel sends FUSE FSYNCDIR). No file
  # fsync comes after the rename.
  timeout -s KILL 10 sync $R/m5c/dir || bad dir-fsync
  kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5c 2>/dev/null || umount -l $R/m5c 2>/dev/null
  if mnt $R/s5c $R/m5c --wal-strategy flush-on-fsync; then
    grep -h "Replayed" $R/mount.log | tail -1 | sed 's/^/    /'
    step rename-survives-dir-fsync 30 cmp $R/d.bin $R/m5c/dir/renamed.bin
    step old-name-gone-after-dir-fsync 5 bash -c "[ ! -e $R/m5c/dir/d.bin ]"
    umnt $R/m5c
  fi
fi
# Control (#98): per-op writes each request into the log file at once, so a
# kill -9 of the daemon also keeps a rename with no sync after it. In
# flush-on-fsync mode, records below 4 MiB stay in RAM until a sync, so there
# a rename with no sync after it is lost. The p.txt create is durable through
# the fsync of the directory, so "lost" means only the rename was lost.
$B seed $R/s5c2 $R/src >/dev/null 2>&1
if mnt $R/s5c2 $R/m5c2 --commit-interval 0 --wal-strategy flush-on-fsync; then
  mkdir $R/m5c2/dir; echo p > $R/m5c2/dir/p.txt
  timeout -s KILL 10 sync $R/m5c2/dir || bad dir-fsync-control
  mv $R/m5c2/dir/p.txt $R/m5c2/dir/q.txt   # no sync after this rename
  kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5c2 2>/dev/null || umount -l $R/m5c2 2>/dev/null
  if mnt $R/s5c2 $R/m5c2 --wal-strategy flush-on-fsync; then
    # The control must lose the rename. If it keeps it, the 05c check above
    # passes with or without the fsync of the directory and proves nothing.
    if [ -e $R/m5c2/dir/p.txt ] && [ ! -e $R/m5c2/dir/q.txt ]; then ok control-rename-without-sync-lost
    elif [ -e $R/m5c2/dir/q.txt ]; then bad control-rename-without-sync-lost "(kept: 05c does not prove the fsync of the directory)"
    else bad control-rename-without-sync-lost "(neither p.txt nor q.txt exists)"; fi
    umnt $R/m5c2
  fi
fi

echo "== Phase 05d: a torn last record does not stop the mount (write log, #20)"
$B seed $R/s5d $R/src >/dev/null 2>&1
if mnt $R/s5d $R/m5d --commit-interval 0; then
  head -c 1000000 /dev/urandom > $R/e.bin
  dd if=$R/e.bin of=$R/m5d/e.bin bs=64k conv=fsync status=none
  # The sacrificial last record: after this sync the mkdir is the last record on disk.
  mkdir $R/m5d/last; timeout -s KILL 10 sync $R/m5d
  kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5d 2>/dev/null || umount -l $R/m5d 2>/dev/null
  # Cut the last record as a power loss can.
  SEG=$(ls $R/s5d/segments/segment-*.seg | sort -V | tail -1)
  truncate -s -3 "$SEG"; echo "    cut 3 bytes of $(basename "$SEG")"
  if mnt $R/s5d $R/m5d; then ok mount-with-torn-last-record
    grep -h "Replayed" $R/mount.log | tail -1 | sed 's/^/    /'
    step records-before-the-tear-kept 30 cmp $R/e.bin $R/m5d/e.bin
    step torn-record-not-applied 5 bash -c "[ ! -e $R/m5d/last ]"
    step write-after-torn-tail 10 bash -c "echo again > $R/m5d/new.txt && [ \"\$(cat $R/m5d/new.txt)\" = again ]"
    umnt $R/m5d
    step scrub-after-torn-tail 120 $B scrub $R/s5d
  fi
fi

echo "== Phase 05e: periodic mode keeps a write with no fsync after its 5 s sync (write log, #133)"
$B seed $R/s5e $R/src >/dev/null 2>&1
# No commit: only the timer sync can keep these (below the 4 MiB cap, the
# records stay in RAM until a sync, as in 05c2, where they are lost).
if mnt $R/s5e $R/m5e --commit-interval 0 --wal-strategy periodic; then
  head -c 300000 /dev/urandom > $R/g.bin
  for i in 1 2 3; do cp $R/g.bin $R/m5e/g$i.bin; mv $R/m5e/g$i.bin $R/m5e/h$i.bin; sleep 1; done
  sleep 6   # more than the 5 s timer after the last request; no fsync, no sync
  kill -9 $MP; sleep 0.5
  fusermount -uz $R/m5e 2>/dev/null || umount -l $R/m5e 2>/dev/null
  if mnt $R/s5e $R/m5e --wal-strategy periodic; then
    step periodic-write-survives-kill 30 bash -c "for i in 1 2 3; do cmp $R/g.bin $R/m5e/h\$i.bin && [ ! -e $R/m5e/g\$i.bin ] || exit 1; done"
    umnt $R/m5e
    step scrub-after-periodic-kill 120 $B scrub $R/s5e
  fi
fi

echo "== FUSET-05: SIGTERM, SIGINT (Ctrl+C) and SIGHUP end the mount cleanly (final commit, no mount.lock)"
# A background job ignores SIGINT; env --default-signal gives the mount the
# default action, as a terminal does for Ctrl+C.
PRE=(env --default-signal=INT)
for SIG in TERM INT HUP; do
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
PRE=()

# POOL-03 (Phase 21): a request panic aborts the daemon; the remount replays the write log.
bash "$(dirname "$0")/panic-replay.sh" "$B" || F=1

echo "== Phase 06: two snapshots of the same state do not double-count storage"
$B seed $R/s6 $R/src >/dev/null 2>&1
mnt $R/s6 $R/m6 && { head -c 2000000 /dev/urandom > $R/m6/data.bin; umnt $R/m6; }
C0=$(cas $R/s6); $B snapshot create --name a $R/s6 >/dev/null 2>&1; C1=$(cas $R/s6)
$B snapshot create --name b $R/s6 >/dev/null 2>&1; C2=$(cas $R/s6)
echo "    CAS bytes: before=$C0 snapA=$C1 snapB=$C2"
if [ -n "$C0" ] && [ "$C0" = "$C1" ] && [ "$C1" = "$C2" ]; then ok no-double-count; else bad no-double-count; fi
step snapshot-list 10 bash -c "$B snapshot list $R/s6 | grep -q ' a' && $B snapshot list $R/s6 | grep -q ' b'"

echo "== Large copy (#20): 1 GiB in order and out of order, bounded daemon memory"
T0=$SECONDS
# Measured peak 130 MiB (debug build, as run.sh verify builds it, 2026-11-18).
# The peak is below 2/3 of 512 MiB, so the cap is 512 MiB: about 8x the reorder
# window and far below the 1 GiB file.
CAP_MIB=512
hwm(){ awk '/^VmHWM:/{print int($2/1024)}' /proc/$MP/status 2>/dev/null; }   # daemon peak RSS, MiB
# The old Buffered mode is the overlay since #98.
nbuf(){ grep -c "switches to overlay" $R/mount.log; }
head -c 1073741824 /dev/urandom > $R/big.bin
# Out-of-order writer: one handle for the whole pass, because a new open of a
# file that has content makes the next write go to the overlay. 1 MiB blocks
# are written last to first inside each 16 MiB group, so each jump stays far
# inside the 64 MiB reorder window. An fsync every 64 MiB stands for the NFS
# COMMITs of the original #20 report.
cat > $R/ooo.py <<'PY'
import os, sys
MB = 1 << 20
src = os.open(sys.argv[1], os.O_RDONLY)
dst = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
nblk = (os.fstat(src).st_size + MB - 1) // MB
for g, first in enumerate(range(0, nblk, 16)):
    for b in reversed(range(first, min(first + 16, nblk))):
        data = os.pread(src, MB, b * MB)
        done = 0
        while done < len(data):
            done += os.pwrite(dst, data[done:], b * MB + done)
    if g % 4 == 3:
        os.fsync(dst)
os.close(dst)
PY
$B seed $R/sL $R/src >/dev/null 2>&1
if mnt $R/sL $R/mL; then   # default commit interval (30 s)
  B0=$(nbuf)
  t=$SECONDS; step big-copy-in-order 180 cp $R/big.bin $R/mL/inorder.bin; TC=$((SECONDS - t)); H1=$(hwm)
  t=$SECONDS; step big-write-out-of-order 120 python3 $R/ooo.py $R/big.bin $R/mL/ooo.bin; TW=$((SECONDS - t)); H2=$(hwm)
  t=$SECONDS
  step big-in-order-cmp 120 cmp $R/big.bin $R/mL/inorder.bin
  step big-out-of-order-cmp 120 cmp $R/big.bin $R/mL/ooo.bin
  TM=$((SECONDS - t)); H3=$(hwm)
  if [ -n "$H3" ] && [ "$H3" -le $CAP_MIB ]; then ok daemon-rss-bounded
  else bad daemon-rss-bounded "(VmHWM $H3 MiB > cap $CAP_MIB MiB)"; fi
  B1=$(nbuf)
  if [ "$B1" = "$B0" ]; then ok no-switch-to-overlay; else bad no-switch-to-overlay "($((B1 - B0)) switch(es))"; fi
  echo "    timing: cp $TC s, out-of-order write $TW s, cmp $TM s"
  # VmHWM only grows, so the last value read before the daemon exits covers
  # the final commit at the unmount. The sampler ends when the daemon ends.
  ( while kill -0 $MP 2>/dev/null; do h=$(hwm); [ -n "$h" ] && echo $h > $R/hwm.last; sleep 0.1; done ) & SP=$!
  umnt $R/mL; wait $SP; H4=$(cat $R/hwm.last 2>/dev/null)
  if [ -n "$H4" ] && [ "$H4" -le $CAP_MIB ]; then ok daemon-rss-bounded-at-unmount
  else bad daemon-rss-bounded-at-unmount "(VmHWM $H4 MiB > cap $CAP_MIB MiB)"; fi
  echo "    daemon VmHWM: after cp $H1 MiB, after out-of-order write $H2 MiB, after cmp $H3 MiB, at unmount $H4 MiB (cap $CAP_MIB MiB)"
  if mnt $R/sL $R/mL; then
    step big-in-order-cmp-after-remount 120 cmp $R/big.bin $R/mL/inorder.bin
    step big-out-of-order-cmp-after-remount 120 cmp $R/big.bin $R/mL/ooo.bin
    umnt $R/mL
  fi
fi
rm -rf $R/sL $R/mL $R/big.bin $R/ooo.py
echo "    section time: $((SECONDS - T0)) s"

echo "RESULT verify-phases: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
