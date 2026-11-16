# Handoff — 22-integration-tests (wave 15)

## Delivered scope

New crate `crates/integration-tests` (`removarr-integration-tests`, tests only; root `Cargo.toml` needed **no change**, `members = ["crates/*"]`
picks it up; `Cargo.lock` gained the crate's edges). No `src/**` of any feature crate and no testkit file was edited. `docs/parity-matrix.md`:
rows T-01, T-02, T-08, T-13, T-16, T-17, T-22, T-23, T-30, T-31, T-32 extended (T-17, T-31, T-32 now covered end to end) plus a summary line.

Harness (`crates/integration-tests/src/lib.rs`): `Stack` builds the **production composition root** (`removarr::wiring::build`, `runtime.start()`) from an
explicit environment map (dummy credentials, nothing inherited) over the synthetic parity tree (`testkit::ParityFixture`, real hardlinks, one temp
dir), a real SQLite file in `<fixture>/appdata`, and the loopback fakes (`FakeQbit`, `FakeArr`, `FakeJellyfin`, all `127.0.0.1:0`). It exposes
jobs, issues (through the real read model), plans (through the real `PlanControl`), the persisted journal, per-action outcomes, plan status,
tree snapshots (`world`) and `restart`/`restart_with` (new process state over the same SQLite). `Lab` runs the fakes and the application on separate
runtimes so a crash is real: `shutdown_background` kills the application tasks mid-await (no shutdown path), then a fresh composition root boots.
`zulu` is the ARR replacement scenario (Radarr fake with a filesystem bridge, stand-in `ffprobe` shell script).

## Scenario list (all green unless marked)

| File | Scenarios |
|---|---|
| `parity_fixture.rs` (task 0) | Full 7.5 inventory from the stored SQLite inventories (21/16/2/8, 636288/491520 bytes, 20/8 inodes, shared inodes A/F1/K, no scan error, symlink FX-24 not a video), the 13 hardlink defects with their outcomes, the 3 duplicates, orphans, junk (3, not the NFOs or samples), media-only (5), symlinks (3), permission (FX-33), protection categories, T_Q unmapped, T_P missing, incomplete (stalled + orphan), no `outside/` file name in any database file, tree and request log unchanged by scanning |
| `pipelines_fs.rs` (task 3) | Consolidation (direction, inode, no temp file, journal order, outcome, rescan), file shared by two torrents (only the active one resumed), recheck timeout (30 s, left stopped, `PartiallyExecuted`), junk deletion with empty-parent removal, task removal (`deleteFiles=false`), partial torrent left stopped, stalled cleanup order (stop, remove task, delete data), fail-closed without an external prefix |
| `pipelines_arr.rs` (task 3) | Unrecoverable old file needs the acknowledgement then hardlinked import (`importMode: "copy"`), recycle-bin case, other-link case, "move" detected (`FailedNeedsReview`, torrent not resumed), hardlinks disabled (`ExecutedUnverified`), ARR profile change invalidates before any import, two plans for one media import once, queue removal (ignored, defect 1; fail-closed behavior pinned), grab (ignored, gap 2; search read-only pinned) |
| `services.rs` | Refresh jobs only read, Jellyfin rescan needs plan + approval and sends one refresh, Radarr rescan is one command without `importMode`, rescans cannot even be planned in a dry run |
| `dry_run.rs` (task 4) | Scan, four plan kinds, approval, live execute refused, simulate over the real router/runtime: tree identical, zero mutations on qBittorrent/Radarr/Sonarr/Jellyfin by request log, journal all `Simulated`, status unchanged; simulation still revalidates the real tree |
| `crash.rs` (task 5) | Crash with the stop in flight, with the recheck in flight (after replacement), with the start in flight, and during task removal after the file deletion; restart over the same SQLite: plan `Interrupted`, unresolved intents resolved (`Skipped`), no unresolved intent, replacement never rolled back, torrent resumed, no `deleteFiles=true`, idempotent second restart |
| `stale_plans.rs` (task 6) | Filesystem rewrite after approval, newer scan before approval, torrent state change, qBittorrent 503, new holder torrent, mapping drift across a restart: nothing mutated, plan `Invalidated`, no mutating journal step; ARR profile change is in `pipelines_arr.rs` |
| `jobs.rs` + `auto_cycle.rs` (task 7) | Execution waits for a scan that shares its root, cancel queued/running, same-plan dedupe, interrupted scan re-queued after a crash; automatic cycle end to end (scheduled scan, auto mode on): only the proven copy is consolidated, nothing else changes, auto plans `auto_policy`; incomplete scan pauses, pause persists across restart, only `POST /auto/resume` clears; dry-run auto cycle simulates only. Conflicting media imports: `pipelines_arr.rs` |
| `variants.rs` | V2, V3, V4, V5, V7, V8 (4.x `pause`/`resume`, `pausedUP` accepted) |
| `persistence.rs` (task 1) | Fresh DB migrated, restart idempotent and byte-equal (issues, plan, hash, approval), database from a newer build refused at startup |
| `backend_routes.rs` + `auth_listener.rs` (task 8) | Whole user flow over HTTP forms with persisted assertions, no host path or secret in 13+ pages and exports, ignore/restore, malformed requests; over a real listener with the production guard: unauthenticated access, CSRF, origin, login/logout, brute force with `Retry-After` and spoofed `X-Forwarded-For` ignored |

The automatic cycle tests are real time (the schedule has minute granularity and `scan.interval_minutes >= 60`, so the anchor is set to the next
minute): about 30 to 60 s each; run in parallel. The recheck timeout test is 30 s (config minimum). Whole crate about 65 s.

## Defects and gaps found (source not touched)

1. **action-runner (13), `crates/workflows/src/actions/arrcmd.rs::queue_item_proof`**: it requires every `QbitFile::candidates` path of every wanted file to
   share an inode with ARR's file. The candidates include the `<name>.!qB` variant, which does not exist for a completed file, so a proven stale queue
   entry (the issue engine accepts it, the planner plans it) is always refused with `QueueItemHidesError: ...mkv.!qB does not share an inode`. Safe, but queue
   removal can never succeed. Reproduce: `cargo test -p removarr-integration-tests --test pipelines_arr -- --ignored a_proven_stale_queue_entry`.
   The pinned safe behavior is `today_the_queue_removal_is_refused_by_the_runner_without_any_mutation`. Fix: stat the first existing candidate (as `issues/arrqueue.rs` does).
2. **backend-api (16) / job-runner (14)**: a grab cannot be planned in the production wiring. `PlannerControl::arr_observations` passes no observation for
   `PlanRequest::Release` and no component keeps search results (`EvaluateMedia` reports counts only), so every searched release gives `ReleaseNotInSearch`
   (documented as not built in 16's deviation 4; there is no route either). Reproduce: `... --test pipelines_arr -- --ignored a_search_never_grabs`. Fail closed;
   the runner side is covered by `actions_tier4.rs`.
3. **testkit (22 owns it, left unchanged)**: `fake_qbit::parity_preset` uses 1000-byte file sizes and names that do not match the tree for T_F, T_C and T_K
   (`...GRPE01.mkv`, `...-sample.mkv`), so it classifies Foxtrot/Charlie/Kilo files as unheld. The harness uses its own exact `parity_torrents`. The testkit
   preset could be replaced by it.

## Observations (not defects)

* An out-of-scope torrent (T_Q, `/srv/other`) makes the qBittorrent service state `MappingErrors` and blocks **every** plan (`ServiceUnavailable(Qbittorrent)`,
  `UnmappedTorrents`) until it is declared with `REMOVARR_QBIT_EXTERNAL_PREFIXES`. Correct fail-closed (CON-07), but surprising for the base fixture of 7.7; the
  pipeline tests declare the prefix.
* 7.7 assumes `pausedUP` allowlisted next to `v5.0.4`; Rust treats `pausedUP` as a 4.x name, so on a 5.x server T_B2 is protected and the Bravo consolidation is
  blocked (`TorrentProtectedState`); on 4.x (V8) it works. Asserted both ways.
* The DB stores the literal target text of symlink FX-22 (`../../outside/notes.txt`, display only); the files of `outside/` are never visited and their names are absent.
* Plan staleness at approval is judged on stored state (a newer scan, ARR fingerprint, configuration, expiry); the live tree is re-checked by the runner at execution.
* After a partial-torrent deletion the journal records `QbitStart ... Done` although no start request is sent (empty set); harmless but noisy.
* Qualified V2 expectation: a manual plan elsewhere in the root stays possible with an unreadable subtree (scope is the subtree); automatic mode pauses.
* qBittorrent observation issues one `torrents/info?hashes=` per torrent plus `files` (N+1 requests per scan) visible in the fake's log; not asserted.

## Deviations / not covered

* Crash boundaries between filesystem steps (intent of `FsReplaceWithHardlink` before the rename) cannot be hooked through a loopback fake; they stay covered
  by `workflows/tests/actions_crash.rs` (port-level injection). Reachable boundaries (qBittorrent request in flight after earlier mutations) are covered here.
* Expiry of a plan by TTL needs a fake clock; the application uses the system clock (`plan_ttl_hours >= 1`), not covered here.
* Sonarr-specific flows (season packs, specials, absolute numbering) are covered by 12/13 tests; integration uses Radarr and the Sonarr fake only for the zero-mutation guards.
* Real `ffprobe`, a real second filesystem (mount boundary), real browsers: not in scope (fakes and a shell stand-in for `ffprobe`).
* Tests are not serialized: each has its own tempdir, port and database. No shared resource constraint exists.

## Commands run and results (host target, macOS)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: 1016 passed, 0 failed, 3 ignored (the opt-in real `ffprobe` test and the two defect tests above).
* `cargo test -p removarr-integration-tests`: 55 passed, 2 ignored, about 65 s (dominated by the automatic cycle and the recheck timeout).
* Nothing committed.
