# Handoff 23 - e2e-perf (wave 16)

## Verdict for release-gate

**Resource budgets are NOT all met. B1, B2 (rescan), B7 miss on the host measurement and block the release unless 27 records a justified waiver** (no budget was weakened).
B3 (roots untouched, zero fs calls while idle) and B11 pass, so no non-waivable budget is missed. Details and owners below.

## Delivered scope

New crate `crates/e2e-perf` (`removarr-e2e-perf`, tests, scripts and support code only; root `Cargo.toml` unchanged because `members = ["crates/*"]`). No `src/**` of any feature crate or testkit file was edited. It reuses `removarr-integration-tests` (`Stack`, `Options`, `world`, `parity_torrents`).

| File | Purpose |
|---|---|
| `src/lib.rs` | `Live`: production composition root + production `Auth` with a controllable clock on a real `127.0.0.1:0` listener; `Browser` (cookie jar, no redirects, Origin on POSTs, `wait_job`) |
| `src/html.rs` | Markup assertions (csrf, tags, `a11y_findings`, duplicate ids) |
| `src/perf.rs` | Synthetic inventory generator, out-of-process measurement (`Proc`: RSS sampling every 100 ms, `/usr/bin/time -l` I/O totals on macOS, SIGTERM/SIGKILL), tree signature, sqlite3 CLI helpers |
| `tests/auth_http.rs` (6) | Login/logout, session rotation and fixation, forged cookies, idle (2 h) and absolute (12 h) expiry incl. HTMX `401` + `HX-Redirect`, CSRF on every command route (field, header, wrong, foreign origin), both login limits (IP and (IP, identifier)) with `429` + `Retry-After`, shrinking `Retry-After`, window recovery, limiter cleanup and bounded tables under a 300-identifier flood, oversized/malformed login bodies cost no Argon2 |
| `tests/ui_flows.rs` (8) | Locale switching (cookie, `Accept-Language`, invalid values, open-redirect `return_to`), accessible markup on ~15 pages x 2 locales + no raw translation key rendered, English fallback per key and placeholder parity, hostile strings (`<img onerror>`, `<script>`, quotes, `&`) in file names, filters, logs, plans, headers, filters/sort/pagination, dry-run one-item and bulk flow (selection summary, preview, plan, confirmation, wrong count refused, approve, live execute refused, simulate, tree and qBittorrent untouched, discard), ignore/restore, CSS rules (44 px `--tap`, tab bar, focus-visible, reduced motion, forced colors, narrow breakpoint, HTMX asset digest) |
| `tests/stress.rs` (6) | 400-command flood (queue stays at 1 scan, 2 jobs), log ring (200,000 events from 8 threads: 2,000 kept, 198,000 dropped, redaction, truncation), concurrency limits refused/clamped, qBittorrent 503 outage (4 attempts, 7 s, recovery without restart), Radarr 503 outage (8 requests, 12 s, no mutation), unreadable subtree gives a labelled "incomplete" dashboard in en and fr |
| `tests/perf_budgets.rs` (`#[ignore]`) | B1-B11 against the real release binary + Argon2 timing with production parameters |
| `tests/stress_heavy.rs` (`#[ignore]`) | Cancel a scan in flight, SIGKILL mid-rescan then restart over the same 65 MiB database, SIGTERM mid-scan, unreachable qBittorrent/Radarr at start |
| `scripts/container-smoke.sh` | Rootless container smoke test (29 checks) |

## Budget table (measured vs target)

Environment: macOS 26 (Darwin 25.5) arm64, Apple M3 8 cores, 24 GiB, APFS local SSD, warm cache, `cargo build --release -p removarr` (thin LTO, stripped, host target, system allocator), harness in a debug test profile, fakes in the harness process on loopback. Fixture: 24,360 regular files in 8,585 dirs (1,680 hardlinks = 60 % of 2,800 completed videos, 4 subtitle files + nfo per torrent, 300 non-linked exact copies in media, 1,000 aged junk files, 400 orphans, 1,000 media-only), 3,000 torrents (2,800 + 200 incomplete) on the qBittorrent fake (6,003 requests per scan, N+1 as noted by 22), 2,000 Radarr movies (2,803 requests), 5,334 active issues, 32,943 `fs_entries`, 24 MiB apparent. Dummy secrets only, `env_clear` child.

