# Handoff — 11-issue-engine (wave 7)

## Delivered scope

`crates/workflows` only: `Cargo.toml` (dev-dependencies only: `removarr-fs`, `removarr-config`, for tests), `src/lib.rs`,
`src/issues/**` (pure classification, explanation, aggregation), `src/observe/**` (scan session and refresh),
`tests/{issues,observe}_*.rs` plus `tests/common/**`. `Cargo.lock` gained the two dev edges. Domain, connectors, root
`Cargo.toml` and other crates are untouched. Normal dependencies are unchanged (domain, matching, tokio, ...): the
dependency gate (`cargo tree -p removarr-workflows -e normal`, no storage/fs/qbit/arr/jellyfin/config) prints nothing.

Nothing here performs an action. The only writes are scan session rows, observation snapshots, the funnel output
(`MatchStore::replace_for_session`) and the issue set (`IssueStore::replace_current`).

| Module | Contents |
|---|---|
| `issues::classify` | `classify(&Observation, &IssueSettings) -> Result<Classified, ClassifyError>`: all 13 categories (the 7 MASTER categories plus `symlink`, `arr_queue`, `unmapped_torrent`, ...) |
| `issues::input` | `Observation`, `IssueSettings`, `QbitObservation { NotConfigured, Unavailable, Available(snapshot) }`, `ArrObservation` |
| `issues::scope` | `Perimeter` / `RootPerimeter` / `QbitPerimeter` and `perimeter(&Observation)`: the whole-root rule for automatic mode (A2, OQ-05) |
| `issues::explain` | `explain(&Issue, now) -> Explanation` (facts / candidates / proven / recommendation, structured codes, no prose), `evidence_level(&Issue)` |
| `issues::aggregate` | `InodeLedger`, `aggregate(&[Issue], &InodeLedger) -> Aggregates`, `is_ignored(&Issue)` |
| `issues::index` | `TorrentIndex` (path + inode ownership), `protection()` (version-aware allowlist), `mapping_faults()` |
| `issues::perm` | `permission_findings(inv, owner)` (pure copy of the fs rule; parity-tested against `ConfinedFs`) |
| `observe` | `run_scan`, `refresh`, `ObserveConfig`, `ObservePorts`, `ServiceCache`, `RefreshTarget`, `ScanReport`, `ObserveError`, service readers |

## Public contracts

```rust
// Classification (pure; no I/O)
let c: Classified = classify(&obs, &settings)?;          // Err only for Running/Cancelled/Interrupted/Failed sessions
c.issues      // every issue, ignored ones included (blocker `Ignored`), sorted by (category, subject): feed IssueStore::replace_current
c.active()    // lists, bulk actions, automatic mode: ignored excluded
c.ignored()   // the "Ignored" view
c.perimeter   // Perimeter: plan-engine calls perimeter.auto_blockers(&roots) for the whole-root A2 rule
c.aggregates  // Aggregates (dashboard) ; c.ledger // InodeLedger

// Scan / refresh
let report = run_scan(&ports, &cfg, &ctl).await?;        // ScanReport { session, classified: Option<_>, services: ServiceCache }
let c = refresh(&ports, &cfg, RefreshTarget::Qbittorrent, &mut cache).await?;
```

* `run_scan` (architecture 7.1): `begin_session`; roots **sequentially** in `cfg.roots` order (`spawn_blocking`, `save_inventory`
  per root); an unfinished root or cancellation -> `Cancelled`; a root that cannot be opened (`Err`) -> `Failed`; in both cases
  `classified = None` and the issue set is **not** replaced. Then qBittorrent (version, listing, files with bounded concurrency),
  ARR (status, library, queue, history of hashes that hold completed content; never quality snapshot/preview/search), Jellyfin
  (status + libraries), the matching funnel on completed -> media (previous output carried over, **no full comparison**),
  `MatchStore::replace_for_session`, classification, `replace_current`, `save_torrents`, `save_service_status`, `finish_session`.
  Status: `CompleteWithErrors` for any scan error, `MountBoundary`/`NonUtf8Name` zone, or a configured service that is not `Ok`
  (`NotConfigured` and `QbitInternal` zones do not degrade it, CP1-F19). A service outage never aborts the scan (CON-20).
