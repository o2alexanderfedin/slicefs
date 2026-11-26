#!/bin/bash
# Large copy on FSKit (#20, WLOG-02): copy a 1 GiB random file into a SliceFS
# mount, sample the daemon RSS each second, compare; unmount, remount, compare
# again. Between the two, rename it in place, move it to another directory and
# rename a small file over another one (FUSE-T sends its own `fuse_rename_in`
# layout; see vendor/fuser/PATCHES.md), and set a text and a binary extended
# attribute (on FUSE-T, macOS keeps them in an AppleDouble `._` file; the
# values must follow the rename and the remount). Fails if a cmp, a rename or
# an xattr check fails, or if the peak RSS is above the limit.
#
# usage: large-copy.sh [backend]          (default backend: fskit)
# env:   SLICEFS      slicefs binary (default: target/release/slicefs)
#        WORK         work dir (default: a new temp dir)
#        SIZE         file size in bytes (default: 1073741824)
#        RSS_LIMIT_MIB  peak RSS limit (default: 512)
#        STEP_TIMEOUT time limit in s for the copy and each cmp (default: 1800)
BK=${1:-fskit}
B=${SLICEFS:-target/release/slicefs}
SIZE=${SIZE:-1073741824}
LIMIT=${RSS_LIMIT_MIB:-512}
T=${STEP_TIMEOUT:-1800}
[ -n "$WORK" ] && mkdir -p "$WORK"
W=$(cd "${WORK:-$(mktemp -d)}" && pwd -P)/lc-$BK
rm -rf "$W"; mkdir -p "$W/seed" "$W/mnt"
echo "run dir: $W"
echo s > "$W/seed/s.txt"
$B seed "$W/store" "$W/seed" >/dev/null 2>&1 || { echo "seed failed"; echo "RESULT large-copy: FAIL"; exit 1; }
head -c "$SIZE" /dev/urandom > "$W/big.bin"

MP=; S=
cleanup() {
  [ -n "$S" ] && kill "$S" 2>/dev/null
  if [ -n "$MP" ] && kill -0 "$MP" 2>/dev/null; then
    $B unmount "$W/mnt" >/dev/null 2>&1; sleep 2; kill -9 "$MP" 2>/dev/null
  fi
}
trap cleanup EXIT

# mount(8) shows the path without the /private prefix.
mounted() { mount | grep -q "${W#/private}/mnt "; }
# waitmnt: up to 60 s; stops early if the daemon ends.
waitmnt() {
  for i in $(seq 1 60); do
    mounted && return 0
    kill -0 "$MP" 2>/dev/null || { echo "  daemon ended before mount"; break; }
    sleep 1
  done
  echo "  MOUNT FAILED"; tail -20 "$W/mount.log"; return 1
}
# waitexit: up to 120 s for the daemon to exit after the unmount.
waitexit() {
  for i in $(seq 1 240); do kill -0 "$MP" 2>/dev/null || return 0; sleep 0.5; done
  echo "  DAEMON DID NOT EXIT in 120 s"; kill -9 "$MP" 2>/dev/null; return 1
}
# run <name> <cmd...>: run with a time limit of $T s; returns the exit code.
run() {
  local n=$1 s=$(date +%s); shift
  "$@" >"$W/step.out" 2>&1 & local p=$!
  while kill -0 $p 2>/dev/null; do
    if [ $(( $(date +%s) - s )) -ge "$T" ]; then kill -9 $p 2>/dev/null; wait $p 2>/dev/null; echo "  $n: HANG (>${T}s)"; return 124; fi
    sleep 1
  done
  wait $p; local rc=$?
  echo "  $n: rc=$rc ($(( $(date +%s) - s ))s)"
  [ $rc -ne 0 ] && head -5 "$W/step.out" | sed 's/^/      /'
  return $rc
}
mount_fs() {
  $B mount --store "$W/store" --backend "$BK" "$W/mnt" </dev/null >>"$W/mount.log" 2>&1 &
  MP=$!
  waitmnt || return
  echo "  workers: $(grep '^Workers:' "$W/mount.log" | tail -1)"
}
unmount_fs() {
  # The unmount has its own short limit, so all limits fit in the CI step.
  local t=$T; T=60; run unmount $B unmount "$W/mnt"; local rc=$?; T=$t
  [ $rc -eq 0 ] || { waitexit; return $rc; }
  waitexit
}

