# Handoff — 10-jellyfin-api (wave 5)

## Delivered scope

`crates/jellyfin` (`removarr-jellyfin`) and `crates/testkit/src/fake_jellyfin.rs`. Only `crates/jellyfin/Cargo.toml`
changed in manifests (dev-deps: `removarr-config` with `test-support`, `tempfile`). Root manifest, domain and other
crates untouched. (`cargo fmt -p removarr-testkit` may have reformatted other agents' testkit files; check the diff.)

| Item | Role |
|---|---|
| `JellyfinReader` | implements `JellyfinRead` (GET only). Inherent: `health()`, `lookup(paths) -> ItemLookup` (detail incl. duplicates), `from_config(&Config)` |
| `JellyfinCommander` | implements `JellyfinCommands`. `new(url, key, settings, &LiveMode)`, `from_config(&Config, &LiveMode)`. Separate type from the reader |
| `wait_for_library_refresh(&dyn JellyfinRead, PollConfig) -> RefreshPoll` | bounded polling of the library scan |
| `recognition::{ExpectedMedia, evaluate, RecognitionReport, RecognitionStatus, Finding, completeness_of}` | pure mismatch/special/alternate-numbering reporting |
| `HttpSettings` | connect 5 s, request 20 s, `max_response_bytes` (from `cfg.limits`), 3 read retries (1/2/4 s +-20 %), none for POST |
| `testkit::fake_jellyfin::FakeJellyfin` | loopback fake (127.0.0.1:0), request recorder, `Fault {status, delay, raw_body}` per endpoint, fixtures `common_library()`, `ambiguous_library()`, `common_libraries()`, `FAKE_TOKEN` |

## Read contract (for 11 workflows, 12 issue, 13 actions, 17 reporting)

- Wire: token only in header `X-Emby-Token` (never query). Endpoints: `GET /health`, `GET /System/Info` (version, proves the
  token), `GET /Library/VirtualFolders`, `GET /Items?Recursive&IncludeItemTypes=Movie,Episode,Video&Fields=Path,ProviderIds`
  (paged 500, bounded 2000 pages), `GET /ScheduledTasks?isHidden=false`.
- `status()` = server version from `/System/Info`. `health()` is inherent on `JellyfinReader` (the trait has no health call).
- `libraries()` locations mapped with `cfg.paths.to_app(ServiceKind::Jellyfin, ..)` (`MappedPath`, mapping errors preserved).
- `items_by_paths(paths)`: every requested path is a key. `Some(item)` only when exactly one item has that path.
  `None` means "no item" **or** "ambiguous duplicate"; use `JellyfinReader::lookup` (or the recognition report) to tell apart.
  A requested path with no Jellyfin path-mapping rule returns `Err(ErrorClass::Config)`, never `None` (a mapping gap is not
  "unrecognized"). Items with an unmappable path are counted in `ItemLookup.unmapped`, never listed (paths are sensitive).
- Item identity: `JellyfinItemRef.kind` is the Jellyfin type (`Movie`, `Episode`, `Video`; `Video` = indexed but not
  recognized). `season` = `ParentIndexNumber`, `episode` = `IndexNumber` for episodes only. `JellyfinItemDetail` adds `series_id`
  and `episode_end` (`IndexNumberEnd`, multi-episode files).
- `library_refresh_running()`: scheduled task key `RefreshLibrary`; `Running`/`Cancelling` = true; task not listed = `Err(NotFound)`
  (never "idle").
- Item refresh has no status API (architecture C25): verify with `items_by_paths`, do not poll.
- Polling: `wait_for_library_refresh` returns `Completed` (seen running, then idle), `CompletedUnobserved` (never seen running
  within `start_grace`; weaker), `TimedOut` (verification **incomplete**, not failed). A failing poll returns `Err`.
  Defaults: timeout 120 s, interval 2 s, grace 10 s; 13 should derive the timeout from `recheck_timeout_seconds`.
- Recognition: build `ExpectedMedia { identity: MediaIdentity, alternate_numbering }` from ARR (`ArrEpisode.scene_*` for
  alternates, `MediaIdentity.absolute` for absolute numbering), then `evaluate(&expected, &lookup.matches[path])`.
  Status: `Recognized` | `Ambiguous` (duplicates, alternate or absolute numbering, title-only differences) | `Mismatch`
  (wrong numbering/kind, unrecognized `Video`, missing numbering, special in a regular season or the reverse, film provider id
  conflict) | `NotFound`. A recognized multi-episode file carries the informational `MultiEpisodeFile`. Provider ids are compared
  for films only (episode items carry episode ids, ARR identities carry the series id).
- Only `Recognized` should satisfy `VerificationStep::JellyfinRecognized`. `completeness_of(&Result)` gives
  `Incomplete` for any API error: an API failure makes dependent verification incomplete, never successful. Jellyfin never
  influences quality or release decisions (no such API exists in this crate).

## Refresh contract (for 13 actions, 22 integration)

- `JellyfinCommander::refresh_library()`: `POST /Library/Refresh`, success on any 2xx (Jellyfin: 204). Asynchronous: poll with
  `wait_for_library_refresh` on a **reader**, then re-check with `items_by_paths`.
- `refresh_item(id)`: `POST /Items/{id}/Refresh?Recursive=true&MetadataRefreshMode=Default&ImageRefreshMode=Default&ReplaceAllMetadata=false&ReplaceAllImages=false`
  (literals, same as Jellyfin's "Scan for new and updated files"; no `FullRefresh`, no replace). `id` must be alphanumeric/dash,
  1..=64 chars, else `Err(Validation)` without any request.
- Constructors need `&LiveMode`; there is no read-only-process way to build one. Commands are never retried (REJ-21). No capability
  flag applies (`JellyfinRefresh` has none, architecture 13). The `Recording*` dry-run implementation of `JellyfinCommands` is 13's.
- Error classes: 401 Unauthorized, 403 Forbidden, 404 NotFound, 429 RateLimited, 408/5xx/timeout/connect Unavailable,
  other non-2xx and malformed/oversized bodies Rejected. Error text holds only `jellyfin <METHOD /path>: <reason>`: no host,
  URL, token, headers or upstream body. `Debug` of reader/commander prints no host or token.

## Deviations from the architecture

1. The client uses `no_proxy()` and `redirect(Policy::none())` (the token must not follow redirects; Jellyfin is a local service).
2. `items_by_paths` lists all Movie/Episode/Video items and matches locally (no server-side `path` filter: not available on
   all 10.x versions). Cost is O(library) per call; 11 should batch all paths of a session into one call.
3. Duplicate items at one path yield `None` from the trait (see Read contract).
4. A task-not-listed answer in `library_refresh_running` is an error rather than `false`.
5. Jellyfin can itself write `.nfo`/images next to media when "save metadata into media folders" is enabled; refresh asks it to.
   removarr never touches files. Operators who must keep library folders immutable should disable that Jellyfin option
   (to be documented in the user guide).

## Open TODOs

- Series/season level folder identities are not listed (files only; series name and numbering come from the episode item).
- Jellyfin metadata lock is out of scope (C14).
- `app::wiring` (16) must build `JellyfinReader::from_config` always and `JellyfinCommander::from_config(&cfg, &live)` only live.
- Workspace clippy `-D warnings` currently fails in `crates/testkit/src/fixture.rs` (`ParityFixture.tmp` never read, dead_code),
  owned by 06; the jellyfin crate and `fake_jellyfin.rs` themselves are clean.

## Commands run and results

- `cargo test -p removarr-jellyfin`: pass (22 tests: health, 204 refresh, timeouts and retry policy, connection refused, malformed
  bodies x6, oversize, status mapping, token/host redaction, paging, films, episodes, specials, multi-episode, ambiguous
  numbering, duplicates, unmapped, polling completed/timeout/unobserved/error).
- `cargo fmt -p removarr-jellyfin --check`: pass. `cargo fmt --all --check`: fails only in other agents' `crates/fs` file.
- `cargo clippy -p removarr-jellyfin --lib -- -D warnings`: pass. `--all-targets`: no warnings in jellyfin; blocked from `-D` by the
  testkit `fixture.rs` dead-code error above.
- Domain dependency gate: prints nothing.
