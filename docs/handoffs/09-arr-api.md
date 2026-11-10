# Handoff — 09-arr-api (wave 5)

## Delivered scope

`crates/arr/**` and `crates/testkit/src/fake_arr.rs` only. Domain, config, the root `Cargo.toml`, testkit
`lib.rs`/`Cargo.toml` and other crates were not edited. `crates/arr/Cargo.toml` gained dev-dependencies only
(`removarr-config` with `test-support`, `tempfile`, `tracing-subscriber`, all `workspace = true`).

| File | Contents |
|---|---|
| `src/lib.rs` | re-exports `ArrReader`, `ArrCommander`, `ArrSettings`, `ArrError`, `release_token`; `pub mod snapshot`, `pub mod correlate` |
| `src/http.rs` | `ArrSettings` (`from_config`), HTTP core: `X-Api-Key` header (marked sensitive), no redirects, no env proxy, connect 5 s / request 30 s / search 120 s timeouts, per-service semaphore (`http_concurrency_per_service`), body capped at `max_response_bytes` while streaming, read retries (3, 1/2/4 s ±20 %), status mapping. GET is the only method without a token; `send` (POST/PUT/DELETE) requires `&LiveMode` and is never retried |
| `src/read.rs` | `ArrReader` implements `ArrRead` (all 11 methods) + in-memory release cache |
| `src/commands.rs` | `ArrCommander` implements `ArrCommands` (all 6 methods) |
| `src/snapshot.rs` | `detect_changes`, `SnapshotChange`, `SnapshotValidity`, `revalidate` |
| `src/correlate.rs` | `Correlator`, `Correlation`, `import_activity`/`import_in_progress`, `queue_health` |
| `src/dto.rs`, `src/time.rs`, `src/error.rs` | v3 wire shapes (no URL/passkey fields), RFC 3339 parser, `ArrError` -> `PortError` |
| `tests/*.rs` | 42 integration tests against `fake_arr` (+ 9 unit tests in `src`) |
| `testkit/src/fake_arr.rs` | loopback fake Sonarr/Radarr, V10 preset, Radarr preset |

## Public contracts

### Construction (for `app::wiring`, agent 16)
```rust
let settings = removarr_arr::ArrSettings::from_config(&cfg, ArrService::Sonarr); // None = not configured
let reader = removarr_arr::ArrReader::new(&settings, cfg.paths.clone(), clock)?; // Arc<dyn Clock>
let read: Arc<dyn ArrRead> = Arc::new(reader.clone());
// live processes only:
let cmd: Arc<dyn ArrCommands> = Arc::new(ArrCommander::new(reader.clone(), &cfg.capabilities, &live_mode));
```
Build the commander from a clone of the **same** reader the evaluate step searches with: clones share the release
cache that `grab` needs (see "Release tokens").

### `ArrRead` semantics (`ArrReader`)
- `status`: `GET /system/status` version; refuses (`Validation`) when `appName` names the other application.
- `library`: one `ArrLibraryEntry` per file. Sonarr: series -> `episodefile` + `episode` per series (fan-out bounded
  by the semaphore); `episode_ids` = episodes whose `episodeFileId` is the file; `MediaKind::Special` for season 0;
  `season`/`episode`/`absolute` set when unambiguous; `monitored` = series monitored and any of its episodes
  monitored. Radarr: movies with a `movieFile`. Sorted by (media id, file id).
- `episodes(series, season)`: Sonarr only (Radarr -> `Validation`, no request). Every expected episode with or
  without a file, specials, `absolute`, `scene_*`; `file_id` is `None` for ARR's 0; `aired` = `airDateUtc <= clock`
  (missing or unparsable = not aired).
- `queue`: paged (250), `includeUnknown{Series,Movie}Items=true`. `status` = ARR `trackedDownloadStatus`
  (`ok`/`warning`/`error`), `tracked_state` = `trackedDownloadState`; missing = `"unknown"`. Messages are scrubbed.