* `refresh`: `latest_session` (must `allows_planning`), re-reads the targeted services through their APIs (and any service
  missing from `ServiceCache`), loads inventories with `ScanStore::load_inventory`, `MatchStore::current`,
  `IgnoreStore::ignored`, classifies, `replace_current`. **It never touches `fs`/`content`**: tested with a counting fake
  filesystem port (`observe_scan.rs::refresh_makes_zero_filesystem_calls_and_zero_content_reads`). A root with no stored
  inventory classifies as not scanned (`ScanIncomplete`).
* `ServiceCache` exists because no port persists the ARR library/queue/history: the job runner (14) keeps it in memory; an empty
  cache is refilled by API reads, never by filesystem calls.

### Deterministic issue IDs (plan-engine, reporting)

`IssueId = first 32 hex of sha256("issue\0" + category + "\0" + subject)` (domain `IssueId::derive`). Evidence, outcome, blockers
and `first_seen_at` never change the id; one issue per `(category, subject)`. Input order never changes output (tested).

| Category | Subject key | `Unit` | Severity | Outcome | Notes |
|---|---|---|---|---|---|
| `incomplete_stalled` | `t:<hash>` | `Torrent` | Warning | KeepManual | needs `progress<1`, content under an incomplete root, `now-added_on >= incomplete_stalled_days`; busy state -> `TorrentProtectedState`; not every file mapped under incomplete -> `MappingError` |
| `incomplete_orphan` | `p:incomplete:<rel>` | `File` | Warning | KeepManual | no torrent (path or inode, incl. `.!qB`/`.parts` rules) |
| `hardlink_missing` | `p:completed:<rel>` | `File` (the video) | Warning, `KeepSample`: Info | `Consolidate`, `KeepManual`, `KeepSample` | completed **video** whose `(dev, ino)` is not in any media root; one row per path (the 13 of the fixture) |
| `duplicate` | `d:<dev>:<ino>-<dev>:<ino>` (sorted) | `Pair{a: completed, b: media}` | Warning | Consolidate | equal-size pair with content proof NameSize/Semantic/Sampled/Identical; `Differs` and identity-only pairs are not duplicates |
| `orphan_completed` | `p:completed:<rel>` | `File`, or `Folder{dir, members: videos, annexes: everything else incl. dirs, symlinks}` | Warning | KeepManual | path held by no torrent; release folder = one unit when its whole subtree is torrent-less (CON-23) |
| `torrent_missing_files` | `t:<hash>` | `Torrent` | Critical | KeepManual | state `missingFiles`, or a wanted file certainly absent (all candidates mapped, root finished, no scan problem near) |
| `media_without_source` | `p:<media root>:<rel>` | `File` | Info | KeepManual | permanent `MediaRootDeletion` blocker, potential 0, empty `physical_ids` |
| `permission` | `p:<root>:<rel>` (root dir: empty rel) | `File` | Critical (parent not writable), else Warning | KeepManual | all roots; owner mismatch always carries `PermissionDenied` (no chown offered) |
| `junk` | `p:completed:<rel>` | `File` | Info | KeepManual | extension list + `.rNN` + orphan subtitle; path **and** inode torrent-less; never samples, never videos |
| `torrent_protected` | `t:<hash>` | `Torrent` | Info, Critical for `error`/unknown state | KeepManual | non-eligible torrent holding completed content; blocker `TorrentProtectedState` |
| `unmapped_torrent` | `t:<hash>` | `Torrent` | Critical (`NoRule`/`Invalid`), Info (`DeclaredExternal`/`OutsideRoots`) | KeepManual | only field + outcome stored, never the raw service path |
| `symlink` | `p:<root>:<rel>` | `File` | Info | KeepManual | blocker `Symlink`; link target text never stored (INV-09) |
| `arr_queue` | `q:<service>:<id>` | `Torrent(download hash)` | Warning, Critical on ARR error | KeepManual | items needing attention only; items with no download id are skipped (torrent-only product) |

### Evidence contract

Every issue carries `evidence: Vec<Evidence>` (provenance, `observed_at`, `completeness`) built in this order: fact entries first
(`Physical` of the item(s), `TorrentHolds` of each holder with `Provenance::Qbittorrent{taken_at}`), then proof entries.

