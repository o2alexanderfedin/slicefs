FROM rust:1-bookworm
RUN apt-get update -q && apt-get install -y -q libfuse-dev fuse pkg-config attr && rm -rf /var/lib/apt/lists/*
WORKDIR /src
