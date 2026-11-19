# chappe-bus: one static binary in an empty image. musl, so there is no libc to
# ship; rusqlite compiles its own SQLite. The binary is built for the platform the
# image is built for (linux/amd64 or linux/arm64).
# Build from this directory:
#   docker build --target check .      fmt, clippy and tests (CI runs this first)
#   docker build -t chappe-bus .        the image
#   docker buildx build --platform linux/amd64,linux/arm64 -t chappe-bus .
FROM docker.io/library/rust:1.100.0-bookworm AS base

RUN apt-get update -qq \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy
# `musl-gcc` is the C compiler for the musl target of either architecture.
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CC_aarch64_unknown_linux_musl=musl-gcc

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The tests read the templates of the adapters.
COPY adapters adapters

FROM base AS check
RUN cargo fmt --check \
    && cargo clippy --locked --all-targets -- -D warnings \
    && cargo test --locked

FROM base AS build
RUN target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --release --locked --target "$target" -p chappe-bus \
    && mkdir -p /out/data \
    && cp "target/$target/release/chappe-bus" /out/chappe-bus

FROM scratch
LABEL org.opencontainers.image.title="chappe-bus" \
      org.opencontainers.image.description="Coordination service for AI coding agents" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build /out/chappe-bus /chappe-bus
# The data volume is mounted at /data; the service creates its database there. The
# directory is in the image and belongs to the user of the service, so a new named
# volume starts out writable.
COPY --from=build --chown=65534:65534 /out/data /data
ENV CHAPPE_DB=/data/chappe-bus.db
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/chappe-bus"]