* **Observed facts**: `Physical{path, sig}` (filesystem, `Filesystem{session}`), `TorrentHolds`, `SamePhysical{a != b}` (two inodes
  observed distinct), `ServiceUnavailable` (unused by this wave), and **notes**.
* **Candidates**: `Content{a, b, proof}` with proof `NameSize`/`Semantic`/`Sampled`; `Identity{subject:"pair:<a>", proof:NameParse}`.
* **Proven**: `Content{.., Identical{compared_at}}` or `Differs{offset}`; `SamePhysical{a == b}` (same inode, proven hardlink);
  `Identity{subject:"arr:<service>:<id>", proof:ArrMatched}`.
* **Recommendation**: not stored. Derived by `explain()` from category, outcome and blockers (`RecommendationKind`), with
  `actionable = no refusing blocker && kind is not informational/ignored/keep-sample`.
* **Notes** (domain `EvidenceKind` has no note variant): an `Identity` entry with `IdentityProof::None` and
  `subject = "note:" + code` (`NOTE_PREFIX`). Codes: `no_inode_shared_with_media`, `no_inode_shared_with_completed`,
  `stored_candidates_no_longer_resolve`, `no_torrent_holds_path`, `inode_shared_with_torrent_file:<rooted path>`,
  `inode_also_in_media:<rooted path>`, `junk_extension`, `orphan_subtitle`, `torrent_state:<state>`, `file_absent:<rooted path>`,
  `mapping:<save_path|download_path|content_path|file>:<no_rule|invalid|declared_external|outside_roots>`,
  `links_outside_scanned_trees_possible`, `unmanaged_by_arr`, `arr_identity_unknown`, `symlink_not_followed`,
  `permission:owner_mismatch:<uid>:<gid>`, `permission:parent_not_writable[:<dir>]`, `permission:not_writable_by_owner`,
  `arr_queue:status:<status>:state:<tracked state>` (ARR messages are deliberately not stored).
* **Stored proofs** (`MatchStore::current`) are used only while `MatchCandidate::proof_still_valid` against the *current*
  signatures (nlink ignored). An invalid proof is shown as `ContentProof::None` with an `Incomplete` evidence (V6), never trusted.
* `evidence_level(&Issue)` = strongest `Content` level (`None` when no content question). Overall completeness:
  `Issue::completeness()` (domain).
* **Age**: `Issue.age_basis` (file mtime ms; newest mtime for folder units; `added_on` for torrent issues); display age =
  `age_basis.age_days(now)`; `first_seen_at` is set to the observation instant and preserved by `IssueStore`.

### Blockers used (all from `BlockerCode`; none invented)

`ScanIncomplete` (root unscanned/unfinished, or an absence claim over an unfinished/errored root), `ScanErrorUnder` (error at, above or
below the target; any error in a root for absence claims), `ProtectedZone` (any zone, incl. `QbitInternal`, at/around the unit),
`ServiceUnavailable`/`ServiceNotConfigured` (qBittorrent for every ownership-dependent classification, CP1-F10; ARR for
identity-dependent ones), `UnmappedTorrents` (`NoRule`) and `MappingError` (`Invalid`) on every torrent-less deletion (orphans, junk,
incomplete orphans, OQ-05/07), `TorrentProtectedState` (non-allowlisted, unknown, progress<1, wrong version name, mapping uncertain,
holder not in the observation), `Sample`, `Ignored`, `ArrMissing`/`ArrAmbiguous`/`ArrNotUpgradeEvidence` (a different release in the library
is never resolved here), `ContentDiffers`, `CrossDevice`, `PermissionDenied` (unwritable target directory, owner mismatch),
`DuplicateTarget`, `ImportInProgress` (ARR queue state, unknown counts as importing), `MediaRootDeletion`, `Symlink`,
`QueueItemHidesError` (INV-39 evidence below). `Recent` is not used: age is data (`age_basis`); the auto age rule (A3) is plan-engine's.

* `arr_queue`: step 1 inode check (every completed video of the download shares `(dev, ino)` with a media file ARR lists for that
  media), step 2 no `ParentNotWritable` finding on the media parent directories. `QueueItemHidesError` unless both pass.