- `history_for_download(hash)`: paged `GET /history?downloadId=<UPPER-CASE hash>` (ARR stores and filters qBittorrent
  ids upper case); records are re-filtered on the hash. `data` blobs are never deserialized.
- `quality_snapshot(media)`: version, profile (`id`, `name`, cutoff **name** resolved from ARR's items or groups,
  `upgradeAllowed`, `minFormatScore`, `cutoffFormatScore`, `formatItems` as name -> score), current files (Sonarr:
  files of the requested episodes, all series files when `episode_ids` is empty; Radarr: `moviefile`), media
  management, `captured_at`, sealed fingerprint. `media.arr` has sorted episode ids. `candidates` is empty: the
  caller sets it from `manual_import_preview` (candidates are excluded from the fingerprint, so no reseal is needed).
  A requested episode ARR no longer lists -> `StateChanged`.
- `manual_import_preview(folder, media)`: maps the folder to the service path (no rule -> `Validation`), `GET
  /manualimport?folder=..&filterExistingFiles=false[&seriesId|movieId]`. Each row -> `ArrCandidateDecision`:
  `subject` = `RootedPath` display (`root:rel`) of the mapped file; ARR rejection reasons verbatim (scrubbed);
  `approved` = no rejection at all. Fail-closed local checks add reasons prefixed `removarr: ` (media not identified,
  different media than requested, Sonarr row without episodes, path not mapping into a root).
- `search_releases(media)`: `GET /release` with `movieId` (Radarr), `episodeId` (one Sonarr episode) or `seriesId` +
  `seasonNumber` (several episodes of one season; anything else or empty -> `Validation`, nothing sent). Never sends a
  POST. `approved` = ARR approved **and** no rejection. `release.guid` is an opaque token (below).
- `command_status`: `queued`/`started`/`completed` (`result: unsuccessful` -> `Failed`), `failed`/`aborted`/`cancelled`/
  `orphaned` -> `Failed`, anything else -> `Unknown`.
- `media_management`: `recycleBin` ("" = `None`, else mapped through the service path mapping),
  `recycleBinCleanupDays`, `copyUsingHardlinks` (missing = `false`, pessimistic).

### `ArrCommands` semantics (`ArrCommander`, live only)
| Method | Capability | Wire |
|---|---|---|
| `rescan(media)` / `rescan_all()` | none (live mode) | `POST /command` `RescanSeries`/`RescanMovie` with or without id |
| `manual_import(req)` | `arr_manual_import` | re-reads the preview of each file's folder; refuses (`StateChanged`) unless ARR still lists the file with no rejection, the same media and (Sonarr) exactly the requested episodes; then `POST /command {"name":"ManualImport","files":[..],"importMode":"copy"}` passing ARR's own `quality`/`languages`/`releaseGroup`/`downloadId` back. The literal appears once in `crates/arr/src` |
| `grab(release)` | `arr_grab` | resolves the token from the release cache (missing -> `StateChanged`, "search again"), `POST /release {guid, indexerId}` |
| `remove_queue_item_keep_media(id)` | `arr_queue_remove` | `DELETE /queue/{id}?removeFromClient=false&blocklist=false` (literals) |
| `set_monitored(media, b)` | `arr_set_monitored` | Sonarr episodes `PUT /episode/monitor`; whole series `PUT /series/editor`; Radarr `PUT /movie/editor` |

The capability check runs before any request (a disabled call sends nothing, not even the preview). A service mismatch is
`Validation`. Returned command ids are polled with `ArrRead::command_status` by 13.