# xattr_ok <file>: both attributes of the xattr step have their values.
xattr_ok() {
  [ "$(xattr -p com.example.text "$1" 2>/dev/null)" = "text value" ] \
    && [ "$(xattr -p -x com.example.bin "$1" 2>/dev/null | tr -d " \n")" = 000102FF ] \
    || { echo "  xattr check failed on $1: $(xattr -l "$1" 2>&1 | head -3)"; return 1; }
}

CMP1=FAIL; CMP2=FAIL; REN=FAIL; XA=FAIL; FAILS=0
# The file after the renames below.
F=d/moved.bin
echo 0 > "$W/peak.kib"
if mount_fs; then
  # RSS sampler (KiB, each second); ends when the daemon ends.
  ( while kill -0 $MP 2>/dev/null; do
      r=$(ps -o rss= -p $MP | tr -d ' ')
      [ -n "$r" ] && [ "$r" -gt "$(cat "$W/peak.kib")" ] && echo $r > "$W/peak.kib"
      sleep 1
    done ) &
  S=$!
  mounted && run copy cp "$W/big.bin" "$W/mnt/big.bin" \
    && mounted && run cmp-after-copy cmp "$W/big.bin" "$W/mnt/big.bin" && CMP1=ok
  mounted && run rename mv "$W/mnt/big.bin" "$W/mnt/renamed.bin" \
    && run mkdir mkdir "$W/mnt/d" \
    && run rename-cross-dir mv "$W/mnt/renamed.bin" "$W/mnt/$F" \
    && echo a > "$W/mnt/a" && echo b > "$W/mnt/b" \
    && run rename-replace mv -f "$W/mnt/a" "$W/mnt/b" \
    && [ "$(cat "$W/mnt/b")" = a ] && [ ! -e "$W/mnt/a" ] && REN=ok
  # xattrs: set on b, rename b to c, read them back from c (again after the remount)
  mounted && run xattr-write xattr -w com.example.text "text value" "$W/mnt/b" \
    && run xattr-write-hex xattr -w -x com.example.bin "00 01 02 ff" "$W/mnt/b" \
    && run xattr-rename mv "$W/mnt/b" "$W/mnt/c" \
    && xattr_ok "$W/mnt/c" && XA=ok
  unmount_fs || FAILS=$((FAILS+1))
  # The sampler ends with the daemon; limit the wait all the same.
  for i in $(seq 1 10); do kill -0 $S 2>/dev/null || break; sleep 1; done
  kill $S 2>/dev/null; wait $S 2>/dev/null; S=

  if mount_fs; then
    mounted && run cmp-after-remount cmp "$W/big.bin" "$W/mnt/$F" \
      && [ "$(cat "$W/mnt/c")" = a ] && CMP2=ok
    mounted && { xattr_ok "$W/mnt/c" || XA=FAIL; }
    unmount_fs || FAILS=$((FAILS+1))
  fi
fi
MP=

PEAK=$(( $(cat "$W/peak.kib") / 1024 ))
[ "$CMP1" = ok ] || FAILS=$((FAILS+1))
[ "$CMP2" = ok ] || FAILS=$((FAILS+1))
[ "$REN" = ok ] || FAILS=$((FAILS+1))
[ "$XA" = ok ] || FAILS=$((FAILS+1))
RSS_OK=ok
if [ "$PEAK" -le 0 ] || [ "$PEAK" -gt "$LIMIT" ]; then RSS_OK=FAIL; FAILS=$((FAILS+1)); fi
echo "large-copy file size: $(stat -f %z "$W/big.bin") bytes"
echo "large-copy peak RSS: $PEAK MiB (limit $LIMIT MiB: $RSS_OK)"
echo "large-copy cmp after copy: $CMP1"
echo "large-copy cmp after remount: $CMP2"
echo "large-copy renames: $REN"
echo "large-copy xattrs: $XA"
echo "large-copy commit: $(git rev-parse --short HEAD 2>/dev/null)"
rm -f "$W/big.bin" 2>/dev/null
if [ $FAILS -eq 0 ]; then echo "RESULT large-copy: PASS"; else echo "RESULT large-copy: FAIL ($FAILS)"; fi
[ $FAILS -eq 0 ]
