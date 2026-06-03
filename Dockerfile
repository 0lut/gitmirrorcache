FROM rust:1.94-bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p git-cache-api --features s3

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /home/git-cache git-cache \
    && mkdir -p /cache \
    && chown -R git-cache:git-cache /cache

COPY --from=builder /src/target/release/git-cache-api /usr/local/bin/git-cache-api

ENV GIT_CACHE_BIND_ADDR=0.0.0.0:8080
ENV GIT_CACHE_ROOT=/cache
ENV GIT_CACHE_OBJECT_STORE_KIND=s3
ENV GIT_CACHE_GIT_BINARY=git
ENV RUST_LOG=info

USER git-cache
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/git-cache-api"]