### Release tokens (INV-31)
Indexer guids are often tracker download or details URLs with a passkey. `ReleaseSearchResult.release.guid` is therefore
`release_token(indexer_id, raw_guid)` = `"rel-" + 32 hex of sha256`, stable across searches of the same release. The raw guid
lives only in the reader's in-memory cache (30 min, ARR's own release cache lifetime; max 10 000 entries). Plans and the DB
store only the token. **Consequence for 12/13:** `ArrGrabRelease` revalidation must run a fresh `search_releases` (which is
required anyway because ARR forgets releases after about 30 minutes) through a reader that shares the commander's cache, and
check the token is present (`ReleaseNotInSearch` otherwise). After a restart the cache is empty, so a grab needs a new search.

### Quality snapshot API for `issue-engine` (11) and `plan-engine` (12)
```rust
use removarr_arr::snapshot::{detect_changes, revalidate, SnapshotChange, SnapshotValidity};
let v: SnapshotValidity = revalidate(&*arr_read, &stored_snapshot).await;
v.is_unchanged()  // true ONLY for a fresh, valid, equal fingerprint
```
- `SnapshotValidity::{Unchanged, Changed(Vec<SnapshotChange>), Unavailable(PortError)}`. An outage, auth failure or any
  read error is `Unavailable`, never `Unchanged`.
- `SnapshotChange` (serde snake_case, for evidence/UI): `StoredFingerprintInvalid` (tampered stored snapshot), `NoMediaRef`,
  `ServiceChanged`, `MediaChanged`, `ProfileChanged{from,to}`, `CutoffChanged`, `UpgradeAllowedChanged`,
  `MinFormatScoreChanged`, `CutoffFormatScoreChanged`, `FormatScoreChanged{name, from: Option, to: Option}`,
  `CurrentFilesChanged{added, removed, modified}` (file ids), `MediaManagementChanged`, `FingerprintChanged` (fallback).
- `detect_changes(stored, current)` is pure and returns an empty list only for identical content with a valid stored
  fingerprint. The fingerprint itself is the domain's (`ArrQualitySnapshot::compute_fingerprint`, INV-34).

### Correlation API for `issue-engine` (11)
```rust
use removarr_arr::correlate::{Correlator, Correlation, import_activity, import_in_progress, queue_health};
let mut c = Correlator::new();
c.add_queue(&queue); c.add_history(&history); c.add_library(&library);   // every configured ARR service
c.by_hash(&hash) / c.by_path(&rooted) / c.for_content(hashes, paths) -> Correlation
```
- `Correlation::{NotFound, Unique(ArrMediaRef), Ambiguous(Vec<ArrMediaRef>), Unidentified}`. Records naming one media merge
  (episode ids united, e.g. one queue row per episode of a season pack). Different media, or identified mixed with
  unidentified records (unknown queue items), are `Ambiguous`; only unknown items is `Unidentified`. Only `Unique` may feed
  `IdentityProof::ArrMatched`.
- `by_path` matches ARR **current media files** exactly (mapped paths). Completed-side paths correlate through their torrent
  hashes (qBittorrent content path -> hashes -> `by_hash`/`for_content`).
- `import_activity(item)`: `Active` (`importPending`, `importing`), `Blocked` (`importBlocked`), `Idle`, `Unknown`.
  `import_in_progress` = `Active | Unknown` (fail-closed input for `ImportInProgress`, section 8.3).
- `queue_health(item)`: `Ok | Warning | Error | Unknown` from `trackedDownloadStatus`, for `arr_queue` severity. **It is never
  hardlink evidence** (INV-39): `arr_queue` issues must still run the inode check, then the permission check.

### `testkit::fake_arr` (for 22 and later agents)
- `FakeArr::start(ArrData)`, `FakeArr::sonarr()` (V10 preset), `FakeArr::radarr()`; `base_url()`, `with_data`, `data`,
  `set_fault(key, Fault{status, delay, raw_body, redirect_to})` (key = first path segment after `/api/v3/`, or `*`),
  `set_command_status`, `recorded()` (method, path, query, JSON body, `key_ok`), `count`, `mutating_count`, `grabs`,
  `commands`, `violations`. API key `FAKE_API_KEY` in `X-Api-Key`; anything else is 401.
