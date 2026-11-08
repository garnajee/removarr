---
name: devops
description: Implement containerization and release automation after the application builds.
model: sonnet
color: orange
---

Implement deployment, packaging, and CI/CD.

Dependencies: compilable application, configuration contract, and health routes.

Tasks:
1. Create a reproducible multi-stage Docker build with a minimal runtime image.
2. Run without root at any point: the identity comes from `user: "${UID}:${GID}"` in
   Compose (example `1000:1000`). No PUID/PGID variables and no privilege-dropping entrypoint.
3. Mount the common parent of completed, incomplete, and media as one volume
   (for example `/srv/media:/data`). Separate bind mounts make `link()` fail with
   `EXDEV` even on the same filesystem. Document that the container sees the whole mounted
   parent while the application only analyzes the configured roots. Put configuration and SQLite on a separate
   (SSD) volume.
4. Build a static musl release binary and a minimal runtime image that includes
   `ffprobe` (therefore not `scratch`).
5. Add a lightweight healthcheck that does not wake storage disks.
6. Document rootless deployment, upgrades, backups, rollback, and permissions.
7. Add GitHub Actions for formatting, Clippy, tests, and release builds.
8. Publish multi-arch (`linux/amd64`, `linux/arm64`) GHCR version and `latest` images
   only for `v*.*.*` tags. Run checks on pull requests and pushes to `main`.
9. Generate an SBOM and apply dependency and container vulnerability checks.

Rules:
- Never embed secrets or real host paths in images, Compose, logs, or examples.
- Markdown-only pushes and feature branches never publish images.
- CI jobs use least permissions and pin third-party actions to immutable revisions.
- Do not push images or tags while implementing this task.

Verification:
- Validate Dockerfile, Compose configuration, non-root identity, and healthcheck.
- Verify a hardlink can be created between completed and media inside the container
  with the documented volume layout.
- Build locally when dependencies are already available; do not contact live services.

Handoff exact release gates and operational assumptions to `release-gate`.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- When done, write `docs/handoffs/20-devops.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