* Folder unit with an ignored member (or ignored annex) is ignored as a whole (fail-closed). A pair whose source/target path is ignored
  is ignored.

### Dashboard aggregates (reporting)

`Aggregates { per_category: BTreeMap<IssueCategory, CategoryTotals{count, blocked, ignored, logical_bytes, potential_bytes}>,
by_severity, ignored_count, potential_bytes, potential_unblocked_bytes, naive_potential_sum, shared_bytes }`. Potential =
allocated bytes of **distinct `PhysicalId`s** behind active issues (`Issue.physical_ids`), per category and globally (global is the union,
`naive_potential_sum` shows the double count). An inode counts only when every seen link is selected and nothing else holds it
(`estimate_reclaimable`); `hardlink_missing`/`duplicate` `Consolidate` counts the library copy, `KeepManual` with a library candidate
counts the source (a label, never a promise), `media_without_source`, samples and ignored: 0. `InodeLedger::from_inventories` builds
the per-inode allocation (and `shared_bytes`) from inventories; reporting must pass inventories it loaded.

## Deviations from the architecture and open decisions

1. **Allowlist is version-aware (follows 08), unlike the parity doc §7.3/§7.7.** `pausedUP` is eligible only on 4.x, `stoppedUP` only on
   5.x, an unknown major protects everything. The base fixture mixes both names under v5.0.4, so on the base fixture H3 (`pausedUP`)
   protects FX-04. §7.7's "FX-04 eligible" holds on a 4.x snapshot (tests use major 4; both majors are asserted in
   `issues_parity.rs::the_allowlist_is_version_aware_in_the_mixed_name_fixture`). Orchestrator: confirm that strict reading.
2. **No notes in `EvidenceKind`**: notes are `Identity` entries with `IdentityProof::None` (see above). Request for the domain (frozen):
   an `EvidenceKind::Note { code, detail }` variant would remove the convention.
3. **Ignored issues are persisted** with the `Ignored` blocker (so the Ignored view has issue data). The reason and date live in the
   `ignores` table; the `IgnoreStore` port returns subjects only, so reporting reads the reason from SQL (04 contract).
4. **`TorrentIndex` and `protection()` duplicate qbit's `PathIndex`/`classify`** (workflows may not depend on `removarr-qbit`). Differences:
   an inode view, and a torrent without a file list owns its whole content directory (fail-closed). `permission_findings` duplicates the
   fs rule for the same reason and so that refresh needs no filesystem port; a test asserts equality with `ConfinedFs` on the fixture for
   three owners. Reconciling the copies belongs to a later domain patch (they are pure).
5. **Duplicates are searched completed -> media only** (one funnel run). Intra-tree pairs (completed/completed, media/media) are not
   produced, so no informational duplicate category entries for them yet.
6. `arr_queue` skips queue items without a `download_id` (no torrent unit to anchor them).
7. `incomplete_stalled` uses `added_on` age only (low I/O, MASTER 3); an actively `downloading` old torrent is listed (OQ-06 only blocks busy states).
8. `hardlink_missing` is per video **file** (13 paths on the fixture); release-folder units appear in `orphan_completed` (CON-23). Orphan
   non-video files outside a release folder are individual `orphan_completed` issues (also junk when they match, OQ-04).
9. Samples: a sample-named file is never `junk` (not listed at all); in other categories it is listed with `Sample`.
10. `Perimeter` (not an issue field) carries the whole-root A2 rule; plan-engine must call `perimeter.auto_blockers(&lock_roots)` or recompute
    it with `perimeter(&Observation)` on loaded data. Per-issue blockers implement only the manual (target-scope) rule of OQ-05.

## Gaps for other owners

* 04: no persisted ARR queue/library/history (ARR-dependent issues need the in-memory `ServiceCache` or an API re-read after restart).
  A port to list current issues would let a refresh diff instead of replace.
* 12 (plan-engine): consolidation planning must still re-run `VerifyContent` (stored proof is sample level after a scan); `DeleteReplaced`
  is never set by classification (ARR quality evidence comes from Evaluate); `Outcome::DeleteReplaced` stays reserved for 12.