| ID | Target | Measured | Result |
|---|---|---|---|
| B1 RSS idle | <= 40 MiB | **229 MiB** (300 s idle) / 231 MiB (60 s), 30.7 MiB right after start. On Linux/musl in the container, fs-only 20,160-file volume without services: 22.7 MiB idle after two scans | **MISS on host**; PASS in container (different fixture, no torrents) |
| B2 RSS peak, full scan | <= 160 MiB | 149-150 MiB (first scan); **239 MiB during the unchanged rescan**. Container (fs-only): 87 MiB peak | PASS first scan, **MISS rescan** |
| B3 idle CPU | <= 1 % | 0.00 % over 300 s | PASS |
| B3 zero fs syscalls idle | roots untouched | roots renamed away: idle, both service refreshes, every GET page and 20 issue details: no fs error logged, no root recreated, tree identical | PASS |
| B4 full scan | <= 60 s | 3.8 s (scan + evaluation, includes 6,003 qBittorrent + 2,803 Radarr requests). Container fs-only: 5.6 s | PASS |
| B4 unchanged rescan | <= 30 s | 3.3 s | PASS |
| B4 fs walk + persistence | <= 15 s | not separable (no stage metric exposed); whole scan 3.8 s | PASS by bound |
| B5 initial scan DB writes | <= 64 MiB | 43.2 MiB (db after `wal_checkpoint(TRUNCATE)`); 64.9 MiB if the WAL is counted too | PASS (borderline if WAL counted) |
| B5 unchanged rescan | <= 4 MiB | 0.00 MiB | PASS |
| B6 | zero fs/content reads in refresh and UI | covered by the roots-renamed-away run (B3); scan-side sample/full-compare counters cannot be observed from outside the binary | PASS (UI/refresh); scan counters: crate tests only |
| B7 dashboard p95 | <= 100 ms | **146 ms** | **MISS** |
| B7 issue list 50 rows (5.3k issues) | <= 100 ms | **118 ms** (page 40: 118, search: 120, 200 rows: 121) | **MISS** |
| B7 HTMX fragment | <= 50 ms | job progress 0.05 ms; selection summary (50 ids) **54 ms** | **MISS** (selection summary), PASS (progress) |
| B7 jobs / history | <= 100 ms | 0.11 / 0.14 ms | PASS |
| B8 binary / image | <= 30 / 250 MiB | host binary 14.2 MiB; container static musl binary 14 MiB, image 55 MiB (`docker image inspect`) | PASS |
| B9 startup to `/readyz` | <= 3 s | 0.02-0.5 s empty DB; 0.02-0.06 s over a 65 MiB DB after SIGKILL | PASS |
| B10 INFO lines per scan | <= 200 | 0 per scan (startup only: 15 lines) | PASS |
| B10 log ring | <= 2,000 | 2,000 after 200,000 events | PASS |
| B11 writes in roots | 0 | 0 entries changed (inode, size, mtime, nlink, whole tree) across scans, rescans, dry-run plan, approval and simulation; 0 mutations on qBittorrent and Radarr fakes | PASS |
| Argon2id login | (no budget) | m=19456,t=2,p=1: verify p50 20.8 ms, p95 58.8 ms; full login (2 requests) 13-22 ms | informational |

Disk I/O: macOS gives no per-process block counters through `time -l` here (0/0). In the Linux container (`/proc/1/io`, fs-only 20k files, two scans and 6 page loads): write_bytes 152 MB (db 43 MB + WAL 32 MB present at the end), read_bytes 131 MB, **rchar 4.8 GB** (cached reads; see defect 1). Cold-cache HDD numbers (handoff 06) were NOT measured: no HDD, no cold cache control here.

## Defects found (source not touched)

1. **B7 per-request cost, owner reporting (15)** `crates/reporting/src/reporter.rs`. Every dashboard and issue-list request runs `issue_cores()` (all active issues, with `evidence LIKE '%"completeness":"Unknown"%' OR evidence LIKE '%"Incomplete":%'` over every evidence blob) plus the `space()` aggregates over `fs_entries` (`GROUP BY dev, ino`, about 47 ms with the sqlite3 CLI on 33k rows). `sample` of the running binary: top self time in SQLite `patternCompare`/`strcspn`/`pread`, `sqlite3VdbeMemFromBtree`; the 6-page Linux run read 4.8 GB. Reproduce: `cargo build --release -p removarr && cargo test -p removarr-e2e-perf --test perf_budgets perf_reference -- --ignored --nocapture`. Suggested direction: store the `incomplete` flag and the potential bytes as columns at issue write time (db-layer 04 / issue-engine 11), cache the space aggregates per scan session.
2. **B1/B2 memory, owners reporting (15) / jobs (14) / issue-engine (11)**: RSS after a scan stays at ~230 MiB on macOS (system allocator) and peaks at 239 MiB during an unchanged rescan. The Linux/musl container (no torrent fake) returned to 22.7 MiB, so allocator behaviour is a likely contributor, but the peak on a rescan is not explained by the first scan (150 MiB). Needs a Linux run of the full 3,000-torrent fixture (needs the fakes reachable from the container, which loopback-only rules prevent from this harness) before a waiver can be justified.
3. **Interrupted jobs are invisible after a restart (UX gap, owner jobs 14 / web-ui 19)**: after SIGKILL the log says `interrupted=1 requeued=1` but `GET /jobs/<old id>` is 404 and `/jobs` never says "Interrupted" (job views are memory only). The re-queued scan runs and the lists converge (verified). Not a safety issue.
4. **Visual (web-ui 19), needs a real-device check**: at 320 px the sticky selection bar (`.sel-bar`) is about 480 px tall in a 640 px high viewport (see screenshots below), leaving little room for list items; the "Hide items younger than" select text is truncated at 320 px.
5. testkit `parity_preset` mismatch (already in 22): unchanged.

