# removarr-v2

A self-hosted web app that audits the storage shared by **qBittorrent**, **Sonarr**, **Radarr** and **Jellyfin**,
then helps you clean it up safely.

It finds missing hardlinks, exact duplicates, stalled downloads, orphans, junk files and permission problems.
Every fix is proposed as a plan you review and approve. Nothing changes on disk until you say so, and the
default mode is a **dry run** that changes nothing at all.

Written in Rust (Axum, Askama, HTMX, SQLite). It ships as one small, non-root Docker image for `amd64` and `arm64`.

## Contents

- [What it does](#what-it-does)
- [Safety model](#safety-model)
- [Requirements](#requirements)
- [Installation](#installation)
  - [1. Get the image](#1-get-the-image)
  - [2. Prepare the directories](#2-prepare-the-directories)
  - [3. Create the secrets](#3-create-the-secrets)
  - [4. Fill in `.env`](#4-fill-in-env)
  - [5. Adjust `docker-compose.yml`](#5-adjust-docker-composeyml)
  - [6. Start and sign in](#6-start-and-sign-in)
  - [7. First dry run](#7-first-dry-run)
  - [8. Going live](#8-going-live)
- [Configuration](#configuration)
- [Reverse proxy](#reverse-proxy)
- [Operations](#operations)
- [Known limitations](#known-limitations)
- [Development](#development)
- [Project layout](#project-layout)
- [License](#license)

## What it does

1. **Observes.** It scans the roots you declare (completed downloads, incomplete downloads, media library) and reads
   the qBittorrent, Sonarr, Radarr and Jellyfin APIs. A file's identity is its `(device, inode)` pair.
2. **Classifies** what it finds into issues:

   | Category | Meaning |
   |---|---|
   | Missing hardlinks | A completed download and its library copy are separate files instead of one hardlinked file |
   | Duplicates | Byte-identical copies, proven by a full comparison |
   | Stalled downloads, incomplete orphans | Incomplete data that is stuck or belongs to no torrent |
   | Completed orphans, unmapped torrents | Completed data that no torrent or path mapping accounts for |
   | Torrents with missing files, protected torrents | Torrents in a state that blocks any action |
   | ARR queue | Sonarr or Radarr queue items that need attention |
   | Junk files, symlinks, permissions | Leftovers outside any active torrent, links, wrong owner or mode |
   | Media without source | Library files with no download counterpart (information only, never deleted) |

3. **Plans.** You select issues and get an immutable plan that previews every action and the space it would free.
4. **Executes** an approved plan step by step, records each step before doing it, then verifies the result.
5. **Reports** history, freed space and statistics, without counting the same gain twice.

## Safety model

- **Dry run by default.** `REMOVARR_DRY_RUN=true` runs the real code path with every mutation disabled.
- **Nothing happens without approval.** Plans expire, and approving one requires an explicit confirmation.
- **Hardlinks are proven, not guessed:** same `(device, inode)`, never the link count or the file name.
- **Duplicates are proven by a full byte comparison** before anything is replaced.
- **qBittorrent tasks are only ever removed with `deleteFiles=false`.** Sonarr and Radarr imports always copy.
- **Media roots are never cleaned automatically.** Samples are protected.
- **Torrents are only touched in a safe state:** complete, and in a seeding or paused state for your qBittorrent
  version (4.x or 5.x).
- **Every action is journaled before it runs**, so an interrupted run is recovered safely at the next start.
- **Automatic mode is off by default.** When enabled, it pauses at the first uncertainty.
- **Only configured roots are scanned.** Anything ambiguous fails closed.

## Requirements

- Docker with Compose v2 (Docker Engine, Docker Desktop, Podman, or the Container Manager on a Synology NAS).
- Your completed downloads, incomplete downloads and media library under **one common parent directory**, for
  example `/srv/media/completed`, `/srv/media/incomplete` and `/srv/media/media`. Hardlinks only work inside a
  single mount (see [step 5](#5-adjust-docker-composeyml)).
- The uid:gid that owns your media files (usually the user running qBittorrent, Sonarr and Radarr).
- Optional: the URLs and API keys of qBittorrent, Sonarr, Radarr and Jellyfin. Without them the app still audits the
  filesystem, but proposes fewer fixes.

## Installation

The steps below use example paths. Replace `/srv/media` and the folder names with your own.

### 1. Get the image

Clone the repository; it holds the Compose file and the templates:

```sh
git clone https://github.com/garnajee/removarr-v2.git
cd removarr-v2
```

Then either:

- **Pull a released image:** `ghcr.io/garnajee/removarr-v2:<version>`. Images are published when a `v*.*.*` tag is
  pushed. If the package is private, sign in first with `docker login ghcr.io`.
- **Or build it locally** (works right away):
  ```sh
  docker build -t removarr:local .
  ```
  Then set `image: removarr:local` in `docker-compose.yml`.

The commands below use `IMAGE` for whichever image you chose, for example `IMAGE=removarr:local`.

### 2. Prepare the directories

Create the config directory. It holds the SQLite database and must be writable by your media owner:

```sh
mkdir -p config
chown 1000:1000 config      # replace 1000:1000 with the uid:gid that owns your media
```

Put `config` on a fast disk (an SSD if you have one) and outside every media root.

### 3. Create the secrets

Sign-in uses one account protected by an Argon2id password hash and a session key.

```sh
docker run --rm -i "$IMAGE" hash-password     # type the password; it is read from stdin, never from an argument
openssl rand -hex 64                          # session key
```

Keep both outputs for the next step.

### 4. Fill in `.env`

```sh
cp .env.example .env
```

Edit `.env` (never commit it):

```sh
# Who the container runs as: the owner of your media
UID=1000
GID=1000

# Secrets. Single quotes keep the $ characters of the hash literal.
REMOVARR_PASSWORD_HASH='$argon2id$v=19$...'
REMOVARR_SESSION_KEY=<output of openssl rand -hex 64>

# Roots, as seen inside the container (the common parent is mounted at /data)
REMOVARR_COMPLETED_ROOT=/data/completed
REMOVARR_INCOMPLETE_ROOT=/data/incomplete
REMOVARR_MEDIA_ROOTS=/data/media

# Keep these until a dry run looks right
REMOVARR_DRY_RUN=true
REMOVARR_AUTO_MODE=false

# Services (optional): URL plus credential
REMOVARR_QBIT_URL=http://qbittorrent:8080
REMOVARR_QBIT_USERNAME=admin
REMOVARR_QBIT_PASSWORD=...
REMOVARR_SONARR_URL=http://sonarr:8989
REMOVARR_SONARR_API_KEY=...
REMOVARR_RADARR_URL=http://radarr:7878
REMOVARR_RADARR_API_KEY=...
REMOVARR_JELLYFIN_URL=http://jellyfin:8096
REMOVARR_JELLYFIN_API_KEY=...

# Path mappings: how each service sees a path = how removarr sees it
REMOVARR_QBIT_PATH_MAP=/downloads=/data/completed,/downloads/incomplete=/data/incomplete
REMOVARR_SONARR_PATH_MAP=/tv=/data/media
REMOVARR_RADARR_PATH_MAP=/movies=/data/media
REMOVARR_JELLYFIN_PATH_MAP=/media=/data/media
```

Path mappings matter: each service reports paths from inside its own container, and removarr never assumes they
are the same as its own. Check the paths in each service's settings and write one `service_path=app_path` pair per
prefix.

`.env.example` lists every variable with its default.

### 5. Adjust `docker-compose.yml`

Change the two lines marked `CHANGE`:

```yaml
volumes:
  - /srv/media:/data        # the COMMON PARENT of completed, incomplete and media, as one mount
  - ./config:/config        # database and optional config file
```

Mount the common parent **once**. `link()` between two separate bind mounts fails with `EXDEV`, even on the same
filesystem, so mounting completed and media separately would make hardlink fixes impossible. The container then
sees the whole parent, but removarr only scans the roots declared in `.env`.

To reach your services by name (`http://sonarr:8989`), attach removarr to the same Docker network as them (see the
`networks` lines at the end of the file).

The port is published on the loopback only: `127.0.0.1:8012`. To reach it from another device, use a reverse proxy
(see [Reverse proxy](#reverse-proxy)), or change the line to `8012:8080` on a trusted network.

### 6. Start and sign in

```sh
docker compose up -d
docker compose ps           # wait for "healthy"
docker compose logs -f removarr
```

Open `http://127.0.0.1:8012` and sign in as `admin` (or `REMOVARR_USERNAME`) with your password.

If you open it over plain HTTP from another machine, sign-in needs `REMOVARR_COOKIE_SECURE=false`. Only do that on a
trusted LAN: the session cookie then travels unencrypted.

### 7. First dry run

1. On the **Dashboard**, start a scan. A first scan of a few tens of thousands of files takes seconds to minutes,
   depending on your disks.
2. Open **Issues** and check each category against what you know of your library. A large number of
   "unmapped" items usually means a path mapping is missing.
3. Select a few issues, create a plan, read the preview, approve it and **simulate** it. The dry run goes through
   every check and reports what it would do, without changing anything.
4. Look at **History**, **Jobs** and **Logs**.

Repeat until the proposals look right.

### 8. Going live

1. Back up `config` (see [Backup and rollback](#backup-and-rollback)).
2. Set `REMOVARR_DRY_RUN=false` in `.env`, then run `docker compose up -d`.
3. Start with one or two simple plans, such as consolidating an exact copy into a hardlink. Check the result in
   qBittorrent and Jellyfin.
4. Enable more capability flags only when you need them (the `REMOVARR_CAP_*` flags in `.env.example`). ARR actions
   (manual import, queue removal, monitoring, grab) are off by default.
5. Automatic mode (`REMOVARR_AUTO_MODE=true`) is optional. It only runs plans that pass every automatic rule:
   minimum age, ARR validation, full comparison, and a complete, reliable scan. It pauses at the first doubt.

## Configuration

Settings come from environment variables (`.env`). An optional TOML file can hold the non-secret settings:
`/config/removarr.toml`, or the path in `REMOVARR_CONFIG_FILE`. A commented example with every key and its default
is in [`examples/removarr.toml`](examples/removarr.toml). Environment variables override the file. Secrets are only
ever read from the environment, and unknown keys are rejected.

Most useful settings:

| Variable | Default | Purpose |
|---|---|---|
| `REMOVARR_DRY_RUN` | `true` | Run without changing anything |
| `REMOVARR_AUTO_MODE` | `false` | Automatic execution of safe plans |
| `REMOVARR_COMPLETED_ROOT` / `REMOVARR_INCOMPLETE_ROOT` / `REMOVARR_MEDIA_ROOTS` | none | The roots to audit (container paths) |
| `REMOVARR_*_PATH_MAP` | none | Service path to container path mappings |
| `REMOVARR_EXPECTED_OWNER` | process uid:gid | Owner expected on files, for permission issues |
| `REMOVARR_SCAN_INTERVAL_MINUTES` | `1440` | Scheduled full scan (`0` = manual only) |
| `REMOVARR_QBIT_REFRESH_MINUTES` | `60` | qBittorrent API refresh, without disk access (`0` = off) |
| `REMOVARR_USERNAME` | `admin` | Sign-in name |
| `REMOVARR_COOKIE_SECURE` | `true` | Send the session cookie over HTTPS only |
| `REMOVARR_TRUSTED_PROXIES` | empty | Reverse proxy addresses or networks |
| `REMOVARR_LOG_LEVEL` | `info` | `debug`, `info`, `warn`, `error` |

## Reverse proxy

To expose removarr beyond the host, put it behind a TLS reverse proxy and keep `REMOVARR_COOKIE_SECURE=true`.

- Set `REMOVARR_TRUSTED_PROXIES` to the proxy address or network, for example `172.18.0.0/16`. Without it,
  removarr ignores `X-Forwarded-For` on purpose, and every client shares the proxy's login rate limit: a few failed
  sign-ins from anyone can block yours for 15 minutes. The log event `forwarded_header_untrusted_peer` warns about
  this. Under Docker port publishing the peer may be the bridge gateway rather than the proxy. Check `client_ip` in
  the `login_*` log lines and trust that address.
- The proxy must keep the `Host` header (or send `X-Forwarded-Host`), otherwise form submissions are refused.

## Operations

### Health

`removarr healthcheck` (used by the image's healthcheck) calls `/healthz` locally. It touches neither the disks nor the
database, so it never wakes a sleeping drive. `/readyz` also checks the database.

### Upgrade

```sh
docker compose stop removarr && cp -a config "config.backup-$(date +%Y%m%d)"
# change the image tag in docker-compose.yml (or rebuild: docker build -t removarr:local .)
docker compose pull && docker compose up -d
docker compose logs -f removarr
```

Pin a version tag in production rather than `latest`. Database migrations run forward at startup.

### Backup and rollback

The database lives in `config` (`removarr.db` with its `-wal` and `-shm` files). Stop the container for a consistent
copy:

```sh
docker compose stop removarr
cp -a config "config.backup-$(date +%Y%m%d)"
docker compose start removarr
```

To roll back, stop the container, restore the backup over `config`, set the previous image tag, then
`docker compose up -d`. Never run an older version on a database migrated by a newer one. `.env` holds the secrets:
back it up separately.

### After a crash

At startup removarr reads its journal. Torrents whose data was not changed are resumed. Uncertain ones stay stopped
and visible, and are listed for review. Temporary files named `.removarr-link-*` may be left next to a file that was
being consolidated; the recovery note tells you which ones you can delete.

## Known limitations

- The settings, file explorer and media pages are not built yet. Settings are edited in `.env` or the TOML file.
  The manual ARR search and grab page exists at `/media/{service}/{id}`, but no page links to it yet.
- Reports RPT-05, RPT-08 and RPT-09 from the specification are not produced.
- Numbers and dates are not localized. The interface is in English and French.
- The dashboard counter for torrents left stopped always shows 0.
- An interrupted job does not link to the job that replaced it.
- Leftover `.removarr-link-*` temporary files are not cleaned up automatically.
- Tested with fakes of the qBittorrent, Sonarr, Radarr and Jellyfin APIs, and in the container. Start with a dry run
  on your own setup.

## Development

Requires a Rust toolchain (1.91 or newer).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p removarr     # binary: target/release/removarr
```

Run locally: `removarr serve` (the defaults read `/config/removarr.toml`; set `REMOVARR_DATA_DIR` and the roots for
a local run). Details, crate boundaries and dependency rules: [`docs/development.md`](docs/development.md).

Optional checks:

- Performance budgets: `cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1`
  (after a release build).
- Container smoke test: `crates/e2e-perf/scripts/container-smoke.sh` (needs Docker).

Continuous integration runs formatting, Clippy, tests, a release build, a dependency advisory check and an image build
on every pull request and push to `main`. Release images (`linux/amd64`, `linux/arm64`, with SBOM and provenance) are
published to GHCR only when a `v*.*.*` tag is pushed. The `latest` tag only moves for non-prerelease versions.

## Project layout

| Path | Content |
|---|---|
| `crates/` | Rust workspace: `domain`, `config`, `storage`, `fs`, `qbit`, `arr`, `jellyfin`, `matching`, `workflows`, `jobs`, `reporting`, `web`, `app` (the `removarr` binary), test crates |
| `locales/` | English and French interface text |
| `examples/removarr.toml` | Commented configuration file |
| `docs/architecture.md` | Architecture, module ownership, performance budgets |
| `docs/release-readiness.md` | Last release assessment |
| `docs/security/` | Security audit reports |
| `MASTER-PROMPT.md`, `.claude/agents/` | The specification and the agents that built the application |

## License

[GNU Affero General Public License v3.0 or later](LICENSE) (AGPL-3.0-or-later).

You may use, modify, fork and sell this software. If you distribute it, or let users interact with a modified
version over a network (for example as a hosted service), you must make the complete corresponding source code
available under the same license.
