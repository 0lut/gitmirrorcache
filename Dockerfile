# syntax=docker/dockerfile:1.7

FROM rust:1.94-bookworm AS builder

WORKDIR /src
COPY . .
RUN --mount=type=cache,id=git-cache-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=git-cache-cargo-git,target=/usr/local/cargo/git \
    cargo build --release --features s3 -p git-cache-api -p git-cache-cli \
    && mkdir -p /out \
    && cp target/release/git-cache-api target/release/git-cache /out/

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git util-linux \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /home/git-cache git-cache \
    && mkdir -p /cache \
    && chown -R git-cache:git-cache /cache

COPY --from=builder /out/git-cache-api /usr/local/bin/git-cache-api
COPY --from=builder /out/git-cache /usr/local/bin/git-cache

ENV GIT_CACHE_BIND_ADDR=0.0.0.0:8080
ENV GIT_CACHE_ROOT=/cache
ENV GIT_CACHE_OBJECT_STORE_KIND=s3
ENV GIT_CACHE_GIT_BINARY=git
ENV RUST_LOG=info

USER git-cache
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/git-cache-api"]
