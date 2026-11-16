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