- Built-in assertions: `ManualImport` with an import mode other than `copy` -> HTTP 400 + violation (6.8(d)); queue removal
  without both `false` literals -> 400 + violation; an `apikey` query parameter -> violation; grab of an unknown guid -> 404
  ("not in cache").
- V10 behaviors: `set_import_destination(src, dest)` (service paths), `set_fs_bridge(FsBridge{service_prefix, local_dir})`
  enables filesystem effects; `ImportEffect::Copy` hardlinks when `copyUsingHardlinks` is on, else copies;
  `ImportEffect::Move` is the "move switch" (source disappears). The upgrade retires old files first: moved into the
  recycle bin when `recycleBin` is set, removed otherwise; ARR state (episode files, `episodeFileId`) is updated.
  `media_management(recycle_bin, cleanup_days, hardlinks)` builds presets.
- V10 data: series 1 (anime) S01E01 (file, abs 1), S01E02 (no file), S01E03 (file, abs 3, scene S02E01 / abs 4), S01E04
  (airs 2099), S00E01 special (file, unmonitored); series 2 (no file); profile with CF-Alpha 10, CF-Beta 50,
  CF-Penalty -100, cutoff group `Group-HD`; preview folder `V10_FOLDER` (E01, E02 accepted; E03 "Not an upgrade");
  queue: `HASH_SEASON` (2 rows, same series, `importPending` warning), `HASH_AMBIGUOUS` (series 1 and 2),
  `HASH_UNKNOWN` (no media, error); history with a planted passkey in `data`; releases: one approved, one rejected whose
  guid is a URL with `PLANTED_PASSKEY`. The three old-file recovery cases (`OtherLink`, recycle bin, unrecoverable) are
  produced by 22's filesystem tree plus these presets (recycle bin on/off, a still-linked completed copy or not).

## Rules enforced (and where tested)

- No local quality hierarchy: the crate copies ARR's names, scores and verdicts; nothing compares qualities, codecs, groups
  or resolutions (INV-11). Cutoff resolution only looks up the name of ARR's cutoff id.
- No outage becomes approval: snapshot `Unavailable` (`snapshot.rs::outage_or_auth_failure_is_never_unchanged`); preview rows
  without media/episodes or unmapped are never approved (`read_sonarr.rs`); an approved release with a rejection is not
  approved (`search.rs`); unknown queue states count as import in progress; missing hardlink setting = disabled.
- Search never grabs: GET only, asserted by recorded requests and `grabs()` (`search.rs`, `commands.rs::grab_needs_...`).
  Structural: `Http::send` needs a `LiveMode`, which `ArrReader` never holds.
- Rescan/command status are separate from searches (commander vs reader; `rescans_need_no_capability_...`).
- Manual import and queue removal are separately authorized (`every_gated_mutator_is_refused_...`).
- Score/profile changes invalidate (`snapshot.rs`: CF score, cutoff, upgrade flag, min score, removed CF, profile
  reassignment, current file, media management, tampered stored snapshot).
- Queue ambiguity (`read_sonarr.rs::queue_correlation_merges_detects_ambiguity_and_unknown_items`, `correlate` unit tests).
- `importMode` copy on the wire (`commands.rs::manual_import_sends_copy_mode_and_arr_objects`), fake refuses others.
- Redaction: every error kind (5xx, 404, 400, 401, 403, bad JSON echoing the key, redirect with the key in `Location`,
  timeout, oversize, connection refused) has no key, host, port or query (`redaction.rs`); `Debug` of reader, commander and
  settings hides the key; ARR-echoed keys in queue messages are scrubbed; captured TRACE logs of reads and mutations contain
  no key, no planted passkey and no base URL (`logs.rs`).
- Mutations never retried, reads retried 3 times (`commands.rs::mutating_calls_are_never_retried_but_reads_are`).

## Deviations from the architecture

