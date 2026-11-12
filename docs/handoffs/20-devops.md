# Handoff 20 - devops (wave 13)

## Delivered scope
`Dockerfile`, `.dockerignore`, `docker-compose.yml` (commented template), `deny.toml`, `.github/workflows/ci.yml`,
`.github/workflows/release.yml`, `.github/ISSUE_TEMPLATE/{bug_report,feature_request}.md`, README "Deployment (Docker Compose)" section
(install, volumes and EXDEV, path mappings, permissions incl. rootless, healthcheck, upgrade, backup, rollback, local build). No Rust file,
no root `Cargo.toml` edited. Nothing committed, pushed or tagged.

## Public contracts
* Image: Alpine 3.22 runtime + `ffmpeg` package (ffprobe 6.1.2) + static musl `/usr/local/bin/removarr`. Entrypoint `removarr`, cmd `serve`.
  Default `USER 65532:65532` (only applies without `--user`); Compose sets `user: "${UID}:${GID}"`. No entrypoint script, no PUID/PGID.
  ENV: `REMOVARR_BIND=0.0.0.0:8080`, `REMOVARR_DATA_DIR=/config`, `HOME=/tmp`. `/config` is `1777` in the image. `REMOVARR_CONFIG_FILE`
  is deliberately NOT preset (config treats an explicitly set missing file as an error; default `/config/removarr.toml` is optional).
* Healthcheck: `removarr healthcheck` (30 s interval, 5 s timeout, 3 retries, 10 s start period), no config load, no root or DB access.
* Compose: port `127.0.0.1:8012:8080`; volumes `/srv/media:/data` (single common parent) and `./config:/config`; `read_only`, `tmpfs /tmp`,
  `cap_drop ALL`, `no-new-privileges`; `env_file: .env`; `REMOVARR_DRY_RUN` default true and `REMOVARR_AUTO_MODE` default false.
* Build: `cargo build --release --locked --target <arch>-unknown-linux-musl -p removarr` with `+crt-static`; the explicit `--target` is required
  (without it proc-macros such as askama_derive fail on a musl host). The build stage asserts the binary is static.

## Release gates (for release-gate)
1. `ci.yml` green on the release commit: fmt, clippy `-D warnings`, test, release build, cargo-deny (advisories, bans, sources), unpublished image build + smoke test.
2. `release.yml` runs only on tags `v*.*.*`: re-runs fmt/clippy/test/deny, builds amd64 and scans it with Trivy (fails on fixable HIGH/CRITICAL),
   then pushes `linux/amd64,linux/arm64` with tags `<version>` and `latest`, SBOM (`sbom: true`) and provenance (`mode=max`).
3. Permissions: workflow default `contents: read`; only the `publish` job has `packages: write`. No other secrets used (`GITHUB_TOKEN` only).
4. Actions are pinned (SHAs below). Confirm the GHCR package is created and its visibility chosen by the owner.
5. Manual: the tag should point at a commit on `main` (not enforced by the workflow).

## Operational assumptions
* One mount for the common parent is mandatory: verified that two separate mounts give `Cross-device link` (see commands).
* `${UID}:${GID}` must own the media and be able to write `./config`. In zsh `UID` is read-only: put `UID`/`GID` in `.env` or use `env UID=.. GID=.. docker compose ...`.
* Secrets only via `.env` (`$` doubled as `$$` for the Argon2 hash). `.env` is git- and docker-ignored.
* Backup is a stopped-container copy of the config volume (db plus `-wal`/`-shm`); rollback restores the pre-upgrade backup because migrations are forward-only.
* Image name is `ghcr.io/garnajee/removarr-v2` (repository `garnajee/removarr-v2`); release.yml derives it from `github.repository`, compose and README use the same name.
* Healthcheck uses `REMOVARR_BIND` from the container env; a user changing the bind port must keep it in the container env (not only the TOML file).

## Deviations
* Runtime has the full Alpine `ffmpeg` package (192 MB image) instead of an ffprobe-only static binary; no smaller official ffprobe-only package exists. Possible later optimisation.
* Trivy and cargo-deny are used instead of `cargo audit`; cargo-deny is installed from crates.io at a pinned version in the job (no third-party action).
* No auth-guard/ux-design dependency: the image does not serve static assets differently; if 19 adds a `static/` directory it must be embedded in the binary (read-only rootfs).

## Open TODOs
* Actions pinned (resolved with read-only `gh api` GETs, annotated tags dereferenced, each SHA equals `commits/<tag>`; exact in-major releases chosen, major bumps not taken):
  `actions/checkout` 34e114876b0b11c390a56381ad16ebd13914f8d5 (v4.3.1); `docker/setup-qemu-action` c7c53464625b32c7a7e944ae62b3e17d2b600130 (v3.7.0);
  `docker/setup-buildx-action` 8d2750c68a42422c14e847fe6c8ac0403b4cbd6f (v3.12.0); `docker/metadata-action` c299e40c65443455700f0fdfc63efafe5b349051 (v5.10.0);
  `docker/build-push-action` 10e90e3645eae34f1e60eeb005ba3a3d33f178e8 (v6.19.2); `docker/login-action` c94ce9fb468520275223c153574b00df6fe4bcc9 (v3.7.0);
  `aquasecurity/trivy-action` 915b19bbe73b92a6cf82a1bc12b087c9a19a5fe2 (v0.28.0). Newer majors exist (checkout v7, build-push v7, ...); upgrading is optional.
  Review the trivy-action SHA once more before the first release (tag reputation could not be audited offline).
* Base images are pinned by version tag, not digest (`rust:1.91.1-alpine3.22`, `alpine:3.22`); add digests when a registry is reachable.
* `deny.toml` has no license policy; add one if wanted. CI rust toolchain 1.91.1 must stay in sync with `ARG RUST_VERSION`.
* arm64 image not built locally (no QEMU here); it is exercised only by `release.yml`. Build uses native Alpine toolchain on both arches.
* No screenshot in README (UI not final; 19).

## Commands run and results (host: macOS arm64, Colima Docker 29.x)
* `docker build -t removarr:local-test .` : first attempt failed (`cannot produce proc-macro for askama_derive` with crt-static and no `--target`); fixed; second build pass (aarch64 musl), final image 192 MB.
* `docker run --rm --user 1000:1000 --entrypoint id`: `uid=1000 gid=1000`; with no `--user`: uid 65532. Never root.
* `ffprobe -version` in image: 6.1.2. `removarr --version`: `removarr 0.1.0`. `removarr healthcheck` with no server: exit 1.
* Static: `ldd` reports "Not a valid dynamic program" (static).
* Hardlink: Docker named volume mounted once as `/data`, `ln /data/completed/a.mkv /data/media/a.mkv` as uid 1000: same `dev:inode`, links=2. Control with two separate mounts: `ln: Cross-device link` (EXDEV). (Bind mount of a macOS host dir through Colima did not give real hardlinks, so a Linux-native volume was used.)
* Hardened run (`--read-only --tmpfs /tmp --cap-drop ALL no-new-privileges`, uid 1000, dummy secrets, dry run): container healthy via Docker HEALTHCHECK, `docker exec removarr healthcheck` exit 0, logs on stdout, `listening 0.0.0.0:8080`.
* `docker-compose config` with the `.env.example` as `.env` and `UID=1000 GID=1000`: valid; user `1000:1000`, host_ip 127.0.0.1 published 8012, read_only, DRY_RUN "true", AUTO_MODE "false".
* Workflow YAML parsed with ruby YAML: both files valid. Not run on GitHub.
* Test container, volumes and the local image were removed. No push, no registry tag, no live service contacted (only base images and crates pulled).
