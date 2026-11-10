# Handoff — 08-qbit-api (wave 5)

## Delivered scope

`crates/qbit` (all of it) and `crates/testkit/src/fake_qbit.rs`. No other file touched (qbit `Cargo.toml` gained dev-dependencies
`removarr-config` with `test-support` and `tempfile`; root manifest and `Cargo.lock` unchanged). Dependencies of the crate are unchanged
(domain, config, tokio, reqwest, url, serde, serde_json, thiserror, tracing, async-trait).

## Public contract (`removarr_qbit`)

| Item | Contract |
|---|---|
| `QbitSettings::new(url, username, Secret, PathMapper)` / `::from_config(&Config) -> Option<Self>` | Defaults: connect 5 s, request 20 s, 32 MiB (`limits.max_response_bytes`), concurrency `limits.http_concurrency_per_service`, read retries 3 (1/2/4 s +-20 %), login cooldown 300 s. `None` = not configured. |
| `QbitReader::new(settings)` / `::from_config(&Config) -> Option<Result<Self, PortError>>` | `None` means qBittorrent not configured: map to `ServiceNotConfigured(Qbittorrent)`, never to a transport error. Cheap `Clone`, clones share session, cookie and version cache. |
| `impl QbitRead for QbitReader` | `version()` logs in lazily and always re-reads `app/version`. `torrents()` = one `torrents/info`. `files(hash)` re-reads that torrent's record first, so candidates use current paths. |
| `QbitReader::snapshot(taken_at) -> TorrentSnapshot` | version + one listing + `torrents/files` for every torrent with bounded concurrency, all against that listing. Cancel-safe. Prefer this over N `files()` calls (11). |
| `QbitReader::mapping_uncertain(&hash) -> bool` | true if the last listing or file query had `NoRule`/`Invalid` on any path of that torrent. |
| `QbitController::new(&QbitReader, &LiveMode)` | The only constructor; shares the reader session. `impl QbitControl`. |
| `QbitController::recheck_and_wait(&hashes, &RecheckWait{timeout, interval}, &CancelFlag) -> Result<BTreeMap<hash, RecheckStatus>, PortError>` | Sends recheck, polls `torrents/info` immediately and then every `interval`, applies `evaluate_recheck`. Never starts or removes. `Err(Cancelled)` on cancel (hashes stay stopped), `Err(Unavailable)` on transport failure, a hash missing from the list is `Failed`. Use `thresholds.recheck_timeout_seconds` and 2 s. |
| `PathIndex::build(&TorrentSnapshot)` | `hashes_for(&RootedPath)`, `hashes_under(&dir)`, `hashes_for_all(paths)`, `is_unreferenced(&path)`. Every hash sharing a path is kept. |
| `classify(&TorrentInfo, major) -> Eligibility` | `Eligible` or `Protected(Protection::{NotComplete, State, StateNotValidForVersion, MappingUncertain})`. |
| `decide_removal(&files, authorized, exists: Fn(&RootedPath)->Option<bool>) -> RemovalDecision` | `Remove` only if authorized and every wanted file (priority > 0) is absent at every candidate. Else `Keep(NotAuthorized \| NoFilesListed \| WantedFilePresent{index} \| WantedFileUncertain{index})`. |
| `plan_restore(initial, modified, recheck) -> RestorePlan{start, left_stopped}` | Start only if active initially and (unmodified or recheck `Verified`). Everything else is `left_stopped` with `InitiallyStopped \| NotRechecked \| RecheckTimedOut \| RecheckFailed \| UnknownInitialState`. |

Errors are `PortError` with: `Unauthorized` (bad credentials, ban, 401/403, unconfirmed login), `Unavailable` (connect, timeout, 5xx, 408/429,
truncated body), `Rejected` (other 4xx, oversize body, invalid JSON or hash, unsupported/garbage version, mapping-uncertain mutation),
`NotFound` (404, torrent absent), `Cancelled`. `detail` is already redacted.

## Behavior and compatibility assumptions

- Login: `POST /api/v2/auth/login` form `username`/`password`, `Referer` = base URL. Success is 204, or 200 with body `Ok.` or empty.
  200 `Fails.`, 200 with any other body, 403 -> `Unauthorized` (unconfirmed login counts as failure). A rejected login starts a cooldown
  (default 300 s) during which no further login is sent (qBittorrent bans IPs after repeated failures). No SID cookie is required.
