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
SLICEFS=target/release/slicefs BOOST_DIR=~/boost scripts/macos-mount-test/boost-test.sh smb 1_85_0
```
