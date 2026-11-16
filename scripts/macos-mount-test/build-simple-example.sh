#!/bin/bash
# Build the fuser `simple` example (from vendor/fuser, in a temp copy) with an
# option to pass FUSE-T mount options (FUSET_OPTS, e.g. backend=fskit).
# usage: build-simple-example.sh <out dir>   -> <out dir>/simple
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd); OUT=$1; T=$(mktemp -d)
cp -R "$REPO/vendor/fuser" "$T/fuser"
python3 - "$T/fuser/examples/simple.rs" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
old = '    cfg.mount_options = vec![MountOption::FSName("fuser".to_string())];'
new = old + '''
    if let Ok(o) = std::env::var("FUSET_OPTS") {
        for x in o.split(',').filter(|x| !x.is_empty()) {
            cfg.mount_options.push(MountOption::CUSTOM(x.to_string()));
        }
    }'''
assert old in s
open(p, 'w').write(s.replace(old, new, 1))
PY
cd "$T/fuser"
PKG_CONFIG_PATH="$REPO/.pkgconfig" CARGO_TARGET_DIR="$T/target" cargo build -q --release --config 'build.rustc="rustc"' --example simple
mkdir -p "$OUT" && cp "$T/target/release/examples/simple" "$OUT/simple"