1. **Correlation and snapshot-diff placement.** Section 2.2 forbids `workflows` from depending on `removarr-arr`, so 11/12
   cannot call `removarr_arr::correlate`/`snapshot` directly. Both modules depend only on domain types and the `ArrRead` port
   and contain no I/O. Options for the orchestrator: (a) hoist them verbatim into `removarr_domain::arr` in a targeted 03
   patch between waves (recommended; no behavior change), or (b) 11/12 re-implement the documented rules. Until then, the
   plain fingerprint comparison in domain is sufficient for invalidation; the diff is explanatory.
2. **Search timeout 120 s** instead of 30 s for `GET /release` only: interactive searches query every indexer and
   routinely exceed 30 s. All other ARR calls use 30 s.
3. **`ReleaseRef.guid` holds an opaque token**, not ARR's raw guid (INV-31). The raw guid exists only in memory. Impact on
   12/13 described under "Release tokens".
4. `ArrQueueItem.status` carries `trackedDownloadStatus` (health) and `tracked_state` `trackedDownloadState`; ARR's coarse
   queue `status` field is not kept (it duplicates the tracked state).
5. `ArrFileRef.cf_score` is 0 when ARR does not report a file score (Sonarr before v4). Decisions must use ARR's preview
   verdicts, never this number.
6. 401 maps to `ErrorClass::Unauthorized` and 403 to `Forbidden` (same as the Jellyfin connector), so 11 can set
   `ServiceState::AuthRejected`; 3xx is `Rejected` (redirects are never followed because they would carry the key).

## Open TODOs

- Orchestrator: decide deviation 1 (hoist `correlate`/`snapshot` into domain).
- 16 (`app`): configure the tracing filter so `hyper`/`hyper_util`/`reqwest` stay below DEBUG; at TRACE they log
  `host:port` (never the key, which is a sensitive header value).
- 13: poll `command_status` with a bounded timeout after `manual_import`/`rescan`; grab revalidation as above.
- 12: put `manual_import_preview` decisions into `snapshot.candidates`; build `CoverageProof` from `episodes()` and the
  decisions' `episode_ids`; `ArrHardlinksDisabled` from `media_management.hardlinks_enabled`.
- Real-ARR field names were taken from the public v3 API (Sonarr 3/4, Radarr 4/5). Assumed shapes, encoded in `fake_arr`:
  preview rejections `[{reason, type}]` and release rejections `[string]` (both shapes accepted for each), file
  `customFormatScore` optional, `copyUsingHardlinks`/`recycleBin`/`recycleBinCleanupDays` in `config/mediamanagement`,
  command `status` + optional `result`.

## Commands run and results (host target, rustc 1.98)

- `cargo build -p removarr-arr`: pass.
- `cargo clippy -p removarr-arr --all-targets -- -D warnings`: pass. `cargo clippy -p removarr-testkit --all-targets -- -D warnings`: pass.
- `cargo test -p removarr-arr`: pass, 51 tests (9 unit; integration: commands 11, logs 1, read_radarr 2,
  read_sonarr 10, redaction 3, search 5, snapshot 10).
- `cargo fmt --all --check`: pass (I formatted only my files; an early `cargo fmt -p removarr-testkit` rewrote only
  `fake_arr.rs`, verified by modification times).
- `cargo clippy --workspace --all-targets -- -D warnings`: pass. `cargo test --workspace`: pass (all binaries ok) at the time
  of the run, with the other wave-5 agents' work in progress.
- Gates: `importMode` appears once in `crates/arr/src` with `"copy"`; no `move`/`auto` word in `crates/arr/src`; no
  `LiveMode::` in `crates/arr/src`; no `apikey` query; domain and workflows dependency greps print nothing.
- No network beyond loopback and crates.io; no real services or secrets; nothing committed.

Note: the pure `snapshot` and `correlate` modules now live in `removarr_domain::arr` (re-exported here); see the "Patch: ARR pure rules" section of `docs/handoffs/03-domain-model.md`.
