# removarr-v2

Self-hosted, low-footprint web app (Rust, Axum, Askama, HTMX, SQLite) that audits the storage shared by
qBittorrent, Radarr, Sonarr and Jellyfin: stagnant incomplete downloads, missing hardlinks, exact duplicates,
orphans, permission problems and junk files. It proposes fixes as immutable plans and applies them only after
explicit approval, with a dry-run mode enabled by default.

**Status:** pre-implementation. The repository currently holds the specification and the agent definitions
used to build the application.

## Repository layout

| Path | Content |
|---|---|
| `MASTER-PROMPT.md` | Master specification and the ordered, autonomous build plan for the agents |
| `.claude/agents/` | The 28 agent definitions (`00` to `27`) |
| `old/` | Legacy Python reference: business rules, tests and the context document |
| `docs/PROGRESS.md` | Build progress tracker, used to resume an interrupted build |
| `docs/handoffs/` | One handoff per agent, written during the build |
| `docs/reviews/` | Independent checkpoint reviews, written during the build |

## Building it

Open the repository in Claude Code and ask: "Read `MASTER-PROMPT.md` and execute it". The orchestrator runs
the agents wave by wave, commits locally on an `autonomous-build` branch, and never pushes or publishes.

## Development

The Rust workspace lives in `crates/`. Bootstrap and quality commands (fmt, clippy, tests, release build):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

Details and dependency-direction gates: `docs/development.md`.

## Deployment (Docker Compose)

The image is a static musl binary on a minimal Alpine runtime that includes `ffprobe`. It never runs as root, not even at
startup: there is no `PUID`/`PGID` and no privilege-dropping entrypoint. The identity comes from Compose.

### Install

```sh
cp .env.example .env                 # fill in the two secrets and your roots; never commit .env
openssl rand -hex 64                 # value for REMOVARR_SESSION_KEY
# edit docker-compose.yml: the /srv/media mount, ./config, and the image tag
UID=$(id -u) GID=$(id -g) docker compose up -d
```

Open `http://127.0.0.1:8012`. The port is published on the loopback only. Exposing it on the network is your explicit
choice, ideally behind a TLS reverse proxy (then set `REMOVARR_TRUSTED_PROXIES`). A dry run (`REMOVARR_DRY_RUN=true`) and
manual mode (`REMOVARR_AUTO_MODE=false`) are the defaults: leave them until a dry run looks right.

### Volumes

| Mount | Purpose |
|---|---|
| `/srv/media:/data` | The **common parent** of completed, incomplete and media, as a single volume |
| `./config:/config` | Configuration and the SQLite database, on a separate fast (SSD) volume; outside every root |

Why one mount: `link()` between two separate bind mounts fails with `EXDEV`, even when both are on the same filesystem, so
hardlink consolidation would be impossible. Consequence: the container sees the whole mounted parent. The application only
analyzes the roots you declare (`REMOVARR_COMPLETED_ROOT`, `REMOVARR_INCOMPLETE_ROOT`, `REMOVARR_MEDIA_ROOTS`, for
example `/data/completed`, `/data/incomplete`, `/data/media`).

### Path mappings

Each service reports paths from its own point of view. Map them to the container view with `service_path=app_path`, for
example `REMOVARR_QBIT_PATH_MAP=/downloads=/data/completed` and `REMOVARR_SONARR_PATH_MAP=/tv=/data/media`. Identity is
never assumed. See `.env.example` and `examples/removarr.toml`.

### Permissions

* Set `user: "${UID}:${GID}"` to the owner of your media (the user running qBittorrent, Sonarr and Radarr).
* `./config` must be writable by that uid:gid: `mkdir -p config && chown "$(id -u):$(id -g)" config`.
* The container root filesystem is read-only, all capabilities are dropped, and `no-new-privileges` is set.
* Rootless Docker or Podman: the uid inside the container maps to your user through the user namespace. Keep the same
  `user:` value and make sure the mounted directories are owned by that mapped user.
* Files owned by another user cannot be fixed by the application; `REMOVARR_EXPECTED_OWNER` sets the expected owner.

### Healthcheck

`removarr healthcheck` sends one local HTTP request to `/healthz`. It loads no configuration and touches neither the media
roots nor the database, so it never wakes a spinning disk. `/readyz` (one database read) is for orchestrators.

### Upgrade

```sh
docker compose pull && docker compose up -d     # after changing the image tag
docker compose logs -f removarr
```

Pin a version tag (`ghcr.io/garnajee/removarr-v2:1.2.3`) in production rather than `latest`. Database migrations run forward
only at startup: take a backup before every upgrade.

### Backup and rollback

Back up the SQLite database from the config volume. Stop the container for a consistent copy (the `-wal` and `-shm` files
belong with the database):

```sh
docker compose stop removarr
cp -a config "config.backup-$(date +%Y%m%d)"
docker compose start removarr
```

Rollback: stop the container, restore the backup directory over `./config`, set the previous image tag, and
`docker compose up -d`. Never run an older version on a database already migrated by a newer one: restore the backup
taken before the upgrade. Backups hold history, not secrets (secrets only live in `.env`, which you back up separately).

### Build locally

```sh
docker build -t removarr:local .     # then set image: removarr:local in docker-compose.yml
```

Release images (`linux/amd64`, `linux/arm64`, SBOM and provenance attached) are published to GHCR only when a
`v*.*.*` tag is pushed. Pull requests and pushes to `main` run formatting, Clippy, tests, a release build, a dependency
advisory check and an unpublished image build.

## Safety principles

- Hardlinks are proven by identical `(device, inode)`, never by link count or file name.
- qBittorrent tasks are removed with `deleteFiles=false` only.
- Sonarr and Radarr are the only authority on release quality.
- Only explicitly configured paths are ever scanned; every ambiguity fails closed.

## License

To be defined.
