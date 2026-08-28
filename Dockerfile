# syntax=docker/dockerfile:1
# The tag is a floor: rust-toolchain.toml pins the exact compiler and rustup
# inside the image honours it, so this only has to be new enough not to fight
# that. Keeping the two in step avoids downloading a second toolchain on every
# build.
FROM rust:1.98-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends clang cmake libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Cargo features to compile in. `--build-arg FEATURES=tls` produces an image
# that can terminate TLS and speak TLS to the origin; the default does not,
# because most deployments terminate at a load balancer and an unused TLS stack
# is unused attack surface. A binary built without it *rejects* a config
# containing `server.tls` or `origin.tls` rather than leaving the port dead.
ARG FEATURES=""

WORKDIR /src
COPY . .
RUN --mount=type=cache,id=harmost-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=harmost-target,target=/src/target \
    cargo build --release --locked --bin harmost \
      ${FEATURES:+--features "$FEATURES"} \
    && cp /src/target/release/harmost /tmp/harmost

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home harmost

COPY --from=builder /tmp/harmost /usr/local/bin/harmost

USER harmost
# 8080 traffic, 8443 TLS (only in an image built with FEATURES=tls),
# 9090 Prometheus, 9091 the admin endpoints. The last two are operator
# surfaces: publish them to a private network, never to the internet.
EXPOSE 8080 8443 9090 9091
ENTRYPOINT ["/usr/local/bin/harmost"]
CMD ["run", "--config", "/etc/harmost/harmost.yaml"]
