# syntax=docker/dockerfile:1
FROM rust:1.88-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends clang cmake libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN --mount=type=cache,id=harmost-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=harmost-target,target=/src/target \
    cargo build --release --locked --bin harmost \
    && cp /src/target/release/harmost /tmp/harmost

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home harmost

COPY --from=builder /tmp/harmost /usr/local/bin/harmost

USER harmost
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/harmost"]
CMD ["run", "--config", "/etc/harmost/harmost.yaml"]
