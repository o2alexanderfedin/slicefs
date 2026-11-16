#!/bin/bash
# Download Boost source releases as uncompressed tar files plus expanded trees.
# usage: fetch-boost.sh <dir> <versions...>   e.g. fetch-boost.sh ~/boost 1_85_0
set -euo pipefail
DIR=$1; shift; mkdir -p "$DIR/x"
for v in "$@"; do
  [ -f "$DIR/boost_$v.tar" ] && [ -d "$DIR/x/boost_$v" ] && continue
  dotted=${v//_/.}
  curl -fsSL "https://archives.boost.io/release/$dotted/source/boost_$v.tar.bz2" -o "$DIR/boost_$v.tar.bz2"
  bunzip2 -f "$DIR/boost_$v.tar.bz2"
  tar -xf "$DIR/boost_$v.tar" -C "$DIR/x"
done
