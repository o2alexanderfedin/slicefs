# Power-loss test of `slicefs rehash` (see README.md). The Rust version is the
# one of scripts/docker-linux-test/Dockerfile. This image has FUSE 3 (for
# LazyFS) instead of FUSE 2: the Debian packages fuse and fuse3 conflict, and
# slicefs needs neither library (the fuser crate mounts with no libfuse).
FROM rust:1.100.0-bookworm
RUN apt-get update -q && apt-get install -y -q libfuse3-dev fuse3 cmake g++ pkg-config && rm -rf /var/lib/apt/lists/*
# LazyFS (github.com/dsrhaslab/lazyfs), pinned to the commit of release 0.3.1.
ARG LAZYFS_COMMIT=045a0b3a1126725e693934e29d3ba15e08cc39ec
RUN git clone -q https://github.com/dsrhaslab/lazyfs.git /opt/lazyfs \
 && cd /opt/lazyfs && git checkout -q "$LAZYFS_COMMIT" \
 && cd libs/libpcache && ./build.sh >/dev/null \
 && cd /opt/lazyfs/lazyfs && ./build.sh >/dev/null \
 && test -x /opt/lazyfs/lazyfs/build/lazyfs
# The crash shim: the process kills itself at its N-th sync-type call.
COPY plshim.c /opt/plshim.c
RUN gcc -O2 -shared -fPIC -o /opt/plshim.so /opt/plshim.c -ldl
WORKDIR /src