- Authenticated calls: 401/403 -> one re-login and one repeat (nothing executed in that case); second 403 -> `Unauthorized`.
- Version: `app/version` text like `v5.0.4`, `4.6.7`, `v4.6.0beta1`; anything else (HTML, banner, > 64 chars) is `Rejected`.
  Mutation drives only major 4 (`torrents/pause`, `resume`) and 5 (`torrents/stop`, `start`); recheck and delete are the same on both.
  Other majors still read, but every mutation is refused. A 404 on stop/start re-discovers the version once and repeats if the major changed.
- Reads retry only on `Unavailable` (policy above). **Mutations are never retried**; a transport error returns `Unavailable` with
  "outcome unknown, not retried": callers must re-read the torrent list.
- Wire: hashes are joined with `|`, sorted. The remove endpoint always carries the form field with the literal false; `deleteFiles` occurs
  once in `crates/qbit/src` (gate grep passes). No public API accepts a boolean for it. Empty hash sets make no call.
- Response bodies are streamed with a counting limit (and `Content-Length` precheck): exceeding `max_response_bytes` -> `Rejected`.
  Redirects are not followed, env proxies are ignored (`no_proxy`).
- `torrents/info` fields used: `hash, name, state, progress, added_on, completion_on (<= 0 -> None), category, size, save_path, download_path
  (>= 4.4, empty -> None), content_path`. A missing or empty `save_path`/`content_path` becomes `MappedPath::Invalid` (fail closed).
  An unparsable hash fails the whole listing (`Rejected`) instead of dropping the torrent.
- File candidates (DR-23): `join(save_path, name)`, `join(download_path, name)`, `join(dirname(content_path), name)`, each followed by its
  `.!qB` variant, deduplicated, each through `PathMapper::to_app(Qbittorrent, ..)`. A name containing `..` yields `Invalid` (mapping escape).
  Matching never uses torrent names.
- Mapping uncertainty: after a listing/file query, any torrent with `NoRule`/`Invalid` on `save_path`, `content_path`, `download_path` or a file
  candidate is "uncertain". `stop/start/recheck/remove` on a set containing it is `Rejected` before any HTTP mutation. Torrents outside roots
  (`OutsideRoots`, `DeclaredExternal`) are not uncertain. Authentication uncertainty: every mutation first needs a successful login and version
  discovery; a rejected login is sticky for the cooldown.
- `PathIndex` ownership: every `Mapped` file candidate and `content_path`; for torrents with `progress < 1` also the content directory
  (multi-file torrents only, both save and download forms: everything below is owned) and the direct `.parts` files in its parent, or, for
  single-file torrents, in the directories of the file candidates. `hashes_under(dir)` also reports any entry below `dir`.
- Classification is version-aware and stricter than `domain::destructive_eligible`: `pausedUP` is eligible only on 4.x, `stoppedUP` only on 5.x,
  unknown majors protect everything. `stalledUP`, `stoppedUP`, `missingFiles` remain distinct. Mapping errors protect too.
- `domain::was_active` treats `error`, `missingFiles` and unknown states as active (it only tests for `paused`/`stopped` prefixes).
  `plan_restore` therefore would start an unmodified `error` torrent that was running before; 13 should not stop such torrents in the first place.
- A recheck that completes between two polls (no `checking*` state ever observed) is `TimedOut`, never `Verified` (domain rule, CON-12). The
  first poll is immediate to minimize that window.

## testkit `fake_qbit`

