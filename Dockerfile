# syntax=docker/dockerfile:1
# removarr image: static musl binary + minimal Alpine runtime with ffprobe.
# Build for the native platform: docker build -t removarr:local .
# Multi-arch (amd64 + arm64) images are built by .github/workflows/release.yml.
# The Alpine toolchain is musl-native, so no cross compiler is needed on either architecture.

ARG RUST_VERSION=1.91.1
ARG ALPINE_VERSION=3.22

# ---- build -------------------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build
RUN apk add --no-cache build-base musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY locales ./locales
# --locked: the committed Cargo.lock is the only source of versions (reproducible).
# crt-static: fully static binary (rustls, bundled SQLite, no OpenSSL). An explicit --target keeps the flag
# off proc-macros and build scripts, which must stay dynamic on a musl host.
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    triple="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --locked --target "$triple" -p removarr \
    && install -D "target/$triple/release/removarr" /out/removarr \
    && ! ldd /out/removarr 2>/dev/null | grep -qv "statically linked"

# ---- runtime -----------------------------------------------------------------------------------
FROM alpine:${ALPINE_VERSION} AS runtime
# ffprobe ships in the ffmpeg package. No shell login, no extra tools beyond busybox.
RUN apk add --no-cache ffmpeg \
    && mkdir -p /config \
    && chmod 1777 /config
COPY --from=build /out/removarr /usr/local/bin/removarr

# Never root: the real identity is set at run time by `user: "${UID}:${GID}"` (Compose).
# This numeric default only applies when no --user is given. No entrypoint script, no PUID/PGID.
USER 65532:65532
ENV REMOVARR_BIND=0.0.0.0:8080 \
    REMOVARR_DATA_DIR=/config \
    HOME=/tmp
VOLUME ["/config"]
EXPOSE 8080

# Local TCP request to /healthz: touches neither the media roots nor the database, so no disk wakes.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["removarr", "healthcheck"]

ENTRYPOINT ["removarr"]
CMD ["serve"]
