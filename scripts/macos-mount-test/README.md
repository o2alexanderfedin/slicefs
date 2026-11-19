# macOS mount tests (FUSE-T)

Real mounts on macOS through FUSE-T. CI runs them in `.github/workflows/macos-mounts.yml`.

| Script | What |
|---|---|
| `fetch-boost.sh <dir> 1_85_0` | Download a Boost release as a tar file and an expanded tree (about 78k files) |
| `boost-test.sh <backend> 1_85_0` | Round trip: tar and tree to the mount and back, compare; remount and compare; delete all; GC on the mounted store; offline GC check |
| `enable-fskit.sh` | Enable the FUSE-T FSKit module without System Settings (for CI): adds it to the FSKit `enabledModules.plist` and restarts the FSKit daemons |

Local run:

```sh
cargo build --release -p slicefs-cli
scripts/macos-mount-test/fetch-boost.sh ~/boost 1_85_0
SLICEFS=target/release/slicefs BOOST_DIR=~/boost scripts/macos-mount-test/boost-test.sh fskit 1_85_0
```

## Why SMB is not a backend

SliceFS mounts on macOS with FUSE-T's FSKit backend only. The SMB backend was
removed: on macOS 26.7, `close()` of a file on the SMB mount takes about 41 ms
when the file was just read through the page cache (`cat`, `diff`); with
`F_NOCACHE` (as `cp` reads) it takes 0.5 ms. A packet capture showed that the
server replies to the SMB `CLOSE` at once, so the wait is in the macOS SMB
client (macOS 26.2 does not have it). In CI, `diff -r` of the Boost tree
(77,655 files) took 4090 s on SMB and 734 s on FSKit.

## Large copy on FSKit (#20)

The check copies a file of 1 GiB into an FSKit mount and records the peak
memory (RSS) of the daemon. Then it compares the file after the copy and after
a remount.

CI runs it too: `large-copy.sh` does the steps below without a person, with a
time limit on each wait, and the macOS mounts workflow (`macos-mounts.yml`)
runs it as the step "Large copy (fskit)" before the Boost round trip. It prints
`RESULT large-copy: PASS` or `FAIL`; it fails when a `cmp` fails or the peak
RSS is above 512 MiB (`RSS_LIMIT_MIB`). To run it by hand:
`scripts/macos-mount-test/large-copy.sh fskit`.

The manual steps:

Before you start, enable FSKit (`enable-fskit.sh`, see the table above). Run
the steps in `bash` from the root of the repository:

```sh
cargo build --release -p slicefs-cli
B=target/release/slicefs
W=$(cd "$(mktemp -d)" && pwd -P)
mkdir $W/seed $W/mnt
echo s > $W/seed/s.txt
$B seed $W/store $W/seed
head -c 1073741824 /dev/urandom > $W/big.bin

mounted() { mount | grep -q "${W#/private}/mnt "; }
waitmnt() { for i in $(seq 1 60); do mounted && return 0; sleep 1; done; echo "MOUNT FAILED"; return 1; }
waitexit() { for i in $(seq 1 240); do kill -0 $1 2>/dev/null || return 0; sleep 0.5; done; echo "DAEMON DID NOT EXIT in 120 s"; return 1; }

$B mount --store $W/store --backend fskit $W/mnt </dev/null >$W/mount.log 2>&1 &
MP=$!
waitmnt
echo 0 > $W/peak.kib
( while kill -0 $MP 2>/dev/null; do
    r=$(ps -o rss= -p $MP | tr -d ' ')
    [ -n "$r" ] && [ "$r" -gt "$(cat $W/peak.kib)" ] && echo $r > $W/peak.kib
    sleep 1
  done ) &
S=$!

mounted && cp $W/big.bin $W/mnt/big.bin
mounted && cmp $W/big.bin $W/mnt/big.bin && echo "cmp after copy: ok" || echo "cmp after copy: FAIL"
$B unmount $W/mnt
waitexit $MP
wait $S

$B mount --store $W/store --backend fskit $W/mnt </dev/null >>$W/mount.log 2>&1 &
MP=$!
waitmnt
mounted && cmp $W/big.bin $W/mnt/big.bin && echo "cmp after remount: ok" || echo "cmp after remount: FAIL"
$B unmount $W/mnt
waitexit $MP

echo "file size: $(stat -f %z $W/big.bin) bytes"
echo "peak RSS: $(( $(cat $W/peak.kib) / 1024 )) MiB"
echo "commit: $(git rev-parse --short HEAD)"
```

The sampler reads `ps -o rss=` (KiB) each second during the copy session and
keeps the largest value. It stops when the daemon exits at the unmount. A
file larger than 1 GiB is also good: change the `head -c` size.

Report these values in #20: the file size, the peak RSS in MiB, the result of
`cmp` after the copy and after the remount, and the commit. When you are done,
remove the work directory (`rm -rf $W`).

The case with many small files on FSKit is the Boost round trip above
(`boost-test.sh fskit`).
