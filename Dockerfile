# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

ARG RUST_IMAGE=rust:1.94.1-slim-bookworm@sha256:cf9dd0ec73e75f827fe59123fff9dc65af1a1c8363c3c31ee8d7f8ad0b6a5fb2
ARG RUNTIME_IMAGE=debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

FROM ${RUST_IMAGE} AS development

RUN cargo install --locked --version 8.5.3 cargo-watch

WORKDIR /workspace

CMD ["cargo", "watch", "--watch", "crates", "--watch", "Cargo.toml", "--watch", "Cargo.lock", "--exec", "run --locked -p lucy -- serve"]

FROM ${RUST_IMAGE} AS builder

WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release -p lucy \
    && cp /workspace/target/release/lucy /tmp/lucy

FROM ${RUNTIME_IMAGE} AS runtime

ARG VERSION=0.1.0-dev
ARG REVISION=unknown
ARG SOURCE_URL=https://github.com/poorwym/Lucy

LABEL org.opencontainers.image.title="Lucy" \
      org.opencontainers.image.description="Server-only PostGIS to 3D Tiles middleware" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.licenses="MIT"

RUN groupadd --gid 10001 lucy \
    && useradd --uid 10001 --gid lucy --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin lucy \
    && install -d -o root -g root -m 0555 /etc/lucy

COPY --from=builder --chown=lucy:lucy /tmp/lucy /usr/local/bin/lucy

ENV LUCY_CONFIG=/etc/lucy/config.yaml \
    LUCY_BIND=0.0.0.0:8080

USER lucy:lucy
WORKDIR /

EXPOSE 8080
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
    CMD ["/usr/local/bin/lucy", "__healthcheck"]

ENTRYPOINT ["/usr/local/bin/lucy"]
CMD ["serve"]