* 15 (reporting): per-row torrent count = `Issue.torrent_hashes.len()`; exports of the T-08 completed/media inventories are `hardlink_missing`
  (completed side) and `media_without_source` (media side).
* 16 (app): build `IssueSettings` from `files.*`, `thresholds.incomplete_stalled_days`, `owner.expected`; `ObserveConfig.roots` in scan order.

## Tests

`issues_categories.rs` (35: every category, evidence levels, proof validity, samples, ARR cases, INV-39), `issues_failclosed.rs` (16: unfinished/missing/
errored roots, zones, unmapped torrents, qBittorrent not configured/unavailable, allowlist by version and state, media-root guard, ARR state),
`issues_overlap.rs` (15: ids, input order, multi-category, inode double count, samples, ignore list incl. member/pair/torrent, explanation),
`issues_parity.rs` (8: real `ConfinedFs` + funnel on the parity fixture, §7.7 tables, V2, V5, permission-rule equality), `issues_scale.rs`
(20k files, 3k torrents: 0.9 s in debug), `observe_scan.rs` (15: scan, cancel, unfinished root, root failure, outages, ignore + refresh, zero fs calls,
restart refresh), 6 unit tests in `issues::files`. Legacy rows routed here: T-01, T-02, T-08, T-15 (see `docs/parity-matrix.md`).

## Commands run and results (host target, rustc 1.98)

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test -p removarr-workflows`: pass (6 unit + 35 + 16 + 15 + 8 + 1 + 15 integration).
- `cargo test --workspace`: pass.
- `cargo tree -p removarr-workflows -e normal --prefix none | grep -E '^removarr-(storage|fs|qbit|arr|jellyfin|config) '`: prints nothing.
- Not run: `cargo build --workspace --release` (no code outside `crates/workflows` changed; the crate builds in dev).
- No network beyond crates.io; nothing committed.

## Wave 16 fixes

`crates/workflows/src/observe/mod.rs` only; behaviour, issues, evidence, sessions and persisted rows unchanged.

* `run_scan`: the inventories are moved into the funnel's blocking task (partitioned by root kind, run, reassembled in scan
  order and returned) instead of three clones; the previous match set is dropped right after the funnel; `Observation`
  takes `qbit`/`arr_obs` by value and they are moved back out of it for the `ServiceCache` (no second torrent snapshot).
* `refresh`: `cache.qbit`/`cache.arr` are lent to `Observation` with `take()` and restored after `classify` (also on a
  classify error), instead of cloned.
* Not changed: `run_funnel` needs the whole `previous` match set (`FunnelInput.previous: &[MatchCandidate]`), so it is
  still loaded in full on a rescan; streaming it needs a `matching` change (owner media-match). `classify` builds its own
  indexes (torrent/path maps, evidence) which dominate the rescan peak; not reduced here.

### Measured (release, perf_budgets, loopback, synthetic)
| Budget | Before | After |
|---|---|---|
| B1 idle RSS | 72.1 MiB | 68.7 MiB (MISS, limit 40) |
| B2 first scan | 149.2 MiB | 130.9 MiB (PASS) |
| B2 unchanged rescan | 222.7 MiB | 209.7 MiB (MISS, limit 160) |
| B4 scan / rescan | 3.8 / 3.3 s | 3.92 / 3.37 s (PASS) |
| B7 dashboard / list 50 / selection summary | | 1.11 / 16.3 / 2.69 ms (PASS) |

Idle: 30.7 MiB right after start vs 68.7 MiB after the scans; the roughly 38 MiB difference is freed-but-resident heap
retained by the macOS system allocator from the scan peak (live data after the scan is small: torrents shed, issues in
SQLite), so most of the idle miss is allocator retention, not live data. Still needs a Linux/musl full-fixture run or an
allocator decision (architect / devops).

### Remaining rescan gap (open, 50 MiB over budget)
Candidates for a follow-up: classification index memory in `crates/workflows/src/issues` (index.rs/ctx.rs), a streaming
`previous` in `matching`, and `save_inventory`/`load_inventory` retention in db-layer. No cross-crate signature was changed.

### Commands
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` pass; `cargo test --workspace` see the final report.
`cargo build --release -p removarr`; `cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1`
(B2 rescan and B1 MISS, rest PASS; the test fails on those two as before).