`FakeQbit::spawn(version)` (loopback `127.0.0.1:0`, aborted on drop), `url()`, `set_version`, `set_login(LoginMode::{Ok200, Empty200, NoContent204,
Fails200, Forbidden403, Garbage200})`, `set_torrents`, `add_torrent`, `set_delay`, `set_oversize`, `force_status`, `force_response`,
`abort_response` (truncated transfer), `expire_sessions`, `set_recheck(RecheckScript{checking_polls, result})`, `requests()`, `requests_to(suffix)`,
`mutation_count()`, `states()`, `has_torrent()`. Credentials: `FAKE_USERNAME`, `FAKE_PASSWORD`. `pause/resume` exist only for major < 5 and
`stop/start` only for >= 5 (the other answers 404), as in the real servers. `parity_preset(now_secs, v4_names)` builds T_A..T_O (13 torrents,
`/data/...` paths, hashes `parity_hash('1')` ..), `parity_escape_torrent` (V4, T_M, hash `e...`), `parity_download_path_torrent` (V9, T_R, hash
`f...`). Use mapping rules `/data/completed`, `/data/incomplete`, `/data/media` -> the test roots.

## Deviations from the architecture

1. `QbitReader::from_config` returns `Option<Result<..>>` (None = not configured) instead of a dedicated error type, so workflows can map it to
   `ServiceNotConfigured` without a new domain error class.
2. Extra public API beyond the two ports: `QbitReader::snapshot`, `mapping_uncertain`, `QbitController::recheck_and_wait`, `PathIndex`,
   `classify`, `decide_removal`, `plan_restore`. `recheck_and_wait` uses `tokio::time` and `Instant` directly, not the injected `Clock`.
3. The read retry jitter derives from the system clock sub-second nanos (no randomness dependency).
4. `PathIndex` is built from a `TorrentSnapshot` only (no mapper needed); the incomplete content-directory rule is derived from the file candidates.

## Open TODOs / notes for other agents

- 13 (actions): call `decide_removal` with a presence predicate from `FsInventory::stat`; call `plan_restore` and `recheck_and_wait`; journal
  `QbitStop`/`QbitRecheck`/`QbitStart`/`QbitRemoveKeepData` around the calls. Re-list torrents after any `Unavailable` from a mutation.
- 11/12 (observe, issues): use `snapshot()`, `PathIndex::build`, `classify(info, snapshot.major)`, and `QbitReader::mapping_uncertain` or the
  `MappedPath` variants for `unmapped_torrent` / mapping-error issues.
- 16 (app wiring): `QbitReader::from_config(&cfg)`, then `QbitController::new(&reader, &live)` only when `cfg.live_mode()` is `Some`.
  The "only constructible with LiveMode" property is by signature (private-field token); there is no compile-fail test (no trybuild available).
- Not covered: tracker, magnet, category or tag endpoints (not modeled, INV-31); `hashes=all`; qBittorrent behind a reverse-proxy login page
  (rejected as unconfirmed login).

## Commands run and results (host target)

- `cargo fmt -p removarr-qbit -p removarr-testkit --check`: pass.
- `cargo clippy -p removarr-qbit -p removarr-testkit --all-targets -- -D warnings`: pass.
- `cargo test -p removarr-qbit`: pass, 30 integration tests against the loopback fake (`tests/adapter.rs`: 4.x/5.x login and version, 204/empty/`Fails.`/403/garbage
  login and cooldown, session expiry, mapping (NoRule, escape, file-name escape), candidates and `.!qB`, snapshot, pause/resume vs stop/start,
  404 upgrade rediscovery, unsupported version, delete body contains `deleteFiles=false` exactly once, read retry, read/mutation timeout, interrupted
  transfer, oversize, error redaction, connection refused, recheck verified/failed/timed out/never-observed/cancelled/vanished, index for shared
  hashes and working files, `from_config`) and 7 pure tests (`tests/rules.rs`: allowlist over 24 state strings x majors 3-6, protections, removal
  decision, restore plan). Repeated 3 times, stable (about 0.3 s each). testkit unit tests: 2 new (percent decoding, preset size).
- `cargo test --workspace`: pass at the time of the run (other wave-5 crates compiled).
- Gates: `grep -rn deleteFiles crates/qbit/src` -> one line (the literal false); `grep -rn 'LiveMode::' crates/qbit/src` -> nothing;
  `cargo tree -p removarr-qbit -e normal` shows only domain and config as internal dependencies.
- Legacy parity routed here: T-25, T-26 (qbit part: escaping path), T-27 (4.x pause), T-28 (204 login), T-29 (API failure wrapped), T-30 and T-32
  (qbit half: no mutation without live token, delete with keep-data flag). 21 owns the matrix rows.
