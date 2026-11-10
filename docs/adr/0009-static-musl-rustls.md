# ADR-0009: Static musl binary with rustls (ring); no OpenSSL

Status: accepted (wave 1). Details: `docs/architecture.md` section 3.

## Context
MASTER §8 requires a static musl release binary, a minimal runtime image containing `ffprobe`,
and multi-arch amd64/arm64. The local toolchain is Homebrew rustc 1.98 without rustup (host
target only).

## Decision
- TLS uses `reqwest` with `rustls-tls` (ring provider, webpki roots). OpenSSL and native-tls
  are forbidden in the dependency tree.
- SQLite is bundled through `sqlx` `sqlite` (libsqlite3-sys bundled).
- Static `*-unknown-linux-musl` builds happen only in the Docker builder (Alpine, rustc 1.98,
  `build-base` for the C parts of ring and sqlite), per architecture. The runtime is a minimal
  Alpine image with `ffmpeg` (for `ffprobe`), running non-root.
- Local development and tests use the host target. Linux-only calls are `cfg`-gated with fallbacks.

## Consequences
- No system TLS or CA dependency in the image.
- The C toolchain is needed in the builder stage only.
- Changing the TLS stack later would touch every connector crate.
