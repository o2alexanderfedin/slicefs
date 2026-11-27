# Pinned to RUST_STABLE in .github/workflows/safety.yml, so that the coverage gate and
# the ci.yml Linux job do not change with a new Rust release. Change both together.
FROM rust:1.100.0-bookworm
RUN apt-get update -q && apt-get install -y -q libfuse-dev fuse pkg-config attr jq && rm -rf /var/lib/apt/lists/*
# Coverage (Phase 21.2, TSAFE-05, mode `coverage` of run.sh): llvm-tools for the
# image toolchain and cargo-llvm-cov from its release tarball, pinned to one
# version. LLVM_COV_SHA256 is the SHA-256 of that tarball (the same value as the
# digest GitHub shows for the release asset); change both together.
RUN rustup component add llvm-tools-preview
ARG LLVM_COV_VERSION=0.9.2
ARG LLVM_COV_SHA256=1e0ed5c64f11e9217f7b9d7c776b3125eab26f27d49313919c909881a2fc7cff
RUN curl -fsSL -o /tmp/llvm-cov.tar.gz \
      "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/cargo-llvm-cov-x86_64-unknown-linux-gnu.tar.gz" \
 && echo "${LLVM_COV_SHA256}  /tmp/llvm-cov.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/llvm-cov.tar.gz -C /usr/local/cargo/bin cargo-llvm-cov \
 && rm /tmp/llvm-cov.tar.gz
WORKDIR /src