## Observations (not defects)

* Heartbeat: scans emit 0 INFO lines; there is no log trace of scan start/end or stage timings, so B4's "fs walk + persistence <= 15 s" cannot be read from the logs.
* `GET /scan`-style requests on command routes answer `403` (guard) rather than `405`; harmless.
* An un-aged synthetic tree hides every file as "recent" (hide_recent_days): generators must set old mtimes.
* Command floods are deduplicated (400 commands: 1 scan job, 303 x 300, 422 x 100 for empty `evaluate`).
* qBittorrent 503 outage: 4 attempts in 7 s; Radarr 503: 8 requests in 12 s; unreachable APIs at start: ready in 16 ms, scan Succeeded in 17 s.
* SIGTERM during a scan stops in 0.09-0.16 s with exit 0; SIGKILL then restart is ready in 17-56 ms over a 65 MiB DB and the second scan gives the same inventory.
* Cancelling a first scan at 150 ms gives `Cancelled`; the dashboard does not present it as complete.

## Container smoke test (Docker 27.4 in Colima, native arm64, no push)

`crates/e2e-perf/scripts/container-smoke.sh`: 29 checks pass. Build 78 s (cached 2 s), image 55 MiB, binary 14 MiB static musl; named volume with one parent (`/data`), 180 real hardlinks; uid 1000:1000, `--read-only --tmpfs /tmp --cap-drop ALL --no-new-privileges`, CapEff 0, port on `127.0.0.1` only; Docker HEALTHCHECK `healthy`; `hash-password` yields the production parameters; login page, wrong password 401, login 303, CSRF 403, dry-run scan Succeeded, 50 junk issues; tree fingerprint (`stat` of all entries) identical before and after; no secret in logs; SIGTERM exit 0. Only base images were pulled by `docker build`; no service other than the loopback container was contacted; the script removes its container, volumes and image. Colima was not running and was started by this wave (`colima start`); it is left running.

## Checks not run

* Real-browser interaction (focus order, HTMX under the CSP, tab-bar/sticky-bar overlap on a touch device, forced-colors): only static Firefox headless screenshots of server-rendered HTML with the CSS inlined (JS removed) at 320/360 px in French and English were taken (dashboard and junk list render without horizontal overflow). Markup, CSS rules and CSP are asserted by tests instead. Screenshots were in the scratchpad, not committed.
* Cold-cache HDD walk, Linux ext4 numbers for the full 3,000-torrent fixture, arm64/amd64 multi-arch, Linux `strace` content-read counters (B6 scan side).
* Plan TTL expiry with a clock (system clock only), the 5-minute CPU figure for a scheduled scan.

## Commands run and results (host macOS arm64)

* `cargo fmt --all --check`: pass. `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: see final summary line in the orchestrator report (new crate adds 20 fast tests: auth_http 6, ui_flows 8, stress 6; about 13 s).
* `cargo build --release -p removarr` then `PERF_RESULTS=<file> PERF_IDLE_SECS=300 cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1`: `perf_reference_fixture_budgets_b1_to_b11` FAILS on the misses above (by design: a miss fails the test); `argon2_verify_time_with_production_parameters` passes. Knobs: `PERF_BIN`, `PERF_WORK`, `PERF_IDLE_SECS`, `PERF_RESULTS`.
* `cargo test -p removarr-e2e-perf --test stress_heavy -- --ignored --nocapture --test-threads=1`: 2 passed (about 40 s).
* `crates/e2e-perf/scripts/container-smoke.sh`: 29 passed, 0 failed.
* Ad hoc Linux run: 20,160-file volume in the image (fs only): scan 5.6 s / 4.5 s, RSS HWM 87 MB, idle 22.7 MiB, `/proc/1/io` as above; container and volume removed.
* Nothing committed; processes started by the harness are killed on drop (two orphaned benchmark binaries from an early failed run were killed by hand).
