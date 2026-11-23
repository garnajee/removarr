# Architecture — removarr

Owner: `01-system-arch` (wave 1). Status: proposed for checkpoint CP1.
Inputs: `MASTER-PROMPT.md` ("MASTER"), `old/CONTEXTE_RACINE_APP_NAS.md` ("CONTEXTE"),
`docs/legacy-parity.md` (IDs `INV`, `T`, `DR`, `CON`, `REJ`, `NEW`, `CTX`, `OQ`, `FX`).
Precedence: CONTEXTE wins on business rules, MASTER wins over agent files, and this document
wins over agent files on cross-cutting choices. Later agents must not edit this file. They record
deviations in their handoff instead (see section 15.4).

Irreversible choices are recorded as ADRs in `docs/adr/` (index in section 15.5).

---

## 1. Overview

### 1.1 Lifecycle stages and the components that own them

The product cycle is observe -> propose -> plan -> approve -> execute -> verify -> report
(MASTER intro, CONTEXTE §3.1). Each stage has exactly one owning module. A module never calls
into a later stage.

| Stage | Owning module (crate::module) | Agent | Can mutate roots or services? |
|---|---|---|---|
| Observe | `workflows::observe` (scan pipeline) using `fs`, `qbit`, `arr`, `jellyfin` read ports | 11 | No |
| Propose | `workflows::issues` (classification), `matching` (evidence) | 11, 07 | No |
| Evaluate (more observation on request: ARR decisions, ffprobe) | `workflows::evaluate` | 12 | No (ARR and ffprobe reads only) |
| Plan | `workflows::plans` | 12 | No (writes DB only) |
| Approve | `workflows::plans::approve` (user form or auto policy) | 12 | No (writes DB only) |
| Execute | `workflows::actions` through the `Mutators` set | 13 | Yes, and only here (live mode only) |
| Verify | `workflows::actions::verify` (re-stat, inode, recheck, ARR/Jellyfin reads) | 13 | No |
| Report | `reporting` (read model, exports, logs view, SVG data) | 15 | No |
| Schedule / run | `jobs` (queue, lanes, locks, cancellation, recovery trigger) | 14 | Never directly. It calls `actions` |
| Present | `web` (Axum handlers, Askama templates, auth) | 16, 17, 19 | Never directly. It enqueues jobs |

Structural consequences:
- Only `workflows::actions` holds a `Mutators` value. Mutating adapters require a `LiveMode`
  token that exists only when `dry_run = false` (section 6, ADR-0003).
- Scans have no access to mutators. Display routes are `GET`-only and never call workflows
  that write anything other than reads. ARR searches use `ArrRead`, which has no grab method
  (INV-22, INV-38).

### 1.2 Layering (CONTEXTE §6.1)

```text
web (Axum + Askama + HTMX)          app (composition root, CLI)
        │                                   │ wires everything
jobs (queue, lanes, scheduler)              │
        │
workflows (observe, issues, evaluate, plans, actions)  ── reporting (read model)
        │                                                   │
matching (pure parsing/similarity + funnel)                 │
        │                                                   │
domain (types, state machines, policies, PORTS = traits) ◄──┘
        ▲
adapters: fs, qbit, arr, jellyfin, storage, config  (implement domain ports)
```

---

## 2. Cargo workspace and crate boundaries

### 2.1 Crates

The workspace root is `/Cargo.toml` with `members = ["crates/*"]` and `resolver = "3"`, so adding a
crate never edits the root file. Edition is 2024 and `rust-version = "1.88"`. The build
toolchain is rustc 1.98 (local Homebrew and the Docker builder). No `rust-toolchain.toml` is
used, because there is no rustup locally.

| Crate dir | Package (lib name) | Purpose | Allowed deps (workspace crates) | Owner(s) |
|---|---|---|---|---|
| `crates/domain` | `removarr-domain` | Types, IDs, paths, state machines, pure policies (allowlist, sample rule, gain accounting, plan hash, recheck evaluation), error classes, **ports (traits)** | none | 03 |
| `crates/config` | `removarr-config` | Config loading (TOML + env), validation, roots, path mappers, `Secret`, redaction, `LiveMode` token | domain | 05 |
| `crates/storage` | `removarr-storage` | SQLite via sqlx, migrations, repositories implementing persistence ports, `Db` handle | domain | 04 |
| `crates/fs` | `removarr-fs` | Confined directory-handle access: scanner, stat, dir snapshot, content sampling and full compare, permission analysis (06); mutation adapter `mutate` (13) | domain, config | 06 (read), 13 (`mutate`) |
| `crates/qbit` | `removarr-qbit` | qBittorrent Web API: `QbitRead` and `QbitControl` adapters, state parsing, path reconstruction | domain, config | 08 |
| `crates/arr` | `removarr-arr` | Sonarr/Radarr v3 REST: `ArrRead` and `ArrCommands` adapters, quality snapshots, correlation | domain, config | 09 |
| `crates/jellyfin` | `removarr-jellyfin` | Jellyfin REST: `JellyfinRead` and `JellyfinCommands` adapters | domain, config | 10 |
| `crates/matching` | `removarr-matching` | Name normalization, scene/P2P parsing, similarity, language/edition checks, funnel (name+size -> semantic -> samples), ffprobe adapter (`MediaProbe`, built from a crate-local `FfprobeSettings { program, timeout, root_paths: BTreeMap<RootId, PathBuf> }` filled by `app`) | domain | 07 |
| `crates/workflows` | `removarr-workflows` | Use cases: `observe` and `issues` (11), `evaluate` and `plans` (12), `actions` (13) | domain, matching | 11, 12, 13 |
| `crates/jobs` | `removarr-jobs` | Persistent queue runtime, lanes, scheduler, root locks, cancellation, backoff, auto cycle orchestration | domain, workflows | 14 |
| `crates/reporting` | `removarr-reporting` | Read model (SQL queries returning view DTOs), exports, log ring buffer layer, chart series and SVG geometry | domain, storage | 15 |
| `crates/web` | `removarr-web` | Axum router, handlers, typed responses, Askama templates, static assets, i18n, auth/CSRF/rate limit | domain, config, workflows, jobs, reporting | 16, 17, 19 |
| `crates/app` | `removarr` (binary `removarr`) | Composition root, startup sequence, CLI subcommands, system `Clock`/`IdGen` | all | 02 (stub), 16, 17 |
| `crates/testkit` | `removarr-testkit` (`publish = false`, dev-dependency only) | Fake clock/IDs, parity fixture builder, fake qBittorrent/ARR/Jellyfin loopback servers | domain | 02, 03, 06, 08, 09, 10 (per file, section 15) |
| `crates/integration-tests` | `removarr-integration-tests` (tests only) | Cross-crate integration tests (wave 15) | all | 22 |
| `crates/e2e-perf` | `removarr-e2e-perf` (tests/benches only) | End-to-end and resource-budget tests | all | 23 |

Why three connector crates instead of one: wave 5 runs 06, 08, 09 and 10 in parallel. Separate
crates give each agent its own `Cargo.toml` and `lib.rs`, so one agent's work-in-progress compile
errors never block another agent's `cargo test -p`. `jobs` is separate from `workflows`
because it owns the runtime (tokio tasks, `CancellationToken`), while `workflows` stays testable
with plain fakes.

### 2.2 Dependency direction (enforced)

- `removarr-domain` depends only on: `serde`, `serde_json`, `thiserror`, `sha2`, `uuid` (type
  only), `async-trait`. It never depends on tokio, sqlx, reqwest, axum, askama, rustix or
  tracing-subscriber, and never uses `std::fs`, `std::process` or `std::net`.
- Adapters (`config`, `storage`, `fs`, `qbit`, `arr`, `jellyfin`) depend on domain, never on
  `workflows`, `jobs`, `web` or each other (the only exception is `config`, which every adapter
  may use).
- `workflows` depends on domain ports and `matching`. It never depends on adapter crates or
  `config`. It receives `Arc<dyn Port>` values and crate-local settings structs
  (`ObserveSettings`, `PlanSettings`, `ActionSettings`) built by `app` from the config.
- `matching` depends on domain only. It never depends on `config`, so `workflows` stays free of
  it transitively.
- `reporting` is the only non-adapter crate allowed to depend on `storage` (CQRS read side:
  it reads through `Db::reader()`). It never writes.
- `web` never depends directly on `storage`, `fs` or connector crates (it reads through
  `reporting` and commands through `workflows`/`jobs`).
- Only `app` constructs adapters.

Quality-gate commands that enforce this (documented by 02 in `docs/development.md`, run by 21
and 27). Each must print nothing:

```sh
cargo tree -p removarr-domain -e normal --prefix none | grep -E '^(tokio|sqlx|reqwest|axum|askama|rustix|hyper) '
grep -rnE 'std::(fs|process|net)' crates/domain/src
cargo tree -p removarr-workflows -e normal --prefix none | grep -E '^removarr-(storage|fs|qbit|arr|jellyfin|config) '
cargo tree -p removarr-web -e normal --depth 1 --prefix none | grep -E '^removarr-(storage|fs|qbit|arr|jellyfin) '
# (web reaches storage only transitively through reporting, which is allowed; the check is on direct deps)
```

---

## 3. Libraries and versions

The version requirements below are caret requirements, and Cargo.lock pins the exact resolved
versions. All of these exist and are mutually compatible. 02 records the resolved versions in its
handoff. All third-party dependencies are declared **once** in `[workspace.dependencies]` by 02.
Crates use `{ workspace = true }`.

| Crate | Requirement | Features (default-features) | Used by |
|---|---|---|---|
| tokio | `1.47` | `rt-multi-thread`, `macros`, `sync`, `time`, `signal`, `process`, `net` | all async crates; dev: `test-util` |
| tokio-util | `0.7.15` | default (`sync::CancellationToken` needs no feature) | jobs, app, web |
| axum | `0.8.4` | default (`http1`, `json`, `query`, `form`, `tokio`) | web, testkit |
| axum-extra | `0.10` | `cookie-private`, `form` (multi-value forms) | web |
| tower | `0.5.2` | `util`, `limit`, `timeout` | web |
| tower-http | `0.6.2` | `trace`, `timeout`, `limit`, `set-header`, `catch-panic` | web |
| askama | `0.14` | default (no web-framework integration crate; render to `Html<String>`) | web |
| sqlx | `0.8.3` | default-features = false; `runtime-tokio`, `sqlite` (bundled), `macros`, `migrate` | storage, reporting |
| reqwest | `0.12.12` | default-features = false; `json`, `cookies`, `rustls-tls` (ring, webpki roots) | qbit, arr, jellyfin |
| url | `2.5` | default | config, connectors |
| serde | `1.0.219` | `derive` | all |
| serde_json | `1.0.140` | default | all |
| toml | `0.8.20` | default | config |
| thiserror | `2.0.12` | default | all libs |
| tracing | `0.1.41` | default | all non-domain |
| tracing-subscriber | `0.3.19` | `fmt`, `env-filter`, `registry` | app, reporting |
| async-trait | `0.1.88` | default | domain (ports), adapters, workflows |
| uuid | `1.16` | `serde`; `v7` (enabled in app only) | domain, app |
| sha2 | `0.10.8` | default | domain |
| rustix | `1.0` | `fs`, `process` | fs, config, testkit |
| argon2 | `0.5.3` | default (`password-hash`, `alloc`) | web (auth), app (`hash-password`) |
| getrandom | `0.3` | default | web (session/CSRF tokens, salts), jobs (jitter) |
| ipnet | `2.11` | `serde` | config (trusted proxies), web |
| regex | `1.11` | default | matching |
| unicode-normalization | `0.1.24` | default | matching |
| time | `0.3.37` | `formatting`, `macros` | web, reporting (display only) |
| http-body-util | `0.1.3` | default | dev: web, integration-tests |
| tempfile | `3.15` | default | dev: fs, storage, config, testkit, workflows |
| proptest | `1.6` | default | dev: config, domain, matching |

Deliberately not used: OpenSSL (static musl builds), `askama_axum` (deprecated with askama 0.13+),
sqlx compile-time `query!` macros (they would need a live `DATABASE_URL` or `.sqlx` cache; use
`query_as` + `FromRow`), `anyhow`, `chrono`, `cron` (scheduling is interval + UTC anchor, see
section 11), `tower-sessions` (in-memory session table, ADR-0006), `wiremock` (fakes are tiny
axum apps in `testkit`), any JS bundler or chart library.

**Profiles** (02): `[profile.release]` `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`,
`opt-level = 3`, `panic = "unwind"` (needed by `catch-panic` and journaled recovery).
`[profile.dev.package."*"] opt-level = 1` (faster tests with sqlite and argon2).

**Lints** (`[workspace.lints]`, 02): `rust.unsafe_code = "forbid"`,
`rust.missing_debug_implementations = "warn"`, `clippy.all = "warn"`,
`clippy.unwrap_used = "warn"`, `clippy.expect_used = "warn"`, `clippy.dbg_macro = "warn"`,
`clippy.print_stdout = "warn"` (allowed in `app::cli` only), `clippy.todo = "warn"`. The gate runs
with `-D warnings`. `clippy.toml`: `allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`.

**Local vs Docker builds**: local builds and tests use the host target only (macOS or Linux).
Code that is Linux-only (`posix_fadvise`) is `#[cfg(target_os = "linux")]` with a no-op fallback.
Static `x86_64/aarch64-unknown-linux-musl` builds happen only in the Docker builder (ADR-0009).

---

## 4. Domain model (crate `removarr-domain`, agent 03)

All types derive `Debug`, `Clone`, `Serialize` and `Deserialize`, and `PartialEq`/`Eq` where
meaningful. Collections inside plan, evidence and snapshot types are `Vec` or `BTreeMap`/`BTreeSet`,
never `HashMap`, so serialization is deterministic. Field lists below are the minimum. 03 may add
fields but must not change meaning.

### 4.1 Modules and key types

| Module | Types and functions |
|---|---|
| `ids` | `SessionId(Uuid)`, `PlanId(Uuid)`, `JobId(Uuid)`, `ActionRef { plan: PlanId, index: u32 }`, `IssueId(String)` = first 32 hex chars of `sha256("issue\0" + category + "\0" + subject)`, `GainId(String)` (see `gain`), `SubjectKey(String)` |
| `time` | `Timestamp(i64)` = UTC unix **milliseconds**, `MtimeNs(i64)` |
| `path` | `RootKind { Completed, Incomplete, Media }`, `RootId(String)` (`"completed"`, `"incomplete"`, `"media-<8 hex of sha256(canonical path)>"`), `RelPath(String)` (UTF-8, `/`-separated, non-empty components, no `.`, `..`, NUL or leading `/`; root itself = empty), `RootedPath { root: RootId, rel: RelPath }`. **Plans, issues and the DB never store absolute host paths.** |
| `fs` | `EntryKind { File, Dir, Symlink, Other }`, `PhysicalId { dev: u64, ino: u64 }`, `EntrySignature { kind, dev, ino, size, nlink, mtime_ns, mode, uid, gid }`, `FsEntry { path, sig, allocated: u64 (st_blocks*512), symlink_target: Option<String> (display only) }`, `IoKind { NotFound, PermissionDenied, CrossDevice, AlreadyExists, NotADirectory, Other }`, `ScanErrorRecord { path, io: IoKind, message }`, `ProtectedZone { path, reason: QbitInternal | MountBoundary | NonUtf8Name }`, `RootInventory { root, root_sig, entries, errors, protected, finished: bool }`, `DirSnapshot { dir, entries: BTreeMap<RelPath, EntrySignature> }` (includes `""` for the dir itself), `PermissionFinding { path, kind: OwnerMismatch { uid, gid } \| ParentNotWritable \| NotWritableByOwner }` |
| `session` | `AuditSession { id, started_at, finished_at, status, roots: Vec<RootStatus>, services: Vec<ServiceStatus>, metrics: SessionMetrics }`, `SessionStatus { Running, Complete, CompleteWithErrors, Cancelled, Interrupted, Failed }`, `RootStatus { root, scanned: bool, error_count, protected_count }`, `ServiceKind { Qbittorrent, Sonarr, Radarr, Jellyfin }`, `ServiceStatus { service, state: ServiceState, observed_at }`, `ServiceState { Ok { version }, Unavailable { reason }, AuthRejected, NotConfigured, MappingErrors { count } }` |
| `torrent` | `TorrentHash(String)` (lower hex, 40 or 64), `TorrentState` (every documented 4.x and 5.x state + `Unknown(String)`; `parse(&str)` is case-sensitive per API), `fn destructive_eligible(state, progress) -> bool` (INV-28: `progress >= 1.0` **and** state in `uploading`, `stalledUP`, `queuedUP`, `forcedUP`, `pausedUP`, `stoppedUP`), `fn is_checking(state)`, `fn was_active(state)` (DR-27), `MappedPath { Mapped(RootedPath), OutsideRoots, DeclaredExternal, NoRule, Invalid { reason } }` (each variant keeps the raw service path string for display), `TorrentInfo { hash, name, state, progress: f64, added_on, completion_on: Option<Timestamp>, category, size, save_path: MappedPath, download_path: Option<MappedPath> (qBittorrent >= 4.4 temp folder; `None` when unset or empty), content_path: MappedPath }`, `TorrentFile { index, name, size, priority, progress: f64, candidates: Vec<MappedPath> }`. `candidates` = **every** plausible on-disk location, in DR-23 order: from `save_path`, from `download_path`, from `content_path`, each immediately followed by its `.!qB` variant (CP1-F03). `fn is_working_file(name) -> bool` (suffix `.!qB`, or a libtorrent `.parts` file: name `.parts` or `.<hash>.parts`), `TorrentSnapshot { taken_at, version: String, major: u8, torrents, files: BTreeMap<TorrentHash, Vec<TorrentFile>> }`, `fn evaluate_recheck(before, observations) -> RecheckStatus { Verified, Pending, TimedOut, Failed }` (CON-12: verified only if checking was observed **and** has ended, `progress >= 1.0` and state allowlisted). Tracker URLs and magnet URIs are **not modeled** (INV-31). |
| `arr` | `ArrService { Sonarr, Radarr }`, `ArrMediaRef { service, media_id: i64, episode_ids: Vec<i64> }`, `MediaKind { Movie, Episode, Special }`, `MediaIdentity { kind, arr: Option<ArrMediaRef>, external_ids: BTreeMap<String,String>, title, year, season, episode, absolute }`, `ArrFileRef { file_id, path: MappedPath, quality, custom_formats: Vec<String>, cf_score: i64, cutoff_not_met: bool, languages: Vec<String> }`, `ArrCandidateDecision { subject: String, size, quality, custom_formats, cf_score, episode_ids: Vec<i64> (episodes ARR assigns to this file), approved: bool, rejections: Vec<String>, source: ManualImportPreview \| Parse }`, `ArrQualitySnapshot { service, service_version, media, profile_id, profile_name, cutoff, upgrade_allowed, min_format_score, cutoff_format_score, format_scores: BTreeMap<String,i64>, current_files: Vec<ArrFileRef>, candidates: Vec<ArrCandidateDecision>, media_management: Option<ArrMediaManagement>, captured_at, fingerprint }` (fingerprint = sha256 of canonical profile + scores + current files, INV-34), `ArrQueueItem { id, download_id: Option<TorrentHash>, media: Option<ArrMediaRef>, status, tracked_state, messages }`, `ArrHistoryRecord { download_id, media, event_type, source_title, date }`, `ArrLibraryEntry { media, identity, file: ArrFileRef, monitored }`, `ArrEpisode { id, series_id, season, episode, absolute: Option<i32>, scene_absolute: Option<i32>, scene_season: Option<i32>, scene_episode: Option<i32>, has_file, file_id: Option<i64>, monitored, aired: bool }` (CP1-F04), `ReleaseRef { guid, indexer_id }`, `ReleaseSearchResult { release: ReleaseRef, indexer, title, size, seeders: Option<u32>, leechers: Option<u32>, age_days, protocol, quality, custom_formats, cf_score, approved, rejections, download_allowed }` and `fn sort_search_results(&mut [ReleaseSearchResult], current_allocated: u64)` (ARR approval first, then CF score, then estimated gain = current allocated bytes minus release size; CP1-F06), `CoverageProof { media: ArrMediaRef, expected: BTreeSet<i64>, mapping: BTreeMap<i64, RootedPath>, specials_included: bool }`, `ArrMediaManagement { recycle_bin: Option<MappedPath>, recycle_bin_cleanup_days: Option<u32>, hardlinks_enabled: bool, captured_at }` (CP1-F20; stored in `ArrQualitySnapshot.media_management` and included in its fingerprint), `OldFileRecovery { OtherLink(RootedPath), RecycleBin, Unrecoverable }` |
| `jellyfin` | `JellyfinItemRef { id, kind, name, path: MappedPath, series, season, episode, provider_ids }`, `JellyfinLibrary { name, locations: Vec<MappedPath> }` |
| `media` | `ProbeReport { duration_s: Option<f64>, video: Vec<VideoStream>, audio: Vec<AudioStream { codec, language, channels }>, subtitles: Vec<SubtitleStream> }` |
| `evidence` | `ContentProof { None, NameSize, Semantic { score_milli: u32 }, Sampled, Identical { compared_at }, Differs { offset } }`, `IdentityProof { None, NameParse { score_milli }, ArrMatched, Ambiguous }`, `QualityEvidence { None, ArrSnapshot { fingerprint } }` (three separate dimensions: content, media identity and quality, CONTEXTE §9.3), `Provenance { Filesystem { session }, Qbittorrent { taken_at }, Arr { service, captured_at }, Jellyfin { at }, Ffprobe { at }, Computed }`, `Evidence { kind: EvidenceKind (typed payloads), provenance, observed_at }`, `MatchCandidate { a: RootedPath, b: RootedPath, sig_a, sig_b, identity: IdentityProof, content: ContentProof, reasons: Vec<String>, session }`. This is the persisted funnel output. A proof is valid only while both current inventory signatures equal `sig_a`/`sig_b` (CP1-F07). |
| `issue` | `IssueCategory` (section 4.2), `Severity { Info, Warning, Critical }`, `Outcome { Consolidate, DeleteReplaced, KeepManual, KeepSample }`, `Unit { File(RootedPath), Folder { dir, members, annexes }, Torrent(TorrentHash), Pair { a, b } }`, `BlockerCode` (section 4.3), `Blocker { code, detail }`, `Issue { id, category, subject, unit, severity, outcome, evidence, blockers, torrent_hashes: BTreeSet, logical_bytes, potential_bytes, age_basis: Timestamp, first_seen_at, session }` |
| `candidate` | `CandidateState` per CONTEXTE §6.4 / CTX-12 (`Observed, Identified, AmbiguousManual, Candidate, Rejected, Planned, PlanExpired, Approved, Executing, FailedCompensated, FailedNeedsReview, Executed, Verified, Closed`). It is **derived, not stored** (CP1-F18): `fn derive_candidate_state(issue: Option<&Issue>, plan: Option<PlanStatus>, outcome: Option<&ActionOutcome>) -> CandidateState`, used by `reporting` for display. The stored, enforced state machine is `PlanStatus` (below). `ActionOutcome::Verified` is the only path to `Verified`. |
| `plan` | `PlanMode { DryRun, Live }`, `PlanOrigin { User, AutoPolicy }`, `ActionKind` (section 13), `fn ActionKind::tier()`, `fn ActionKind::capability()`, `Precondition { EntryUnchanged { path, sig, compare_nlink }, DirUnchanged { snapshot }, NoScanErrorUnder { path }, TorrentStates { states: BTreeMap<TorrentHash,(TorrentState,f64)> }, NoTorrentReferences { path }, ArrFingerprint { media, fingerprint }, NoArrImport { media, hashes }, SameDevice { a, b }, ServiceAvailable(ServiceKind), ConfigFingerprint(String), OldCoverageRecoverable { old_files: Vec<(RootedPath, PhysicalId, OldFileRecovery)> } (CP1-F20) }`, `VerificationStep { InodeEquals { path, phys }, Absent { path }, RecheckVerified { hashes }, TorrentAbsent { hashes }, ArrCurrentFile { media, path }, JellyfinRecognized { path }, ModeEquals { path, mode }, PathStillHasInode { path, phys } (completed source unchanged after an ARR import, CP1-F01), ArrQueueHasRelease { service, release }, OldFileAccounted { path, recovery } (where each replaced old media file went, CP1-F20) }`, `PlannedAction { index, kind, preconditions, verification, expected_gain: GainEstimate, depends_on: Vec<u32>, torrent_hashes }`, `ActionPlan { id, created_at, expires_at, mode, origin, session, config_fingerprint, actions, excluded: Vec<(SubjectKey, Vec<Blocker>)>, lock_roots: BTreeSet<RootId>, torrent_states, arr_snapshots, gains: PlanGains }`, `PlanHash(String)` = sha256 hex of canonical `serde_json` bytes of `ActionPlan`, `PlanStatus { Draft, Approved, Executing, Executed, PartiallyExecuted, Failed, Expired, Invalidated, Discarded, Interrupted }` with `fn PlanStatus::can_transition_to(self, next) -> bool` (explicit table, enforced by `PlanStore::transition`. Simulations do not change the status; see section 6), `Approval { plan_id, plan_hash, approved_at, by: User \| AutoPolicy, confirmed_count, confirmed_torrent_removals: u32, acknowledged_samples: Vec<RootedPath>, acknowledged_unrecoverable: Vec<RootedPath> (CP1-F20) }` |
| `journal` | `JournalStep { FsProbeHardlink, FsReplaceWithHardlink, FsRemoveFile, FsRemoveTree, FsRemoveEmptyDir, FsSetMode, QbitStop, QbitStart, QbitRecheck, QbitRemoveKeepData, ArrManualImport, ArrRemoveQueueItem, ArrSetMonitored, ArrGrab, ArrRescan, JellyfinRefresh }`, `IntentRecord { id, action: ActionRef, step, mode (Live \| Simulated), target: serde_json::Value, observed: serde_json::Value, expected_gain, hashes, created_at }`, `StepOutcome { Done { final_state }, Failed { class, message }, Simulated, Skipped { reason } }`, `ResultRecord { intent_id, outcome, at }`, `ActionOutcome { Verified, ExecutedUnverified { reason }, Failed { class }, Compensated, FailedNeedsReview, Simulated, NotExecuted { blockers } }`, `UnfinishedIntent` |
| `gain` | `GainKind { AttributableDeletion, AttributableConsolidation, AddedCost }`, `GainRecord { gain_id, action, kind, phys, bytes, recorded_at }`, `fn gain_id(phys, size, mtime_ns) -> GainId` (sha256-derived; the DB enforces UNIQUE, so the same freed inode can never be counted twice, CON-13), `fn estimate_reclaimable(selected: &BTreeMap<PhysicalId, (u32 selected_links, u64 nlink, u64 allocated)>, held_elsewhere: &BTreeSet<PhysicalId>) -> u64` (DR-04, T-03), `ObservedDelta { root, free_before, free_after_local, free_after_services }`, `PlanGains { logical, attributable, added, net }` |
| `policy` | `fn is_sample(&RelPath) -> bool` (OQ-01), `RiskTier { T1, T2, T3, T4, T5 }`, `Capability` (section 13) |
| `error` | `ErrorClass` (section 10.1), `PortError { class, detail: String }` (detail is already redacted), `DomainError` (validation and transition errors) |
| `ports` | traits, section 5 |

### 4.2 Issue categories (final names; stable for URLs, DB and i18n keys)

| `IssueCategory` | MASTER | Subject key | Default severity | Actionable? |
|---|---|---|---|---|
| `incomplete_stalled` | §5.1 | `t:<hash>` | Warning | Manual only: `CleanupStalledIncomplete` (OQ-06) |
| `incomplete_orphan` | §5.1 | `p:<root>:<rel>` | Warning | Manual only: `DeleteIncompleteOrphan` |
| `hardlink_missing` | §5.2 | `p:completed:<rel>` (file or release-folder unit) | Warning (`KeepSample`: Info) | By outcome: `Consolidate`, `DeleteReplaced`, `KeepManual`, `KeepSample` |
| `duplicate` | §5.3 | `d:<dev>:<ino>-<dev>:<ino>` (sorted pair) | Warning | Only completed -> media pairs (`ConsolidateHardlink`); other pairs are informational |
| `orphan_completed` | §5.4 | `p:completed:<rel>` | Warning | Manual deletion (tier 3); auto only per section 13.3 |
| `torrent_missing_files` | §5.4 | `t:<hash>` | Critical | Informational (a "library copy with same size exists" hint, CTX-04 deferred) |
| `media_without_source` | §5.4 | `p:<media root>:<rel>` | Info | **Never** actionable (INV-26) |
| `permission` | §5.5 | `p:<root>:<rel>` | Critical (parent not writable), Warning (owner mismatch) | Manual `FixMode` only, own entries only |
| `junk` | §5.6 | `p:completed:<rel>` | Info | Manual deletion (tier 3); never auto |
| `torrent_protected` | §5.7 | `t:<hash>` | Info (Critical for `error`/`unknown`) | Informational: lists non-allowlisted torrents that hold completed content or block an action |
| `unmapped_torrent` | §5.7 / CON-07 | `t:<hash>` | Critical (`NoRule`/`Invalid`), Info (`DeclaredExternal`/`OutsideRoots`) | Informational; blocks actions per OQ-07 |
| `symlink` | OQ-15 | `p:<root>:<rel>` | Info | Never actionable |
| `arr_queue` | §6 / CONTEXTE §3.9, §6.2 (CP1-F05) | `q:<service>:<queue id>` | Warning (Critical when ARR reports an error) | Manual `ArrRemoveQueueItem` (cap `arr_queue_remove`). This is blocked with `QueueItemHidesError` unless the INV-39 evidence shows the item is stale: first the inode check (the download's completed files already share `(dev, ino)` with media files of the same media), then no `permission` finding on the media parent dirs. |

An element may appear in several categories: there is one issue per (category, subject) (OQ-04).
"Potentially recoverable" totals are deduplicated per `PhysicalId` across all categories, and are
labeled potential, never guaranteed (INV-21).

Mapping of the `docs/legacy-parity.md` §7.7 topic names: Hardlink defects -> `hardlink_missing`; Real
duplicates -> `duplicate`; Orphans -> `orphan_completed` + `torrent_missing_files`; Media-only ->
`media_without_source`; Junk -> `junk`; Permissions -> `permission`; Symlinks -> `symlink`;
qBittorrent protection -> `torrent_protected`; Out-of-scope torrent -> `unmapped_torrent`;
Incomplete -> `incomplete_stalled` (FX-36 via T_O) and `incomplete_orphan` (FX-37).

### 4.3 Blocker codes (minimum set)

`ScanIncomplete`, `ScanErrorUnder`, `ServiceUnavailable(ServiceKind)`, `ServiceNotConfigured(ServiceKind)`,
`MappingError`, `UnmappedTorrents`, `TorrentProtectedState`, `TorrentReferencesTarget`, `Sample`,
`Recent`, `Ignored`, `ArrMissing`, `ArrAmbiguous`, `ArrRejected`, `ArrNotUpgradeEvidence`,
`CoverageIncomplete`, `DurationMismatch`, `AudioLanguageMissing`, `EditionMismatch`,
`PermissionDenied`, `CrossDevice`, `FailedConsolidation`, `ImportInProgress`, `ContentDiffers`,
`DuplicateTarget`, `MediaRootDeletion`, `CapabilityDisabled`, `NotInBuild`, `DryRunOnly`,
`ProtectedZone`, `Symlink`, `NumberingMismatch` (parsed numbering disagrees with ARR, CP1-F04),
`QueueItemHidesError` (CP1-F05), `ReleaseNotInSearch` (CP1-F06), `ArrImportMoved` (the completed source
path vanished or changed inode after an ARR import, CP1-F01), `OldCoverageUnrecoverable` and
`ArrHardlinksDisabled` (CP1-F20).

**Units containing a protected zone** (any `ProtectedZone` inside the unit's subtree, including
`QbitInternal`) always carry `ProtectedZone` and can never be planned (CP1-F19).

---

## 5. Ports (traits in `removarr_domain::ports`)

Filesystem ports are **synchronous**. Callers run them inside `tokio::task::spawn_blocking`.
Network, persistence and process ports are async through `#[async_trait]`, so that they are
object-safe as `Arc<dyn Trait>`. Every async read method must be safe to drop (cancellation by
`select!`). Mutating async methods must **not** be dropped mid-flight by callers (section 8.2).

```rust
// Skeleton. 03 writes the final code with these names and semantics.
pub trait Clock: Send + Sync { fn now(&self) -> Timestamp; }
pub trait IdGen: Send + Sync { fn uuid(&self) -> uuid::Uuid; }

#[derive(Clone, Default, Debug)]
pub struct CancelFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);   // is_cancelled(), cancel()
#[derive(Clone, Default, Debug)]
pub struct ScanControl { pub cancel: CancelFlag, pub progress: std::sync::Arc<std::sync::atomic::AtomicU64> }

// ---- Filesystem (impl: removarr-fs). Every path is a RootedPath; impls resolve it from the
// root directory handle with no-follow semantics and reject symlinked components.
pub trait FsInventory: Send + Sync {
    fn scan_root(&self, root: &RootId, ctl: &ScanControl) -> Result<RootInventory, PortError>;
    fn stat(&self, path: &RootedPath) -> Result<Option<EntrySignature>, PortError>; // None = ENOENT
    fn snapshot_dir(&self, dir: &RootedPath) -> Result<DirSnapshot, PortError>;
    fn free_space(&self, root: &RootId) -> Result<u64, PortError>;
    fn permission_findings(&self, inv: &RootInventory, owner: (u32, u32)) -> Vec<PermissionFinding>;
}
pub trait ContentReader: Send + Sync {
    fn samples_match(&self, a: &RootedPath, b: &RootedPath, expect: [&EntrySignature; 2],
                     spec: SampleSpec, ctl: &ScanControl) -> Result<bool, PortError>;
    fn full_compare(&self, a: &RootedPath, b: &RootedPath, expect: [&EntrySignature; 2],
                    chunk_bytes: u32, ctl: &ScanControl) -> Result<CompareOutcome, PortError>;
    // CompareOutcome { Identical, Differs { offset }, Changed } (signature re-checked after reading)
}
pub trait FsMutator: Send + Sync {          // live impl requires LiveMode (section 6)
    fn probe_hardlink(&self, source: &RootedPath, target_dir: &RootedPath) -> Result<(), PortError>;
    fn replace_with_hardlink(&self, source: &RootedPath, target: &RootedPath,
                             expect_source: &EntrySignature, expect_target: &EntrySignature)
                             -> Result<PhysicalId, PortError>;  // temp link + renameat + verify + fsync dir
    fn remove_file(&self, target: &RootedPath, expect: &EntrySignature) -> Result<(), PortError>;
    fn remove_tree(&self, dir: &RootedPath, expect: &DirSnapshot) -> Result<(), PortError>;
    fn remove_empty_dir(&self, dir: &RootedPath, expect: &EntrySignature) -> Result<(), PortError>;
    fn set_mode(&self, target: &RootedPath, expect: &EntrySignature, mode: u32) -> Result<(), PortError>;
}

// ---- qBittorrent (impl: removarr-qbit)
#[async_trait::async_trait]
pub trait QbitRead: Send + Sync {
    async fn version(&self) -> Result<String, PortError>;          // login lazily; 200 "Ok." and 204 accepted
    async fn torrents(&self) -> Result<Vec<TorrentInfo>, PortError>;
    async fn files(&self, hash: &TorrentHash) -> Result<Vec<TorrentFile>, PortError>;
}
#[async_trait::async_trait]
pub trait QbitControl: Send + Sync {        // live impl requires LiveMode
    async fn stop(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;   // 4.x pause / 5.x stop
    async fn start(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;  // 4.x resume / 5.x start
    async fn recheck(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;
    async fn remove_tasks_keep_data(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;
    // There is no boolean parameter. The impl sends deleteFiles=false as a literal (INV-06).
}

// ---- Sonarr / Radarr (impl: removarr-arr), one instance per service
#[async_trait::async_trait]
pub trait ArrRead: Send + Sync {
    fn service(&self) -> ArrService;
    async fn status(&self) -> Result<String, PortError>;                     // version
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError>;
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError>;
    async fn history_for_download(&self, hash: &TorrentHash) -> Result<Vec<ArrHistoryRecord>, PortError>;
    async fn quality_snapshot(&self, media: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError>;
    async fn manual_import_preview(&self, folder: &RootedPath, media: Option<&ArrMediaRef>)
        -> Result<Vec<ArrCandidateDecision>, PortError>;                     // GET only, never imports
    async fn episodes(&self, series_id: i64, season: Option<i32>)
        -> Result<Vec<ArrEpisode>, PortError>;                               // Sonarr: ALL expected episodes, with or without files (CP1-F04)
    async fn search_releases(&self, media: &ArrMediaRef)
        -> Result<Vec<ReleaseSearchResult>, PortError>;                      // GET /release: never grabs
    async fn command_status(&self, id: i64) -> Result<CommandState, PortError>;
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError>; // GET config/mediamanagement: recycle bin + hardlinks (CP1-F20)
}
#[async_trait::async_trait]
pub trait ArrCommands: Send + Sync {        // live impl requires LiveMode + per-method capability
    async fn rescan(&self, media: &ArrMediaRef) -> Result<i64, PortError>;
    async fn rescan_all(&self) -> Result<i64, PortError>;                   // service-wide rescan (CP1-F13)
    async fn manual_import(&self, req: &ManualImportRequest) -> Result<i64, PortError>;     // cap arr_manual_import
    // ManualImportRequest { files: Vec<(RootedPath, ArrMediaRef)> } has NO mode field. The adapter
    // always sends importMode = "copy" as a literal (ARR hardlinks when "Use Hardlinks instead of
    // Copy" is on). "move"/"auto" can never be expressed (CP1-F01).
    async fn grab(&self, release: &ReleaseRef) -> Result<(), PortError>;                    // cap arr_grab (CP1-F06)
    async fn remove_queue_item_keep_media(&self, id: i64) -> Result<(), PortError>;          // cap arr_queue_remove
    async fn set_monitored(&self, media: &ArrMediaRef, monitored: bool) -> Result<(), PortError>; // cap arr_set_monitored
}

// ---- Jellyfin (impl: removarr-jellyfin)
#[async_trait::async_trait]
pub trait JellyfinRead: Send + Sync {
    async fn status(&self) -> Result<String, PortError>;
    async fn libraries(&self) -> Result<Vec<JellyfinLibrary>, PortError>;
    async fn items_by_paths(&self, paths: &[RootedPath])
        -> Result<BTreeMap<RootedPath, Option<JellyfinItemRef>>, PortError>;
    async fn library_refresh_running(&self) -> Result<bool, PortError>;     // ScheduledTasks state, for bounded polling (CP1-F13)
    // Item refresh has no status endpoint, so it is not polled. Verification uses items_by_paths.
}
#[async_trait::async_trait]
pub trait JellyfinCommands: Send + Sync {   // live impl requires LiveMode
    async fn refresh_library(&self) -> Result<(), PortError>;               // 204 accepted
    async fn refresh_item(&self, id: &str) -> Result<(), PortError>;
}

// ---- ffprobe (impl: removarr-matching::ffprobe)
#[async_trait::async_trait]
pub trait MediaProbe: Send + Sync {
    async fn probe(&self, path: &RootedPath) -> Result<ProbeReport, PortError>;
}

// ---- Persistence (impl: removarr-storage). Each method is ONE SQLite transaction.
#[async_trait::async_trait] pub trait ScanStore: Send + Sync {
    async fn begin_session(&self, s: &AuditSession) -> Result<(), PortError>;
    async fn save_inventory(&self, session: SessionId, inv: &RootInventory) -> Result<DiffStats, PortError>; // diff vs current rows, batched
    async fn finish_session(&self, s: &AuditSession) -> Result<(), PortError>;
    async fn latest_session(&self) -> Result<Option<AuditSession>, PortError>;
    async fn load_inventory(&self, root: &RootId) -> Result<RootInventory, PortError>;   // current rows
}
#[async_trait::async_trait] pub trait SnapshotStore: Send + Sync {
    async fn save_torrents(&self, session: SessionId, s: &TorrentSnapshot) -> Result<(), PortError>;
    async fn latest_torrents(&self) -> Result<Option<TorrentSnapshot>, PortError>;
    async fn save_arr(&self, s: &ArrQualitySnapshot) -> Result<(), PortError>;
    async fn save_service_status(&self, session: SessionId, s: &[ServiceStatus]) -> Result<(), PortError>;
}
#[async_trait::async_trait] pub trait MatchStore: Send + Sync {             // persisted funnel output (CP1-F07)
    async fn replace_for_session(&self, session: SessionId, c: &[MatchCandidate]) -> Result<(), PortError>;
    async fn update_proof(&self, a: &RootedPath, b: &RootedPath, proof: &ContentProof) -> Result<(), PortError>; // VerifyContent
    async fn current(&self) -> Result<Vec<MatchCandidate>, PortError>;
}
#[async_trait::async_trait] pub trait IssueStore: Send + Sync {
    async fn replace_current(&self, session: SessionId, issues: &[Issue]) -> Result<(), PortError>; // keeps first_seen_at
    async fn get_many(&self, ids: &[IssueId]) -> Result<Vec<Issue>, PortError>;
    async fn attach_evidence(&self, id: &IssueId, ev: &[Evidence]) -> Result<(), PortError>;
}
#[async_trait::async_trait] pub trait IgnoreStore: Send + Sync {
    async fn ignore(&self, subjects: &[SubjectKey], reason: Option<&str>, at: Timestamp) -> Result<(), PortError>;
    async fn restore(&self, subjects: &[SubjectKey]) -> Result<(), PortError>;
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError>;
}
#[async_trait::async_trait] pub trait PlanStore: Send + Sync {
    async fn insert(&self, plan: &ActionPlan, hash: &PlanHash) -> Result<(), PortError>;   // plan + actions atomically
    async fn get(&self, id: PlanId) -> Result<Option<(ActionPlan, PlanHash, PlanStatus, Option<Approval>)>, PortError>;
    async fn approve(&self, a: &Approval) -> Result<(), PortError>;       // Conflict unless Draft, hash equal, not expired
    async fn transition(&self, id: PlanId, from: PlanStatus, to: PlanStatus) -> Result<(), PortError>; // CAS
}
#[async_trait::async_trait] pub trait JournalStore: Send + Sync {
    async fn record_intent(&self, i: &IntentRecord) -> Result<(), PortError>;  // durable (synchronous=FULL) before returning
    async fn record_result(&self, r: &ResultRecord) -> Result<(), PortError>;
    async fn record_outcome(&self, a: ActionRef, o: &ActionOutcome) -> Result<(), PortError>;
    async fn unfinished(&self) -> Result<Vec<UnfinishedIntent>, PortError>;
    async fn for_plan(&self, id: PlanId) -> Result<Vec<(IntentRecord, Option<ResultRecord>)>, PortError>;
}
#[async_trait::async_trait] pub trait GainStore: Send + Sync {
    async fn record(&self, gains: &[GainRecord], delta: Option<&ObservedDelta>) -> Result<(), PortError>; // UNIQUE gain_id; duplicate = Conflict
}
#[async_trait::async_trait] pub trait JobStore: Send + Sync {
    async fn enqueue(&self, job: &JobRecord) -> Result<JobId, PortError>;     // UNIQUE dedupe_key while queued/running
    async fn claim_next(&self, lane: Lane) -> Result<Option<JobRecord>, PortError>;
    async fn set_stage(&self, id: JobId, stage: &str) -> Result<(), PortError>; // stage changes only, not per-file progress
    async fn finish(&self, id: JobId, outcome: &JobOutcome) -> Result<(), PortError>;
    async fn interrupt_running(&self) -> Result<Vec<JobRecord>, PortError>;    // at startup
}
#[async_trait::async_trait] pub trait StateStore: Send + Sync {             // small app_state key/value
    async fn get(&self, key: &str) -> Result<Option<String>, PortError>;    // e.g. "auto_paused"
    async fn set(&self, key: &str, value: &str) -> Result<(), PortError>;
}
```

`JobRecord`, `Lane { Observe, Execute }`, `JobKind` and `JobOutcome` are domain types (section 8.1).
`SampleSpec { size, count }`, `ManualImportRequest`, `CommandState` and `DiffStats` are domain types.

---

## 6. Dry-run and capability enforcement (ADR-0003)

1. `dry_run` and `auto_mode` are read **once at startup**. There is no UI toggle, and changing
   them requires a restart. The settings page shows them read-only.
2. `removarr_config::LiveMode` is a zero-sized token with a private field. The only constructor
   is `Config::live_mode(&self) -> Option<LiveMode>`, which returns `None` when `dry_run = true`.
   A `#[cfg(feature = "test-support")] LiveMode::for_tests()` exists for tests only. The feature
   is enabled only through `[dev-dependencies]`, never in the `app` package's normal dependencies.
3. Every **live** mutating adapter constructor takes `&LiveMode`:
   `removarr_fs::mutate::LiveFsMutator::new(roots, &LiveMode)`, `removarr_qbit::QbitController::new(.., &LiveMode)`,
   `removarr_arr::ArrCommander::new(.., caps, &LiveMode)` and `removarr_jellyfin::JellyfinCommander::new(.., &LiveMode)`.
   Read adapters never take it.
4. `workflows::actions::Mutators { mode: PlanMode, fs, qbit, arr, jellyfin }` is built **only** in
   `app::wiring`. Two kinds exist. `Mutators::recording()` holds
   `workflows::actions::dry_run::Recording*` implementations: they record the would-be call into
   the journal (`IntentRecord.mode = Simulated`, `StepOutcome::Simulated`) and touch nothing. The
   recording set can always be built. `Mutators::live(&LiveMode, ..)` holds the real adapters and
   can be built only with a `LiveMode`. With `dry_run = true`, only the recording set exists in
   the process. With `dry_run = false`, `app` builds both.
5. Plans carry `mode` (`DryRun` when created in a dry-run process, else `Live`).
   `JobKind::ExecutePlan { plan_id, simulate: bool }`:
   - `simulate = true` always runs with the recording set. It is allowed for any `Draft` or
     `Approved` plan in any process, never changes `PlanStatus`, and is stored as a simulation run
     (this is CONTEXTE §6.2 Planificateur "simulation complète", also available in live processes;
     CP1-F14e).
   - `simulate = false` requires `plan.mode == Live`, an approved plan, and a live process. The
     runner asserts `mutators.mode == Live == plan.mode`.
   - A `DryRun` plan can only ever be simulated. Going live requires re-planning after a restart
     with `dry_run = false`.
   - Simulated intents are ignored by recovery (`unfinished()` filters `mode = Live`).
6. The dry-run path is the real path. Same plan engine, same revalidation (real `stat`, real
   torrent list re-read, real ARR fingerprint re-read), same step sequence and same journal
   records. The only difference is the recording adapters. Verification steps report `Simulated`.
   Plans and simulated journals are persisted in the DB (CON-28). Nothing is ever written inside
   a root.
7. Capability flags (section 13) are checked twice: when planning (blocker `CapabilityDisabled`)
   and inside the live adapter (`ErrorClass::CapabilityDisabled`). Unimplemented tiers are blocked
   by `workflows::actions::IMPLEMENTED_TIER: RiskTier` (blocker `NotInBuild`). 13 raises it only
   after the tier's crash-point and dry-run tests pass and CP2 passes.
8. Guard tests: (a) `app` unit test: with `dry_run = true`, the wiring produces `PlanMode::DryRun`
   mutators (16); (b) integration test T-30-plus: a full pipeline in dry-run with counting fakes
   performs zero fs mutations and zero `stop/start/pause/resume/recheck/delete`, ARR mutator and
   Jellyfin refresh calls (22, per tier by 13). (c) The gate greps:
   - `deleteFiles` appears exactly once in `crates/qbit/src`, with the literal `false`;
   - `importMode` appears exactly once in `crates/arr/src`, with the literal `"copy"`, and the words
     `move` and `auto` never appear as an import mode (CP1-F01);
   - `LiveMode::` is used only in `config`, the four adapter constructors, `app::wiring` and tests.

   (e) Live-process simulation guard (CP1-F22; owners 13 unit, 22 integration): in a process
   built with `dry_run = false` and counting live fakes, `ExecutePlan { simulate: true }` on an
   approved live plan performs zero fs mutations, zero qBittorrent `stop/start/pause/resume/recheck/delete`,
   zero ARR mutator calls and zero Jellyfin refresh calls, records only `Simulated` intents, and
   leaves `PlanStatus` unchanged.

   (d) `fake_arr` (09) rejects any manual import whose `importMode` is not `copy`. 22 has a test
   where the fake ARR simulates a move (the source path disappears): the action must end in
   `FailedNeedsReview` with `ArrImportMoved`, never `Verified`.

---

## 7. Workflows

### 7.1 Observe (scan session, `workflows::observe`, agent 11)

Sequence for one `Scan` job. All roots and all services together form one `AuditSession`.

1. `ScanStore::begin_session`.
2. For each configured root, sequentially (never in parallel, to avoid HDD seek storms):
   `FsInventory::scan_root` in `spawn_blocking`, then `ScanStore::save_inventory` (diff).
   Cancellation is checked per directory. A cancelled root makes the session `Cancelled` and
   **no issues are produced from a cancelled session**.
3. qBittorrent: `version`, `torrents`, then `files` for every torrent (bounded concurrency).
   Pick each file's *display* path: the first `Mapped` candidate present in the inventory wins,
   else the first candidate (DR-23). **Torrent ownership is wider than the display path**
   (CP1-F03). The ownership index contains every `Mapped` candidate of every file of every
   torrent (the `save_path`, `download_path` and `content_path` forms and their `.!qB` variants),
   plus the inodes of those paths. For a torrent with progress < 1, it also contains the following
   (CP1-F21). **Content directory** is defined only for **multi-file** torrents: it is the
   `content_path` directory, in both its `save_path` and `download_path` forms. Every entry under
   it is owned, as are the `.parts` files directly in the parent directory of that content
   directory. For **single-file** torrents (`content_path` is the file itself), only the file
   candidates (with `.!qB`) and the `.parts` files in the directories of those candidates are owned.
   Their parent directory is **never** treated as owned. `orphan_completed`, `incomplete_orphan`, `junk` and `NoTorrentReferences`
   test against this whole index. An owned path carries `TorrentReferencesTarget`.
4. ARR (each configured service): `status`, `library`, `queue`, and `history_for_download` for
   hashes attached to completed content (bounded). No `quality_snapshot`,
   `manual_import_preview` or `search_releases` here (these run in Evaluate).
5. Jellyfin: `status`, `libraries` (health and version only. Recognition checks run in Evaluate
   and verification).
6. Matching funnel (agent 07): identity index -> name+size index -> semantic candidates (top 3,
   top 10 for repair search) -> samples (eliminate only, bounded, `content_read_concurrency`).
   **No full comparison during scans.** The output is persisted with
   `MatchStore::replace_for_session` (CP1-F07). A `VerifyContent` proof (`Identical`/`Differs`) of
   a pair whose signatures did not change is carried over, not recomputed.
7. Classification (`workflows::issues`): build issues (section 4.2) with evidence, blockers,
   units (release folder rule, OQ-11) and potential bytes. Apply the ignore set: ignored subjects
   are excluded from lists and actions but kept for the "Ignored" view.
8. `IssueStore::replace_current`, `SnapshotStore::*`, `ScanStore::finish_session` with status
   `Complete` or `CompleteWithErrors` (any `ScanErrorRecord`, `ProtectedZone::MountBoundary` or
   `NonUtf8Name`, or service not `Ok`). **Single rule for `QbitInternal` zones** (CP1-F19): they
   are expected and never touched. They do **not** make the session `CompleteWithErrors` and do
   not block auto mode for the root (A2). Any unit whose subtree contains one is blocked
   (`ProtectedZone`, section 4.3).

A service outage does not abort the scan (CONTEXTE §3.15, CON-20). Dependent issues carry
`ServiceUnavailable`, which blocks plans that depend on that service. qBittorrent **not
configured** works the same way (CP1-F10): every action and every classification that needs
torrent ownership (consolidation, every completed or incomplete deletion, `junk`,
`orphan_completed`, `incomplete_*`) carries `ServiceNotConfigured(Qbittorrent)` and is never
plannable. These issues are still listed, so the information is visible.

`RefreshService(kind)` job: re-reads one service (step 3, 4 or 5), then re-runs **classification
only** (step 7) from the stored inventory (`ScanStore::load_inventory`) and the stored funnel
output (`MatchStore::current`). It makes **zero filesystem calls and zero content reads** on roots
(CP1-F07, budget B6). It is used by the "Refresh" buttons, and by the scheduled qBittorrent
refresh (`scan.qbit_refresh_minutes`, default 60, 0 = off), which keeps incomplete tracking current
between filesystem scans through the API only (MASTER §3).

### 7.2 Evaluate (`workflows::evaluate`, agent 12)

This is a read-only job, triggered explicitly (issue detail button "Ask Sonarr/Radarr", bulk
action) or by the auto cycle. For the selected issues it gathers `ArrRead::quality_snapshot`,
`ArrRead::manual_import_preview` on the completed folder (ARR's own accept/reject decision for
local files, including "not an upgrade"), optional `search_releases` (display only), and `MediaProbe::probe`
(bounded, `ffprobe_concurrency`) for the current and candidate files. For Sonarr media it also
reads `ArrRead::episodes` (every expected episode, with or without a file, including specials and
absolute or scene numbers) so that the coverage proof can be built (CP1-F04). Results are attached
as evidence (`IssueStore::attach_evidence`). It runs only on demand, because ARR's preview and
ffprobe read file content on the HDDs. A media-level variant
(`Evaluate { media: ArrMediaRef, search: bool }`) serves the media detail page and the release
search (CP1-F06).

**Coverage proof** (`plans`, 12, used by `ReplaceRelease`). Expected set = ARR episodes of the
selected seasons that are monitored **and** aired. Specials (season 0) count only when the user
explicitly includes them. Every expected episode id must map to **exactly one** candidate file
through the `episode_ids` of ARR's manual-import preview (ARR's mapping handles absolute, scene and
TVDB numbering). Every candidate file must map only to expected episodes. Otherwise
`CoverageIncomplete`. The result is stored in the plan as `CoverageProof`. The parsed
episode/absolute numbers from `matching` (07) are **explanatory evidence only**. If they disagree
with ARR's assignment, the plan gets `NumberingMismatch` and needs a manual check (INV-36).

**VerifyContent** (`workflows::evaluate::verify_content`, agent 12; CP1-F08). For the selected
`duplicate`/`hardlink_missing` issues it runs `ContentReader::full_compare` on their stored
`MatchCandidate` pairs. Concurrency is bounded by `content_read_concurrency`, and the job holds
shared root locks. Results (`Identical`, `Differs { offset }`, or `Changed` (stored as
`ContentProof::None`, with an evidence note)) are written with `MatchStore::update_proof` and
`IssueStore::attach_evidence`. A proof stays valid only while both signatures are unchanged.
Execution always compares again.

### 7.3 Plan and approve (`workflows::plans`, agent 12)

- `PlanRequest` comes from the backend (unit or bulk use **the same function**). It is an enum:
  - `Issues { category, issue_ids, action, remove_torrent_tasks: bool (default false), include_specials: bool }`;
  - `Media { media: ArrMediaRef, action: SetMonitored { monitored } }` (media detail page, CP1-F05);
  - `Release { media: ArrMediaRef, release: ReleaseRef }` (grab from a search result, CP1-F06);
  - `ServiceRescan { service, media: Option<ArrMediaRef> }` (service-side rescan; CP1-F13).

  Unknown, stale, ignored or wrong-category IDs reject the whole request, with the offending IDs
  listed (CON-32, INV-24). There is no implicit selection.
- Companion actions (CP1-F12): `RemoveEmptyParent` is added automatically for every parent under
  `<completed_root>` that the plan leaves empty or containing only planned annexes.
  `RemoveTorrentTask` is added **only** if `remove_torrent_tasks = true`, and then only for hashes
  whose every wanted file is planned for deletion. The confirm modal lists torrent removals
  separately, with their own typed count (`confirmed_torrent_removals`, DR-35).
- `DeleteReplaced` planning (CP1-F02, manual and auto). For every media id of the unit, ARR's
  current file path must:
  - map (`Mapped`) into a media root;
  - exist in the latest complete scan as a regular file;
  - have a size equal to ARR's file size;
  - have a `PhysicalId` that differs from every deletion target.

  The plan stores an `EntryUnchanged { compare_nlink: false }` precondition per replacement file.
  Any ARR/disk mismatch gives `ArrAmbiguous` and outcome `KeepManual`. A manual plan may still
  delete such an item only as a plain deletion with the reinforced typed confirmation (DR-35).
  It is never `DeleteReplaced` and never auto.
- The plan engine expands each issue into `PlannedAction`s with preconditions captured **from
  the scan session and stored snapshots**, computes gains (DR-04), sorts and dedupes targets (at
  most one consolidation per media target and per source, CON-25), lists blocked items in
  `excluded` with their blockers, sets `expires_at = created_at + plan_ttl_hours`, computes
  `PlanHash`, and persists it with `PlanStore::insert`. The commit is the "plan written and
  synced" step (CONTEXTE §3.5 step 1).
- Approval: `approve(plan_id, plan_hash, confirmed_count, confirmed_torrent_removals, acknowledged_samples)`.
  This is rejected if the hash differs, the plan expired, a count differs from the plan, or a sample
  in a folder unit is not acknowledged (OQ-11). Approval never enqueues
  execution. The user presses "Execute" (separate `POST`).
- Auto policy (`plans::auto`, section 13.3) builds plans with `origin = AutoPolicy` and approves
  them as `Approver::AutoPolicy` only if every auto rule holds.

### 7.4 Execute and verify (`workflows::actions`, agent 13)

Common rules for every plan:
- The job runner holds exclusive root locks for `plan.lock_roots` (section 8.3) before calling
  `actions::execute(plan_id, &Mutators, cancel)`.
- **Revalidate everything before the first service mutation** (fixes CON-11): fs preconditions
  by `stat`/`snapshot_dir`, torrent states by re-reading `torrents`, `NoTorrentReferences` by
  re-reading torrents and files for the targets' paths, ARR fingerprints by re-reading
  snapshots, ARR queue import activity, and the config fingerprint. Any difference sets the
  action to `NotExecuted` and the plan to `Invalidated`. Nothing adapts silently.
- Every mutation step is: `JournalStore::record_intent` (durable), then the mutator call, then
  `record_result`. Intents carry the target, observed state, expected gain and hashes (CONTEXTE
  §3.13).
- Outcome is `Verified` only after the verification steps pass. Otherwise it is
  `ExecutedUnverified`.
- Manual plans: independent items continue after an item failure, and the failing item's hashes
  stay stopped and are reported. Auto plans stop at the first failure (CON-26).
- **Affected hashes** (CP1-F09, DR-25 generalized): every hash that has a `Mapped` file candidate
  (section 7.1 ownership index) equal to any **source or target** path of the plan, including
  media target paths of a torrent that seeds from the library, or that has a target inside its
  content directory. These hashes are stopped, rechecked and restored. Hashes that share an inode
  only through another path are listed as informational in the plan and are not stopped.

**ConsolidateHardlink** (tier 2; CONTEXTE §3.5 order, CON-15):
1. Revalidate all items (source in completed, target in a media root, same `st_dev`, no symlink
   in either chain, signatures with `compare_nlink = false`, target dir writable per scan, torrents
   allowlisted, no ARR import for the media, `ScanErrorUnder` absent).
2. For each item: intent `FsProbeHardlink`, then probe (temp link in target dir, check inode,
   unlink). `EPERM`/`EACCES` fails the item before any content read, with an explanatory message
   that never suggests `sudo` (T-14, CON-30).
3. Intent `QbitStop` {hashes, initial states}, then stop all affected hashes. Revalidate again.
4. For each item: `ContentReader::full_compare` (cancellable per chunk). `Differs` or `Changed`
   fails the item with nothing mutated (T-11, T-12).
5. Intent `FsReplaceWithHardlink`, then `replace_with_hardlink`: `linkat(source -> .<name>.removarr-<pid>-<ns>`
   in the target dir), `renameat` over the target, `fstatat` target == source `PhysicalId`, fsync
   the target dir, then the result with the final inode (T-13, INV-04).
6. Intent `QbitRecheck` for modified hashes, recheck, then poll with `QbitRead::torrents` every
   2 s until `evaluate_recheck` returns `Verified` or `TimedOut` (`recheck_timeout_seconds`).
7. Intent `QbitStart` for hashes that were active initially **and** are either unmodified or
   recheck-verified. Every other hash stays stopped and becomes a "left stopped" record
   (INV-14, INV-20).
8. Verify: `stat` both paths, same `PhysicalId`. `Verified` requires inode equality and a
   verified recheck.
9. Gains: `AttributableConsolidation` = allocated bytes of the replaced media inode if no other
   link held it (scan `nlink` minus links in plan == 0), plus an observed free-space delta.

**Delete under completed** (tier 3: `DeleteCompletedFile`, `DeleteCompletedFolder`, `DeleteJunk`,
`RemoveEmptyParent`, `RemoveTorrentTask`):
1. Revalidate (signatures with `compare_nlink = true` (CON-14), folder snapshots, no scan
   errors under or above, torrents holding any target allowlisted with progress 1,
   `NoTorrentReferences` for torrent-less targets (qBittorrent must be reachable, otherwise
   blocked), and for `DeleteReplaced` items the `EntryUnchanged` precondition of every
   replacement media file is re-`stat`ed and the ARR current-file path is re-read and must be
   equal (CP1-F02). Any mismatch: `NotExecuted` + `ArrAmbiguous`).
2. If any hash is affected: intent `QbitStop`, stop, revalidate.
3. Per target: intent, then `remove_file` or `remove_tree` (planned entries only, deepest first,
   symlinks unlinked never followed, T-17), then the result.
4. Parent cleanup under `<completed_root>` only: a parent left empty, or containing only annexes
   listed in the plan, gets `remove_empty_dir` in the same execution (INV-49).
5. Hashes whose files were all deleted and whose removal is planned: intent `QbitRemoveKeepData`,
   then `remove_tasks_keep_data` (INV-19, T-32). Partially affected hashes: recheck, then
   **leave stopped** and report (T-23). Hashes stopped but untouched because the batch aborted:
   resume if initially active (T-22).
6. Verify: targets absent, removed torrents absent from the list. Gains are
   `AttributableDeletion` per inode only when every link is removed (T-03), plus an observed delta.

**CleanupStalledIncomplete** (tier 3, manual only, OQ-06): see section 16 for the rule. Steps:
revalidate (torrent still not complete, its state not in `checking*`/`moving`/`allocating`/`checkingResumeData`,
age >= threshold, every file path `Mapped` under `<incomplete_root>`, data signatures equal to the
plan), intent `QbitStop`, stop, intent `QbitRemoveKeepData`, remove the task (always
`deleteFiles=false`), revalidate the data paths again (exist, confined under `<incomplete_root>`, still
referenced by no torrent), intents `FsRemoveFile`/`FsRemoveTree`, delete, verify (paths
absent, torrent absent).

**DeleteIncompleteOrphan** (tier 3, manual): revalidate (no torrent references the path after a
fresh torrent read, using the full ownership index of section 7.1: all candidates, `.!qB`
variants, `download_path`, content directories of non-complete torrents, `.parts` files), intent,
`remove_file`/`remove_tree`, verify. Working files (`is_working_file`) are never classified as
orphans while any torrent exists whose content directory contains them (CP1-F03).

**FixMode** (tier 3, manual): the target must be owned by the process UID (a rootless process
cannot `chown`, so owner mismatches are report-only, with instructions). Revalidate, intent
`FsSetMode`, `set_mode` (never recursive), verify the mode.

**ReplaceRelease** (tier 4, manual only; ADR-0010):
1. Preconditions captured at planning time:
   - ARR media identified unambiguously;
   - ARR `manual_import_preview` accepts every candidate file with no rejections;
   - `CoverageProof` (section 7.2; INV-35, INV-36);
   - ffprobe duration within tolerance, and the audio languages of the current file present in the
     candidate (INV-37);
   - edition match;
   - ARR snapshot fingerprint;
   - `EntryUnchanged { compare_nlink: false }` and the recorded `PhysicalId` of every **completed
     source** candidate file;
   - same `st_dev` for source and target media root;
   - **`OldCoverageRecoverable`** (CP1-F20; CONTEXTE §3.6, INV-35). ARR's upgrade import deletes
     the old media file *inside* `manual_import`, before the app's inode proof. So every old
     media file that ARR may replace (ARR current files of the covered episodes or movie) must be
     recoverable through one of the following:
     - (a) `OtherLink(path)`: its inode has another link in the latest complete scan, outside the
       media path being replaced (typically the old completed release, still seeding). That link
       is itself an `EntryUnchanged` precondition, and the plan must not delete it before step 3
       is verified;
     - (b) `RecycleBin`: `ArrRead::media_management()` reports a configured recycle bin whose path
       maps (`Mapped`, `OutsideRoots` or `DeclaredExternal`; not `NoRule`/`Invalid`), with a
       cleanup period of 0 (never) or >= 7 days.

     Otherwise the file is `Unrecoverable` and the plan gets `OldCoverageUnrecoverable`. The
     blocker can be lifted **only** by an explicit per-file acknowledgement of the risk at approval
     (`Approval.acknowledged_unrecoverable`, typed confirmation). It is never lifted
     automatically, and `ReplaceRelease` is never auto (A9).
   - If `media_management().hardlinks_enabled` is false, the plan carries the warning
     `ArrHardlinksDisabled`: step 3(c) can never hold, so the best outcome is
     `ExecutedUnverified` with `AddedCost`. The user sees this before approving.
2. Execute: revalidate, including re-reading the ARR snapshot and `media_management()` (the
   fingerprint covers both and must be equal), re-`stat`ing every completed source and every
   `OtherLink` path of `OldCoverageRecoverable`. Then intent `ArrManualImport`, then
   `ArrCommands::manual_import` (ARR is the importer; requires `arr_manual_import`; the adapter
   always sends `importMode = "copy"`, CP1-F01). Poll `command_status` until completion or timeout.
3. Verify per episode or movie. All of these must hold for `Verified`:
   - (a) ARR's current file is the new media path;
   - (b) the **completed source path still exists** with its recorded `PhysicalId`
     (`PathStillHasInode`);
   - (c) the media path shares that same `(dev, ino)` (inode proof, INV-35).

   Outcomes:
   - If (b) fails (source gone or inode changed: ARR moved the file despite `copy`), the outcome is
     `FailedNeedsReview` with `ArrImportMoved`. The affected torrents are reported Critical, and
     every remaining step is cancelled.
   - If (a) and (b) hold but (c) fails (ARR copied instead of hardlinking), the outcome is
     `ExecutedUnverified`. The new file's allocated bytes are recorded as `AddedCost`, and
     `DeleteMediaFile` stays blocked.
   - Jellyfin refresh (live) and the recognition check (`items_by_paths`, polling
     `library_refresh_running` with a bounded timeout) are informational verification.
   - `OldFileAccounted` (CP1-F20): for every old media file, the result records where it went:
     - `OtherLink` re-`stat`ed and still present;
     - moved to the recycle bin (confirmed by `stat` when the recycle bin maps into a root,
       otherwise "per ARR configuration");
     - still in place;
     - gone (only possible for acknowledged `Unrecoverable` files).

     A lost old file that was not acknowledged makes the outcome `FailedNeedsReview`.
4. Old media files that ARR did not remove: an optional `DeleteMediaFile` step. It runs only when
   the user selected it explicitly in this plan, only after step 3 is `Verified` for its episode,
   and never auto (OQ-12).
5. An optional deletion of the old completed release follows the tier-3 rules. Net gain = freed
   minus `AddedCost`. `AddedCost` is 0 when step 3(c) holds, and the new allocated bytes when ARR
   copied. This is consistent with step 3.

**ArrGrabRelease** (tier 4, manual only, never auto, live only; cap `arr_grab`, default off;
CP1-F06). It is created from a `ReleaseSearchResult` of an `Evaluate { search: true }` run no older
than `plan_ttl_hours` (precondition `ReleaseNotInSearch` if missing) and with an unchanged ARR
fingerprint. Steps: revalidate, intent `ArrGrab`, `ArrCommands::grab(release)`. Verify that ARR
accepted the command and the ARR queue shows the item (`ArrQueueHasRelease`) within
`recheck_timeout_seconds`, otherwise `ExecutedUnverified`. ARR then downloads and imports, so ARR
remains the single importer. A search alone never grabs.

**Manual ARR/Jellyfin commands** (live only; one-action plans):
- `ArrRemoveQueueItem` (cap `arr_queue_remove`; `removeFromClient=false`, `blocklist=false`). It
  comes from an `arr_queue` issue and is blocked with `QueueItemHidesError` unless the item is
  proven stale (INV-39; CP1-F05).
- `ArrSetMonitored` (cap `arr_set_monitored`; from `PlanRequest::Media`; the previous value is
  journaled, CTX-06, CTX-10).
- `ArrRescan { service, media: Option<ArrMediaRef> }` (`None` = `rescan_all`) and `JellyfinRefresh`
  (from `PlanRequest::ServiceRescan`; CP1-F13).

### 7.5 Recovery after restart (agent 13, triggered by 14 before any other execute job)

`JournalStore::unfinished()` lists plans with **live** intents that have no result (simulated
intents are ignored). For each one:
- Hashes stopped by the plan, with no fs or ARR mutation intent recorded for their files: resume
  them if they were active initially (journaled `QbitStart`).
- Hashes with a mutation intent that has no result, or a result without a verified recheck:
  intent `QbitRecheck`, recheck, and resume only if `Verified`. Otherwise leave them stopped and
  create a "left stopped" record.
- fs intents without result: `stat` the target and record the observed state as the result
  (`ExecutedUnverified` or `NotExecuted`). Never redo the mutation.
- Set the plan to `Interrupted`. It can never resume. The user re-plans.
- While recovery is unresolved (for example qBittorrent unreachable), no new execute job starts.
  Recovery retries with backoff (section 8.4).

### 7.6 Auto cycle (tier 5; orchestration 14, policy 12)

After a scheduled `Scan` completes, if `auto_mode = true` and `auto_paused` is not set, the
`AutoCycle` job runs the two-phase sequence of DR-34 / CONTEXTE §3.11 (CP1-F11). Evaluate and
observe steps run inline inside the job, with shared root locks only. Exclusive locks are held
only while a plan executes.
1. **Phase 1, consolidations.** Build auto `ConsolidateHardlink` plans from the session, approve
   them as policy, and execute them (live) or simulate them (dry-run).
2. **Rescan, only where something changed** (CP1-F23). Let R be the set of roots touched by at
   least one **live** mutation result (`StepOutcome::Done`) in phase 1.
   - If R is empty (no consolidation executed, or dry-run where every step is `Simulated`),
     there is no rescan, and phase 2 reuses the triggering session, which is unchanged by
     construction.
   - Otherwise run the observe pipeline restricted to R (`observe::run(roots = Some(R))`), with
     fresh service reads. Roots outside R keep their stored inventory and status from the
     triggering session. The new session records which roots were rescanned.
   - Abort the cycle, setting `auto_paused`, if any rescanned root is not error-free, or if a
     required service is not `Ok`. Failed consolidations (the completed identity is still absent
     from media) become `FailedConsolidation` blockers.
3. **Phase 2, deletions.** `Evaluate` the deletion candidates of the session from step 2 (ARR
   evidence), then build, approve and execute auto deletion plans (section 13.3 rules). Execution
   revalidation re-`stat`s every target anyway, so a stale root outside R cannot lead to an
   unchecked deletion.

At the first failure or uncertainty in any phase: stop, set `auto_paused` with the reason
(`StateStore`), and show it on the dashboard. Only a user `POST /auto/resume` clears it.

---

## 8. Concurrency, cancellation, locking, low I/O

### 8.1 Jobs and lanes (agent 14)

| `JobKind` | Lane | Dedupe key | Disk I/O on roots |
|---|---|---|---|
| `Recover` | Execute | `recover` | `stat` only |
| `Scan` (all roots + services) | Observe | `scan` | metadata walk + bounded samples |
| `RefreshService(kind)` (manual, or qBittorrent every `scan.qbit_refresh_minutes`) | Observe | `refresh:<kind>` | **none** (no fs calls, no content reads) |
| `Evaluate { issue_ids }` / `Evaluate { media, search }` | Observe | `evaluate:<sorted ids or media hash>` | ARR preview and ffprobe reads (bounded); none for search only |
| `VerifyContent { issue_ids }` | Observe | `verify:<hash>` | full compare (bounded) |
| `ExecutePlan { plan_id, simulate }` | Execute | `exec:<plan_id>:<simulate>` | as per plan (`stat` and re-reads only when simulating) |
| `AutoCycle { session }` | Execute | `auto:<session>` | as per plans, plus an inline rescan of only the roots that phase 1 live-mutated (none in dry-run) |

- Two worker tasks: **one Observe worker and one Execute worker**. Mutations are globally
  serialized (ADR-0007). This single execution lane is the in-process per-media lock (INV-16)
  and makes per-media lock tables unnecessary. If execution concurrency is ever raised, a
  per-media lock must be added first.
- Jobs are persisted (`JobStore`). At startup `interrupt_running()` marks running jobs
  `Interrupted`. Interrupted `Scan`/`Refresh`/`Evaluate`/`Verify` jobs are re-queued once.
  `ExecutePlan`/`AutoCycle` are never re-queued; recovery handles them.
- Progress lives in memory (`tokio::sync::watch` per job: stage, counters, current item). The DB
  is written only on stage changes and completion. UI polling reads memory only.
- Scheduling never approves anything.

### 8.2 Cancellation

- One root `tokio_util::sync::CancellationToken`, cancelled on SIGTERM or SIGINT. Each job gets
  a child token. `jobs` bridges it to `CancelFlag` for blocking fs work.
- Checkpoints: scan, per directory; sample and compare, per chunk; HTTP reads, by dropping the
  future in `select!`; execution, **only between steps**. A mutation step (one mutator call, or one
  HTTP mutating request) always runs to completion and is journaled.
- On cancellation during execution, the runner stops issuing new mutations and runs compensation:
  resume unmodified initially-active hashes, recheck modified hashes with a bounded timeout, and
  leave unverified ones stopped. It journals `batch_interrupted` (T-22).
- Shutdown: stop accepting HTTP (axum graceful shutdown), cancel jobs, and wait up to
  `shutdown_grace_seconds` (default 45). Compose sets `stop_grace_period: 60s`. If the process
  is killed, recovery runs at the next start.

### 8.3 Locking

- `jobs::RootLocks`: one `tokio::sync::RwLock<()>` per `RootId`. `Scan`, `Evaluate` and
  `VerifyContent` take **shared** locks on the roots they read. `ExecutePlan` takes **exclusive**
  locks on `plan.lock_roots`, acquired in `RootId` order (no deadlock).
- External importer (INV-16): action revalidation refuses consolidation, import or deletion
  for media with an ARR queue item in an importing or import-pending state, or whose
  `download_id` is among the affected hashes (blocker `ImportInProgress`). ARR configured but
  unreachable = blocked. ARR not configured = no external importer known, so this check is skipped.
- SQLite: a single writer connection (section 9.2) serializes writes.

### 8.4 Concurrency limits and retries (defaults; configurable in `[limits]`)

| Resource | Default | Max |
|---|---|---|
| Roots scanned concurrently | 1 (sequential) | 1 (fixed) |
| Content reads (samples, full compare) | `content_read_concurrency = 1` | 4 |
| ffprobe processes | `ffprobe_concurrency = 1` | 4 |
| HTTP requests per service | `http_concurrency_per_service = 4` | 16 |
| Execute worker | 1 | 1 (fixed, ADR-0007) |
| Argon2 verifications | 1 (semaphore) | 1 |
| tokio worker threads | `min(available_parallelism, 4)` | — |
| tokio blocking threads | 8 | — |

HTTP timeouts: connect 5 s, request 20 s (qBittorrent, Jellyfin), 30 s (ARR), ffprobe 120 s
(`kill_on_drop`). Retries: **read** calls with `ErrorClass::Unavailable` retry at most 3 times
with exponential backoff (1 s, 2 s, 4 s, ±20 % jitter). Mutating calls are **never** retried
automatically (REJ-21). Recovery retries every 5 minutes, at most 12 attempts, then needs a
manual retry (`POST /recovery/retry`, CP1-F13).

Response size (CP1-F17, CONTEXTE §6.5): each connector reads at most `limits.max_response_bytes`
(default 32 MiB) per response body, streamed with a counting limit. Exceeding it aborts the request
with `ErrorClass::Rejected`. ffprobe stdout is capped at 4 MiB.

### 8.5 Low-I/O policy (MASTER §3, INV-44)

1. Filesystem scans run only from the scheduler (interval and UTC anchor) or a manual `POST`.
   There is no watcher or loop.
2. Scans read metadata only (`fstatat`), plus at most `sample_count × sample_size` bytes per
   retained semantic pair. Full comparisons happen only in `VerifyContent` and inside execution.
3. Incomplete tracking uses the qBittorrent API (`added_on`, `state`, `progress`) through
   `RefreshService` (manual or every `scan.qbit_refresh_minutes`). It reclassifies from the stored
   inventory and stored funnel output: no filesystem call and no content read on roots.
4. No UI route touches a root. Pages read SQLite (on the config SSD) and in-memory state. Job
   progress polling (`hx-trigger="every 2s"`, only while a job runs) reads memory.
5. `/healthz` touches neither roots nor the DB. `/readyz` runs `SELECT 1` on the SSD DB.
6. SQLite writes are batched: inventory diffs in 1000-row transactions, no per-file commits, no
   per-file progress writes. Unchanged rows are skipped (diff by signature).
7. Logs go to stdout (no fsync) plus an in-memory ring buffer (2000 events). There is no log file.
8. Only the intent journal requires a durable commit (section 9.3).
9. Exports are streamed from the DB in the HTTP response. Nothing is written to disk.
10. Sequential reads use `posix_fadvise(SEQUENTIAL)` and `DONTNEED` after comparison (Linux).

---

## 9. Persistence (agent 04)

### 9.1 Location and schema guidance

DB file: `<data_dir>/removarr.db` (config volume, SSD). It must be outside every root (validated,
INV-33). Migrations are embedded with `sqlx::migrate!("./migrations")`, forward-only, applied
at startup. Tables (04 finalizes the columns):

`meta`; `scan_sessions`, `session_roots`, `session_services`; `fs_entries` (current state per
`(root, rel_path)`: signature, allocated, `first_seen_at`, `changed_session`, `gone_session`);
`scan_errors`, `protected_zones` (per session); `torrents`, `torrent_files` (current snapshot,
upsert-if-changed); `match_candidates` (persisted funnel output: pair, signatures, identity and
content proof, session; CP1-F07); `arr_snapshots` (JSON plus fingerprint); `arr_queue_items` (current);
`simulation_runs` (plan id, started/finished, simulated journal reference); `issues` (current, `first_seen_at`,
`resolved_at`) with an evidence JSON column; `ignores` (`subject_key` PK, reason, `created_at`);
`plans` (canonical JSON, `hash` UNIQUE, status, mode, `expires_at`); `plan_actions`;
`approvals`; `journal_intents`, `journal_results` (FK to intents), `action_outcomes`; `gains`
(`gain_id` UNIQUE); `observed_deltas`; `jobs` (`dedupe_key` UNIQUE where status in
queued/running, enforced by a partial unique index); `app_state`.

JSON columns use the domain's deterministic `serde_json` serialization. Retention: per-session
rows (`scan_errors`, `protected_zones`, `gone` entries, `simulation_runs` with their simulated
journal rows) are pruned after 90 days by an
explicit maintenance step at the end of `Scan`. Plans, journal, approvals and gains are
**never** pruned (audit evidence).

### 9.2 Connections and pragmas

`removarr_storage::Db { writer, reader }`:
- `writer`: `SqlitePool` with `max_connections = 1`; `journal_mode = WAL`,
  **`synchronous = FULL`**, `foreign_keys = ON`, `busy_timeout = 5000`, `temp_store = MEMORY`.
- `reader`: `SqlitePool` with `max_connections = 4`, `read_only = true`, `foreign_keys = ON`, `busy_timeout = 5000`.
- `wal_autocheckpoint` stays at the default (1000 pages). `PRAGMA optimize` runs at the end of
  each `Scan` and at shutdown. There are no checkpoint loops.
- With `synchronous = FULL`, every writer commit fsyncs the WAL. Batching (8.5) keeps commits to
  a few per scan, so this costs little while making every commit durable (ADR-0002).

### 9.3 Intent journal durability

The journal lives in SQLite (`journal_intents`/`journal_results`). `record_intent` returns
only after the writer transaction commits, and with `synchronous = FULL` the WAL is fsynced
before `COMMIT` returns. That satisfies "intent written + fsync before mutation" (INV-08). No
separate JSONL file exists. Exports (`/export`) render the journal as JSONL for humans.

### 9.4 Transaction ownership

| Transaction | Owner |
|---|---|
| Any single DB write (one port method = one SQLite transaction) | storage (04) |
| Inventory diff batches | storage (04), called by observe (11) |
| Plan + actions insert; approval CAS; status transitions | storage (04), called by plans (12) |
| Intent / result / outcome records | storage (04), called by actions (13) |
| Filesystem step atomicity (temp link + rename + verify + dir fsync) | fs `mutate` (13) |
| qBittorrent stop -> mutate -> recheck -> restore sequence | actions (13) |
| Plan-level step ordering, compensation, recovery | actions (13) |
| Job state transitions | jobs (14) via `JobStore` |

No port exposes a DB transaction handle. Composite atomic operations get a dedicated port method.

---

## 10. Errors and observability

### 10.1 Error classes (`removarr_domain::error::ErrorClass`)

| Class | Meaning | HTTP (web) | Retry |
|---|---|---|---|
| `Config` | invalid configuration | n/a (startup exit 2) | no |
| `Validation` | bad input, invalid plan request | 400 / 422 | no |
| `NotFound` | unknown ID | 404 | no |
| `Conflict` | stale plan, hash mismatch, duplicate job, duplicate gain | 409 | no |
| `Unauthorized` / `Forbidden` | no session / CSRF failure | 401 -> redirect to `/login` / 403 | no |
| `RateLimited` | login throttled | 429 + `Retry-After` | no |
| `StateChanged` | revalidation failure | action `NotExecuted`, page shows the reason | no |
| `Unavailable` | service down or timeout (transient) | 503 fragment | reads only (8.4) |
| `Rejected` | service refused (auth, 4xx, protocol) | 502 fragment | no |
| `CapabilityDisabled` | capability flag off, or dry-run | 403 fragment | no |
| `Io(IoKind)` | filesystem errors (ENOENT -> protect the zone, EXDEV -> no consolidation) | 500 fragment | no |
| `Confinement` | path escape, symlink component, outside roots | 500, logged at WARN | no |
| `Integrity` | content differs, post-verify mismatch | n/a (action failure) | no |
| `Storage` | SQLite failure | 503 | no |
| `Cancelled` | cooperative cancellation | n/a | no |
| `Internal` | bug | 500 generic | no |

Each crate defines its own `thiserror` enum with `fn class(&self) -> ErrorClass` and converts it
into `PortError` at the port boundary. Messages are redacted at construction: no secrets, no
query strings, no `userinfo`. Web responses show a translated generic message plus a
correlation ID, never raw error text from services.

### 10.2 Observability

- `tracing` everywhere. The subscriber is built in `app`: `fmt` to stdout (level from
  `log_level`, `RUST_LOG` overrides) plus `reporting::LogBufferLayer` (bounded ring, 2000 events,
  sanitized, served at `/logs`).
- Span conventions: `job{id, kind}`, `scan{session, root}`, `plan{id}`,
  `action{plan, index, step}`, `http{method, route}` (route template, not raw path),
  `connector{service, endpoint}` (endpoint template, no query values).
- Secret hygiene (INV-31): `config::Secret<String>` has a redacting `Debug`/`Display` and no
  `Serialize`. `config::redact::url()` strips userinfo and query. Connector DTOs do not
  deserialize `tracker`, `magnet_uri` or similar fields. API keys are sent only as headers
  (`X-Api-Key`, `Authorization: MediaBrowser Token=...`) and are never logged, since request
  logging is off in reqwest.
- INFO logs: job start and end, session summary, plan created/approved/executed, every journal
  intent and result (one line each), service state changes. Per-file details are DEBUG.
- Metrics (CTX-03) are stored per job in `jobs.outcome` JSON: duration, entries scanned, bytes
  sampled and compared, compare throughput, errors per root, API calls and errors per service,
  recheck durations, expired and invalidated plans, compensated actions, torrents left stopped.
  The dashboard reads them. There is no Prometheus endpoint (not requested).

---

## 11. Configuration contract (agent 05)

Sources: an optional TOML file at `REMOVARR_CONFIG_FILE` (default `/config/removarr.toml`;
missing file = environment only), then `REMOVARR_*` environment variables, which override the
file. **Secrets are environment-only.** The TOML schema has no secret fields
(`deny_unknown_fields`). Lists in env are comma-separated. Mapping entries are
`service_prefix=app_path`, and a path containing `,` or `=` is rejected (ADR-0005).

| Key (TOML) | Env | Default | Validation |
|---|---|---|---|
| `server.bind` | `REMOVARR_BIND` | `0.0.0.0:8080` | socket addr (the compose file publishes `127.0.0.1:8012:8080`) |
| `server.data_dir` | `REMOVARR_DATA_DIR` | `/config` | exists, writable, outside every root and no root inside it |
| `server.log_level` | `REMOVARR_LOG_LEVEL` | `info` | `debug`/`info`/`warn`/`error` |
| `server.trusted_proxies` | `REMOVARR_TRUSTED_PROXIES` | empty | CIDRs (`ipnet`) |
| `server.shutdown_grace_seconds` | `REMOVARR_SHUTDOWN_GRACE_SECONDS` | 45 | 5..=300 |
| `roots.completed` | `REMOVARR_COMPLETED_ROOT` | **required** | absolute, exists, dir, `canonicalize(p) == p` (no symlink component) |
| `roots.incomplete` | `REMOVARR_INCOMPLETE_ROOT` | none (incomplete features "not configured") | same |
| `roots.media` | `REMOVARR_MEDIA_ROOTS` | **required**, >= 1 | same; all roots distinct and **non-nested** (any nesting = refuse to start) |
| `path_mappings.qbittorrent` | `REMOVARR_QBIT_PATH_MAP` | empty | entries `service_prefix=app_path`; both absolute; no `.`/`..` components; no two equal prefixes; app path inside a declared root or an ancestor of roots |
| `path_mappings.sonarr` / `.radarr` / `.jellyfin` | `REMOVARR_SONARR_PATH_MAP`, `REMOVARR_RADARR_PATH_MAP`, `REMOVARR_JELLYFIN_PATH_MAP` | empty (= identity is **not** assumed) | same |
| `owner.expected` | `REMOVARR_EXPECTED_OWNER` | process effective `uid:gid` (logged at startup, OQ-09) | `u32:u32` |
| `thresholds.incomplete_stalled_days` | `REMOVARR_INCOMPLETE_STALLED_DAYS` | 14 | 1..=3650 |
| `thresholds.auto_min_age_hours` | `REMOVARR_AUTO_MIN_AGE_HOURS` | 48 | 0..=8760 |
| `thresholds.hide_recent_days` | `REMOVARR_HIDE_RECENT_DAYS` | 2 | 0..=365 (UI filter default) |
| `thresholds.recheck_timeout_seconds` | `REMOVARR_RECHECK_TIMEOUT_SECONDS` | 600 | 30..=7200 |
| `thresholds.plan_ttl_hours` | `REMOVARR_PLAN_TTL_HOURS` | 24 | 1..=168 |
| `files.video_extensions` | `REMOVARR_VIDEO_EXTENSIONS` | `.mkv,.mp4,.avi,.m4v,.ts,.m2ts,.wmv` (OQ-13) | lower-cased, leading dot |
| `files.junk_extensions` | `REMOVARR_JUNK_EXTENSIONS` | `.nfo,.txt,.sfv,.md5,.url,.rar,.zip,.7z,.par2,.jpg,.jpeg,.png` + `.r00`-`.r99` pattern (OQ-02) | same; never intersects video extensions |
| `files.subtitle_extensions` | `REMOVARR_SUBTITLE_EXTENSIONS` | `.srt,.ass,.ssa,.sub,.idx,.vtt` | same |
| `scan.interval_minutes` | `REMOVARR_SCAN_INTERVAL_MINUTES` | 1440 (0 = manual only) | 0 or 60..=10080 |
| `scan.anchor_utc` | `REMOVARR_SCAN_ANCHOR_UTC` | `03:00` | `HH:MM`; next run = anchor + k·interval > now (UTC) |
| `scan.qbit_refresh_minutes` | `REMOVARR_QBIT_REFRESH_MINUTES` | 60 (0 = off) | 0 or 5..=1440; API-only refresh + reclassification, no disk I/O (CP1-F07) |
| `limits.content_read_concurrency` | `REMOVARR_CONTENT_READ_CONCURRENCY` | 1 | 1..=4 |
| `limits.ffprobe_concurrency` | `REMOVARR_FFPROBE_CONCURRENCY` | 1 | 1..=4 |
| `limits.http_concurrency_per_service` | `REMOVARR_HTTP_CONCURRENCY` | 4 | 1..=16 |
| `limits.max_response_bytes` | `REMOVARR_MAX_RESPONSE_BYTES` | 33554432 (32 MiB) | 1 MiB..=512 MiB (CP1-F17) |
| `matching.similarity_threshold` | `REMOVARR_SIMILARITY_THRESHOLD` | 0.60 | 0..=1 (identification only) |
| `matching.sample_size_bytes` / `sample_count` | `REMOVARR_SAMPLE_SIZE_BYTES` / `REMOVARR_SAMPLE_COUNT` | 1048576 / 5 | > 0 |
| `matching.compare_chunk_bytes` | `REMOVARR_COMPARE_CHUNK_BYTES` | 16777216 | >= 65536 |
| `matching.duration_tolerance_seconds` / `_ratio` | `REMOVARR_DURATION_TOLERANCE_SECONDS` / `_RATIO` | 5.0 / 0.002 | >= 0 |
| `matching.ffprobe_path` | `REMOVARR_FFPROBE_PATH` | `ffprobe` (resolved on `PATH`) | executable |
| `services.qbittorrent.url` | `REMOVARR_QBIT_URL` | none (= not configured) | `http`/`https` URL, no userinfo |
| `services.qbittorrent.username` | `REMOVARR_QBIT_USERNAME` | `admin` | non-empty |
| — | `REMOVARR_QBIT_PASSWORD` | secret | required if URL set |
| `services.qbittorrent.external_prefixes` | `REMOVARR_QBIT_EXTERNAL_PREFIXES` | empty | container prefixes declared outside every root (OQ-07) |
| `services.sonarr.url` / `services.radarr.url` | `REMOVARR_SONARR_URL` / `REMOVARR_RADARR_URL` | none | URL |
| — | `REMOVARR_SONARR_API_KEY` / `REMOVARR_RADARR_API_KEY` | secret | required if URL set |
| `services.jellyfin.url` | `REMOVARR_JELLYFIN_URL` | none | URL |
| — | `REMOVARR_JELLYFIN_API_KEY` | secret | required if URL set |
| `mode.dry_run` | `REMOVARR_DRY_RUN` | **true** | bool |
| `mode.auto_mode` | `REMOVARR_AUTO_MODE` | **false** | bool |
| `capabilities.*` | `REMOVARR_CAP_<NAME>` | see section 13.2 | bool |
| `auth.username` | `REMOVARR_USERNAME` | `admin` | non-empty |
| — | `REMOVARR_PASSWORD_HASH` | secret, **required** | Argon2 PHC string (`$argon2id$...`; `$` is doubled as `$$` in compose files) |
| — | `REMOVARR_SESSION_KEY` | secret, **required** | >= 64 characters (for example `openssl rand -hex 64`) |
| `auth.session_ttl_hours` | `REMOVARR_SESSION_TTL_HOURS` | 12 | 1..=720 |
| `auth.cookie_secure` | `REMOVARR_COOKIE_SECURE` | true | bool (set false only for plain-HTTP LAN) |
| `auth.login_limit_per_ip` / `_per_ip_user` | `REMOVARR_LOGIN_LIMIT_PER_IP` / `_PER_IP_USER` | 20 / 5 per 15 min | 1..=1000 |

qBittorrent's `download_path` (temp folder) must be covered by `path_mappings.qbittorrent`
(normally onto `<incomplete_root>`). If a torrent's `download_path` maps to `NoRule`, the torrent is
an `unmapped_torrent` like any other unmapped path (CP1-F03).

Startup validation failures print every missing or invalid **key name** (never values) and exit
with code 2 (FH-01). Additional root checks: a root must not be the filesystem root `/`, must not
contain the data dir, and the scanner never descends into a directory named `BT_backup`, nor one
holding `*.fastresume` files (`ProtectedZone::QbitInternal`, INV-07). No root path is assumed: an
unset root key is a startup error (completed, media) or a "not configured" feature (incomplete).

Path mapping algorithm (05, used by all connectors): lexically normalize the service path
(reject relative paths and `.`/`..`; collapse duplicate `/`), take the longest rule prefix on
**component boundaries**, and replace only that prefix. Then classify: inside a root ->
`Mapped(RootedPath)`; matches `external_prefixes` -> `DeclaredExternal`; mapped but outside all
roots -> `OutsideRoots`; no rule -> `NoRule`; invalid -> `Invalid`. Reverse mapping
(`to_service`) uses the same rules. Physical confinement is then enforced by `removarr-fs`
(directory handles, no-follow). `Config::fingerprint()` = sha256 of the canonical non-secret
config, stored in plans (`Precondition::ConfigFingerprint`).

Example files (05): `examples/removarr.toml` (every key, comments) and `.env.example` (secret
names and empty values). From wave 13 on, `.env.example` is owned by 20.

---

## 12. Web layer (crate `removarr-web`)

### 12.1 Routes

All `GET` routes are read-only. All `POST` routes require a session (except `/login`) **and**
CSRF. A request with header `HX-Request: true` gets the fragment template. Otherwise the full
page is rendered. Every action works without JavaScript (forms and redirects); HTMX only enhances.

| Method | Path | Purpose | View model |
|---|---|---|---|
| GET | `/healthz` | liveness (no DB, no roots); public | — |
| GET | `/readyz` | DB `SELECT 1`, config loaded; public | — |
| GET | `/static/{file}` | embedded assets (`htmx-<ver>.min.js`, `app.css`), immutable cache; public | — |
| GET/POST | `/login` | form, verify | `LoginVm` |
| POST | `/logout` | end session | — |
| POST | `/locale` | set the locale cookie (`en`/`fr`) | — |
| GET | `/` | dashboard | `DashboardVm` |
| GET | `/issues/{category}` | tab list (`q`, `sort=name\|size\|age`, `dir`, `hide_recent`, `kind=all\|folder\|file`, `page`, `per_page<=200`) | `IssueListVm` |
| GET | `/issues/{category}/{issue_id}` | detail (evidence, blockers, paths of the inode, torrents, ARR snapshot, ffprobe) | `IssueDetailVm` |
| GET | `/ignored` | ignored items | `IssueListVm` (ignored mode) |
| POST | `/issues/selection-summary` | read-only: count + bytes of `issue_ids[]` (no state change; POST only because selections can be large) | fragment |
| POST | `/issues/ignore`, `/issues/restore` | `subjects[]`, `reason` | redirect |
| POST | `/issues/evaluate` | enqueue `Evaluate` for `issue_ids[]` | redirect to job |
| POST | `/issues/verify-content` | enqueue `VerifyContent` | redirect to job |
| POST | `/plans` | build a plan from `category`, `action`, `issue_ids[]` | redirect to `/plans/{id}` |
| GET | `/plans`, `/plans/{id}` | plan list, preview (actions, excluded + blockers, gains, annex preview, dry-run results) | `PlanListVm`, `PlanPreviewVm` |
| GET | `/plans/{id}/confirm` | confirmation modal fragment | `ConfirmVm` |
| POST | `/plans/{id}/approve` | `plan_hash`, `confirmed_count`, `confirmed_torrent_removals`, `acknowledged_samples[]`, `acknowledged_unrecoverable[]` (CP1-F20, F25) | redirect |
| POST | `/plans/{id}/execute` | enqueue `ExecutePlan { simulate: false }` (approved, not expired, live plan in a live process) | redirect to job |
| POST | `/plans/{id}/simulate` | enqueue `ExecutePlan { simulate: true }` (any draft or approved plan, any process; CP1-F14e) | redirect to job |
| POST | `/plans/{id}/discard` | discard a draft or approved plan | redirect |
| GET | `/jobs`, `/jobs/{id}`, `/jobs/{id}/progress` | job list, detail, progress fragment (memory only) | `JobVm` |
| POST | `/jobs/{id}/cancel` | cooperative cancel | redirect |
| POST | `/scan` | enqueue `Scan` | redirect to job |
| POST | `/services/{qbittorrent\|sonarr\|radarr\|jellyfin}/refresh` | enqueue `RefreshService` (read-only, allowed in dry-run) | redirect to job |
| POST | `/services/{sonarr\|radarr\|jellyfin}/rescan` | builds a one-action `ServiceRescan` plan (journaled like any plan; executable only live) | redirect to `/plans/{id}` |
| GET | `/media/{service}/{media_id}` | media detail: ARR identity, expected episodes, files and inodes, quality snapshot, last search results, monitoring (CP1-F05, F06) | `MediaDetailVm` |
| POST | `/media/{service}/{media_id}/evaluate` | enqueue `Evaluate { media, search }` (`search=true` runs an interactive search: read-only, never grabs) | redirect to job |
| POST | `/plans` (variants) | `PlanRequest::Media` (set monitored) and `PlanRequest::Release` (grab a listed result) use the same `/plans` endpoint with a `kind` field | redirect to `/plans/{id}` |
| POST | `/recovery/retry` | re-enqueue `Recover` after the automatic retries are exhausted (CP1-F13) | redirect to job |
| POST | `/auto/resume` | clear `auto_paused` | redirect |
| GET | `/history` | journal and action outcomes | `HistoryVm` |
| GET | `/stats`, `/stats/chart/{kind}.svg` | gains over time (potential, expected, attributable, observed) | `StatsVm` |
| GET | `/logs` | ring buffer (`level`, `q`) | `LogsVm` |
| GET | `/explorer` | search by path, title, episode, inode or hash (CTX-01) | `ExplorerVm` |
| GET | `/settings` | effective config (redacted), mode flags, capabilities, service diagnostics | `SettingsVm` |
| GET | `/export/{session}/{json\|csv\|md}` | streamed export (CTX-09) | — |

### 12.2 Main view models (data from `reporting`, presentation fields added by `web`)

- `DashboardVm { urgency: Urgency { Green, Orange, Red } + reason keys, counts_by_category: Vec<(IssueCategory, u64, Severity)>, potential_bytes (unique inodes), hardlink_shared_bytes, roots: Vec<RootUsageVm { capacity, used, free, logical, allocated }>, services: Vec<ServiceVm { kind, state, version, observed_at }>, last_scan: Option<SessionVm { finished_at, status, age, stale: bool }>, running_jobs: Vec<JobVm>, torrents_left_stopped: u64, arr_queues: Vec<ArrQueueVm { service, total, warnings, errors, stale_candidates }> (CP1-F05, CTX-02), problem_torrents: u64 (non-allowlisted states holding content), recovery_pending: bool, auto: AutoVm { enabled, paused_reason }, dry_run: bool }`.
  Urgency is Red if there is any Critical issue, the scan is incomplete or older than twice the
  interval, a configured service is not Ok, torrents are left stopped, recovery is pending, or
  auto is paused. It is
  Orange if there is any Warning, and Green otherwise. It is always rendered with an icon and a
  text label (never color alone).
- `IssueListVm { category, rows: Vec<IssueRowVm>, total_count, total_bytes, page, per_page, sort, dir, filters, selectable: bool }`.
- `IssueRowVm { id, subject, name, kind: Folder|File|Torrent|Pair, age_days, size_bytes, torrent_count, severity, outcome, blocker_count, proof: ProofBadge }`.
  Selection totals are computed server-side on submit and also, when HTMX is on, by an
  `hx-post` to `/issues/selection-summary` (read-only `POST`, CSRF-protected, returns
  count + bytes).
- `IssueDetailVm { issue, entries (all paths of each inode, physical copy vs hardlink), torrents, arr: Option<ArrSnapshotVm>, probe: Option<ProbeVm>, blockers, available_actions }`.
- `PlanPreviewVm { id, hash, mode, status, expires_at, actions: Vec<ActionVm { kind, targets, annexes, torrents, expected_gain, verification }>, excluded: Vec<(subject, blockers)>, gains: PlanGains, irreversible_effects: Vec<key>, requires_typed_count: bool, samples_to_acknowledge, dry_run_journal: Option<Vec<StepVm>> }`.
- `ConfirmVm { plan_id, hash, target_count, bytes_impact, irreversible_effects, samples, typed_count_required, torrent_removals: Vec<(hash, name)>, torrent_removal_count }`.
  A typed count is required whenever any action has no verified replacement (DR-35). Torrent task
  removals are listed separately and need their own typed count (CP1-F12).
- `MediaDetailVm { media, identity, expected_episodes: Vec<ArrEpisode>, files (with inodes and hardlink state), snapshot: Option<ArrSnapshotVm>, search: Option<Vec<ReleaseSearchResult>> (sorted, with indexer, seeders, size, CF scores, rejections), monitored, available_actions }`.
- `JobVm { id, kind, status, stage, counters, started_at, finished_at, cancellable }`, `HistoryVm`
  (rows: time, plan, action, target, reason, outcome, gain id, attributable bytes), `StatsVm` (four
  series plus SVG), `LogsVm`, `ExplorerVm`, `SettingsVm`, `LoginVm { error_key, retry_after }`.

### 12.3 Security layers (17)

- Session: `axum_extra::extract::PrivateCookieJar` (AES-GCM, key from `REMOVARR_SESSION_KEY`).
  The cookie `removarr_session` holds a random 32-byte session id and is `HttpOnly`,
  `SameSite=Strict`, `Path=/`, `Secure` per config. The server holds a bounded in-memory session
  table (max 32 entries, TTL `session_ttl_hours`, idle 2 h), so a restart logs everyone out
  (ADR-0006). The id is rotated on login. Logout and expiry remove the entry.
- CSRF: a per-session synchronizer token (32 random bytes), sent as the hidden field
  `csrf_token` and the `X-CSRF-Token` header (set via `hx-headers` on `<body>`). `/login` uses a
  pre-session double-submit cookie. Every `POST` additionally checks `Origin`/`Referer` against
  the request host.
- Login rate limit: bounded in-memory fixed windows keyed by client IP and by (IP, normalized
  username), checked **before** Argon2. Every failure returns a generic error. `429` comes with
  `Retry-After`. Client IP is taken from `X-Forwarded-For` only when the peer is in
  `trusted_proxies`. Argon2 runs in `spawn_blocking` behind a 1-permit semaphore.
- Headers (`tower-http` `set-header`): `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, `X-Frame-Options: DENY`.
  HTMX config: `<meta name="htmx-config" content='{"allowEval":false,"includeIndicatorStyles":false}'>`
  and no `hx-on` attributes.
- Limits: body 1 MiB, request timeout 30 s, `catch-panic` returns a generic 500.

### 12.4 Templates, assets, i18n (19)

- Askama templates in `crates/web/templates/` (`base.html`, `pages/*.html`, `fragments/*.html`).
  All dynamic text uses Askama's default HTML escaping. Absolute host paths are never rendered.
  Paths are shown as root label + relative path.
- Charts: server-side SVG built from `reporting` series (geometry function in `reporting::svg`,
  markup in a template).
- `locales/en.json` and `locales/fr.json` are embedded with `include_str!`, flat dotted keys,
  with English fallback per key. The locale comes from a `removarr_locale` cookie (not part of
  the session), with `Accept-Language` as the default.
- **HTMX is pinned to exactly `htmx.org@2.0.4`** (CP1-F15). The file
  `crates/web/static/vendor/htmx-2.0.4.min.js` (from `dist/htmx.min.js` of that npm version) is
  embedded with `include_bytes!` and served at `/static/htmx-2.0.4.min.js`. Checksum handling:
  - 19 downloads it once (orchestrator-approved pinned download). It cross-checks it against two
    independently published digests: the per-file hash in the jsDelivr package metadata for
    `htmx.org@2.0.4`, and the `sha384` SRI value in the official htmx 2.0.4 installation docs.
  - 19 records the SHA-256 (hex) and the SHA-384 SRI in `crates/web/static/vendor/README.md`, and
    the SHA-256 in a constant `web::assets::HTMX_SHA256`.
  - A unit test hashes the embedded bytes and compares them to the constant.
  - The `<script>` tag carries the `integrity` attribute.

  Presence and checksum are a release gate (27). A missing or mismatched file **fails the
  build/tests**: there is no "ship without HTMX" option. Progressive enhancement stays a
  *degradation property*: every action also works as a plain form post when JavaScript is off.

---

## 13. Risk tiers and capability flags

### 13.1 Tier mapping (MASTER §10, CONTEXTE §7.2)

| Tier | Scope | Action kinds (`ActionKind`) | Components that must complete the tier | Capability flags | Required tests before the next tier (CP2) |
|---|---|---|---|---|---|
| 1 Read-only | scan, issues, evaluate, reports, plans, full dry-run path | none mutating; dry-run simulation of every kind | 06-11, 12 (plans in dry-run), 13 (recording mutators, revalidation), 14, 15 | — | parity inventory (§7.5), dry-run-no-mutation with counting fakes |
| 2 Exact-copy consolidation | permission probe, full compare, temp hardlink, atomic replace, stop/recheck/resume | `ConsolidateHardlink`; manual commands `ArrRescan`, `JellyfinRefresh` | 12, 13, 22 | `consolidate` | crash after each intent and each mutation (probe, stop, replace, recheck, start), T-11..T-14, T-22, V6, V7, dry-run-no-mutation |
| 3 Deletions under completed | unlinked files, folders, junk outside torrents, stalled incomplete, task removal (`deleteFiles=false`), empty parents, targeted chmod | `DeleteCompletedFile`, `DeleteCompletedFolder`, `DeleteJunk`, `RemoveEmptyParent`, `RemoveTorrentTask`, `CleanupStalledIncomplete`, `DeleteIncompleteOrphan`, `FixMode` | 12, 13, 22 | `delete_completed`, `cleanup_incomplete`, `fix_permissions`, `auto_remove_empty_torrents` (policy, auto only) | crash points per step, T-15..T-17, T-23, T-32, sample protection, partial torrent, fail-closed V2-V4, dry-run-no-mutation |
| 4 ARR replacement | quality snapshots, coverage proof, ffprobe checks, numbering, approved grab, queue/monitoring commands | `ReplaceRelease` (+ `DeleteMediaFile` step), `ArrGrabRelease`, `ArrRemoveQueueItem`, `ArrSetMonitored` | 09, 12, 13, 22 | `arr_manual_import`, `arr_grab`, `arr_queue_remove`, `arr_set_monitored` | ARR score change invalidates the plan, rejected candidate never selected, coverage (incl. episodes without files, specials, absolute numbering), ffprobe blockers, `importMode=copy` asserted, "move would be detected", `OldCoverageRecoverable` (other-link case, recycle-bin case, unrecoverable -> blocked unless acknowledged, hardlinks-disabled warning, `OldFileAccounted` results), search never grabs, crash points, dry-run-no-mutation. **CP2-tier4 must verify CP1-F20.** |
| 5 Automatic mode | auto cycle with every auto rule | auto plans of `ConsolidateHardlink`, `DeleteCompletedFile`, `DeleteCompletedFolder`, `RemoveEmptyParent`, `RemoveTorrentTask` only | 12 (`plans::auto`), 14, 22 | `mode.auto_mode` (default false) | T-20, T-21, T-31, stop-at-first-failure, fail-closed on every incomplete input, dry-run auto simulation |

### 13.2 Capability flags (`[capabilities]`, env `REMOVARR_CAP_<NAME>`)

| Flag | Default | Effect |
|---|---|---|
| `consolidate` | true | allow planning and executing `ConsolidateHardlink` (live still requires `dry_run = false` and approval) |
| `delete_completed` | true | tier-3 deletions under completed (manual approval) |
| `cleanup_incomplete` | true | `CleanupStalledIncomplete`, `DeleteIncompleteOrphan` (manual only) |
| `fix_permissions` | true | `FixMode` (manual only) |
| `arr_manual_import` | **false** | ARR manual import (required by `ReplaceRelease`) |
| `arr_queue_remove` | **false** | remove ARR queue items (keep media) |
| `arr_set_monitored` | **false** | change ARR monitoring |
| `arr_grab` | **false** | `ArrGrabRelease` from a listed search result. Manual approval only, never automatic, live only (CP1-F06; MASTER §6, §9) |
| `auto_remove_empty_torrents` | **false** | auto plans may include `RemoveTorrentTask` (CON-04) |

A search (`ArrRead::search_releases`) never grabs. A grab exists only as an approved
`ArrGrabRelease` plan.

`ActionKind::capability() -> Option<Capability>`: `ConsolidateHardlink` -> `consolidate`; the five
completed deletions -> `delete_completed`; `CleanupStalledIncomplete`/`DeleteIncompleteOrphan` ->
`cleanup_incomplete`; `FixMode` -> `fix_permissions`; `ReplaceRelease` -> `arr_manual_import`;
`ArrRemoveQueueItem` -> `arr_queue_remove`; `ArrSetMonitored` -> `arr_set_monitored`;
`ArrGrabRelease` -> `arr_grab`; `ArrRescan`,
`JellyfinRefresh` -> `None` (always allowed in live mode, never in dry-run). `DeleteMediaFile` is
an `ActionKind` (tier 4, capability `arr_manual_import`) that is valid **only** inside a
`ReplaceRelease` plan, with `depends_on` pointing to the verified import action. The plan engine
rejects it anywhere else.

### 13.3 Automatic rules (12, `plans::auto`)

An auto plan contains an action only if **all** of these hold:
A1 `auto_mode` is on (live only if `dry_run = false`; otherwise it is simulated) and `auto_paused`
is unset.
A2 The latest session is `Complete` with **zero** scan errors and zero `MountBoundary`/`NonUtf8Name`
protected zones in every root the action touches. `QbitInternal` zones do not count (section 7.1
step 8), but no unit containing one is ever planned (CP1-F19). qBittorrent is `Ok` with zero `NoRule`/`Invalid` torrents. Every service the
action depends on is `Ok` (ARR for deletions). This is the whole-root perimeter (OQ-05).
A3 Every entry of the unit has age >= `auto_min_age_hours` (newest mtime; consolidations included, CON-21).
A4 It is not ignored, not a sample, not involved in a failed or unresolved consolidation, and the
same subject has no failed action in the last 7 days.
A5 Every torrent involved is allowlisted with progress 1.
A6 Consolidation: same device, target dir writable per scan, name+size or semantic+sampled
candidate. The full compare at execution is mandatory.
A7 Deletion: under `<completed_root>` only. The media is identified unambiguously by ARR, and
ARR's manual-import preview rejects the completed file as not an upgrade (`ArrNotUpgradeEvidence`).
**The replacement is physically present** (CP1-F02). For every media id of the unit, ARR's current
file path:
- maps (`Mapped`) into a media root;
- exists in the latest complete scan as a regular file;
- has a size equal to ARR's file size;
- has a `PhysicalId` that differs from every deletion target.

Each replacement file is an `EntryUnchanged { compare_nlink: false }` precondition, re-`stat`ed
before the first mutation. Any ARR/disk mismatch gives `ArrAmbiguous`, which means a manual
decision only. Coverage is correct. A folder unit is all-or-nothing, with no sample, no protected
zone and no ambiguous member.
A8 `RemoveTorrentTask` only if `auto_remove_empty_torrents` is on and every wanted file is gone.
A9 It is never one of: media-root deletion, incomplete cleanup, `FixMode`, `ReplaceRelease`,
`ArrGrabRelease`, ARR or Jellyfin commands, sample deletion.

---

## 14. Resource budgets (for `23-e2e-perf`)

Reference fixture: 20,000 regular files across the three roots (with realistic hardlink ratio:
60 % of completed videos hardlinked into media), 3,000 fake torrents with files on a loopback
qBittorrent fake, a fake Sonarr/Radarr with 2,000 media items, temp dir on local SSD or tmpfs,
release build, warm page cache unless stated.

| ID | Budget | Measurement |
|---|---|---|
| B1 | RSS idle <= **40 MiB** | 60 s after a completed scan of the fixture, no jobs, no HTTP traffic |
| B2 | RSS peak <= **160 MiB** during a full `Scan` of the fixture | sample `/proc/self/status` VmHWM or `ps` every 100 ms |
| B3 | CPU idle <= **1 %** of one core averaged over 5 min; **zero** filesystem syscalls on roots while idle | after the scan, rename the roots away: idle plus loading every GET page must log no fs error and leave the roots untouched |
| B4 | Full `Scan` of the fixture <= **60 s**; fs walk + inventory persistence <= **15 s**; unchanged rescan <= **30 s** | job metrics |
| B5 | DB writes: initial scan <= **64 MiB**; unchanged rescan <= **4 MiB** (db + wal growth) | file sizes before and after, `wal_checkpoint(TRUNCATE)` between runs |
| B6 | Content reads during a scheduled scan: 0 full compares; sample reads <= `sample_count × sample_size` per retained pair. `RefreshService` (manual or scheduled) and every UI request: **zero** filesystem calls and zero content reads (CP1-F07) | counting `FsInventory`/`ContentReader` wrappers |
| B7 | Page latency p95 <= **100 ms** (dashboard, issue list 50 rows, 5,000 issues); HTMX fragments p95 <= **50 ms** | 200 sequential requests on loopback |
| B8 | Binary (musl, stripped) <= **30 MiB**; runtime image <= **250 MiB** uncompressed | `ls -l`, `docker image inspect` |
| B9 | Startup to `/readyz` OK <= **3 s** with an empty DB | timestamp diff |
| B10 | INFO log lines per scheduled scan <= **200**; the UI log buffer never exceeds 2,000 events | count stdout lines |
| B11 | Writes inside roots: **zero** outside approved live plan execution (dry-run included) | inode/mtime snapshot of the fixture before and after |

A missed budget blocks the release unless 27 records an explicit, justified waiver. Data-loss
budgets (B3 roots untouched, B11) can never be waived.

---

## 15. Module ownership (parallel safety)

### 15.1 Rules

1. **Exclusive paths.** An agent creates and edits only the paths listed for it. Paths owned by
   a finished earlier wave may be edited by a later single-agent wave **only** where the table
   says "may edit". The one allowed exception everywhere: adding a single `pub mod x;` line to a
   `lib.rs`/`mod.rs` owned by an earlier wave, when `x` is in the agent's ownership.
2. **Root `Cargo.toml`** (workspace members glob, `[workspace.package]`,
   `[workspace.dependencies]`, `[workspace.lints]`, profiles) is owned by **02**. 02 declares
   **every** dependency in section 3 up front.
   - Single-agent waves (2, 3, 6-12, 14-16, 20): the agent may add a new entry to
     `[workspace.dependencies]` only if it is unavoidable, with the justification recorded in its
     handoff.
   - Parallel waves (4, 5, 13): **no one edits the root `Cargo.toml`**. If a new third-party dep is
     unavoidable, add it in the agent's own crate `Cargo.toml` with an explicit version, and record
     it in the handoff. The orchestrator may hoist it later.
   - `Cargo.lock` is committed. Cargo serializes concurrent updates, and no agent edits it by hand.
3. **Crate `Cargo.toml`** files belong to the crate owner (first listed). Later single-wave
   co-owners of the same crate may add `workspace = true` deps.
4. **Domain is read-only after wave 3.** If a later agent needs a domain change, it uses a
   crate-local type, records the need in its handoff, and the orchestrator re-runs 03 for a
   targeted patch between waves. 03 must implement **all** of section 4 and section 5, including
   the connector DTOs, so wave 5 does not need domain edits.
5. **Config is read-only after wave 4.** 05 implements the whole of section 11, including every
   service section, `LiveMode` and the capabilities.
6. Docs: each agent writes only `docs/handoffs/<its file>.md` plus the doc paths listed below.
   `docs/architecture.md` and `docs/adr/` belong to 01. `docs/PROGRESS.md` and `docs/reviews/`
   belong to the orchestrator.
7. **Parity tests** (CP1-F16): every crate glob in the table (`crates/X/**`) **excludes**
   `crates/X/tests/parity_*.rs`, which belongs to 21. Between waves (never during a parallel
   wave), 21 may add `[dev-dependencies]` entries with `workspace = true` to any crate's
   `Cargo.toml`, and nothing else in that file.
8. "Creates / owns" lists only paths an agent creates first. A later agent that takes over or
   edits an existing path lists it under "May edit". No path appears under "Creates / owns" for
   two agents.

### 15.2 Ownership table

| Agent | Wave | Creates / owns | May edit (later, sequential) | Must not touch |
|---|---|---|---|---|
| 00 legacy-audit | 0 | `docs/legacy-parity.md` | — | code |
| 01 system-arch | 1 | `docs/architecture.md`, `docs/adr/**` | — | code |
| 02 rust-scaffold | 2 | `/Cargo.toml`, `/Cargo.lock` (initial), `/rustfmt.toml`, `/clippy.toml`, `/.gitignore` (append), every `crates/<name>/Cargo.toml` + minimal `src/lib.rs` (or `main.rs`) for **all** crates in section 2.1 except `integration-tests` and `e2e-perf`; `crates/testkit/src/{lib.rs,clock.rs,ids.rs,fixture.rs,fake_qbit.rs,fake_arr.rs,fake_jellyfin.rs}` as empty stubs with `pub mod` lines; `docs/development.md` | — | domain logic, connector behavior |
| 03 domain-model | 3 | `crates/domain/**`; `crates/testkit/src/clock.rs`, `crates/testkit/src/ids.rs` | — | other crates |
| 04 db-layer | 4 | `crates/storage/**` (including `migrations/`) | — | `crates/config`, domain |
| 05 config-secrets | 4 | `crates/config/**`, `examples/removarr.toml`, `.env.example` (until wave 13) | — | `crates/storage`, domain |
| 06 fs-engine | 5 | `crates/fs/**` **except** `crates/fs/src/mutate.rs` and `crates/fs/tests/mutate_*.rs`; `crates/testkit/src/fixture.rs` | — | `crates/qbit`, `crates/arr`, `crates/jellyfin`, other testkit files, root `Cargo.toml`, `crates/testkit/Cargo.toml` |
| 08 qbit-api | 5 | `crates/qbit/**`, `crates/testkit/src/fake_qbit.rs` | — | same exclusions mutatis mutandis |
| 09 arr-api | 5 | `crates/arr/**`, `crates/testkit/src/fake_arr.rs` | — | same |
| 10 jellyfin-api | 5 | `crates/jellyfin/**`, `crates/testkit/src/fake_jellyfin.rs` | — | same |
| 07 media-match | 6 | `crates/matching/**` | `crates/testkit/src/fixture.rs` (add matching helpers only) | fs, connectors |
| 11 issue-engine | 7 | `crates/workflows/Cargo.toml`, `crates/workflows/src/lib.rs`, `crates/workflows/src/observe/**`, `crates/workflows/src/issues/**`, `crates/workflows/tests/{observe,issues}_*.rs` | — | `plans`, `actions` dirs |
| 12 plan-engine | 8 | `crates/workflows/src/evaluate/**`, `crates/workflows/src/plans/**`, `crates/workflows/tests/{evaluate,plans}_*.rs` | `crates/workflows/Cargo.toml` (deps), `crates/workflows/src/lib.rs` (mod lines) | `actions` |
| 13 action-runner | 9 | `crates/workflows/src/actions/**` (incl. `dry_run`, `verify`, `recovery`, `IMPLEMENTED_TIER`), `crates/workflows/tests/actions_*.rs`, `crates/fs/src/mutate.rs`, `crates/fs/tests/mutate_*.rs` | `crates/workflows/Cargo.toml`, `crates/fs/Cargo.toml` (deps), `lib.rs` mod lines; may fix bugs in connector mutating methods only via the owner's handoff (orchestrator decides) | observe, issues, plans logic |
| 14 job-runner | 10 | `crates/jobs/**` | — | workflows internals |
| 15 reporting | 11 | `crates/reporting/**` | — | storage schema (request changes via handoff) |
| 16 backend-api | 12 | `crates/web/Cargo.toml`, `crates/web/src/{lib.rs,router.rs,state.rs,error.rs,forms.rs,health.rs,i18n.rs}`, `crates/web/src/handlers/**`, `crates/web/templates/**` (minimal functional templates, including `pages/login.html`), `locales/en.json`, `locales/fr.json` (initial keys), `crates/app/**` (wiring, startup, CLI `serve`/`healthcheck`), `crates/app/tests/**` | — | auth module, connectors |
| 17 auth-guard | 13 | `crates/web/src/auth/**`, `crates/web/tests/auth_*.rs` | `crates/web/templates/pages/login.html` (created minimal by 16), `crates/web/Cargo.toml`, `crates/web/src/router.rs` (layer wiring only), `crates/web/src/lib.rs`, `crates/web/src/state.rs` (auth state field), `crates/app/src/**` (auth wiring, `hash-password` subcommand) | templates other than login, `locales/`, Docker/CI files |
| 18 ux-design | 13 | `docs/ux/**` | — | any code, `README.md` |
| 20 devops | 13 | `Dockerfile`, `.dockerignore`, `docker-compose.yml`, `.github/**` (workflows, issue templates), `README.md`, `.env.example` (from wave 13), `deny.toml`, `docs/operations.md` | — | `crates/**` (request changes via handoff) |
| 19 web-ui | 14 | `crates/web/static/**` (including the pinned HTMX file), `crates/web/src/assets.rs` (`web::assets`, `HTMX_SHA256`), `crates/web/src/views/**`, `crates/web/tests/ui_*.rs` | takes over from 16: **all** of `crates/web/templates/**` (for `pages/login.html`: markup and styling only, never its auth logic), `crates/web/src/i18n.rs`, `locales/**`; also `crates/web/src/handlers/**` (rendering and view-model wiring only), `crates/web/src/lib.rs`, `crates/web/Cargo.toml` (dev-deps such as `sha2` for the asset test) | auth logic, routes semantics |
| 21 unit-tests | between waves, from 3 | `crates/*/tests/parity_*.rs`, `docs/legacy-parity.md` (§2 status and Rust test name columns only) | — | `src/**` of any crate (bugs are handed back to the owner) |
| 22 integration-tests | 15 | `crates/integration-tests/**` | `crates/testkit/src/**` (extend fixture and fakes, no breaking changes) | `src/**` of feature crates |
| 23 e2e-perf | 16 | `crates/e2e-perf/**`, `docs/perf-results.md` | — | `src/**` of feature crates |
| 24 sec-setup | 17 | `.claude/skills/**` (project-scope install) | — | code |
| 25 sec-audit | 18, 19 | `docs/security/**` (or the external path chosen by the orchestrator) | — | code |
| 26 sec-fix | 19 | the files named by confirmed findings (exclusive in its wave) + regression tests next to them | — | audit artifacts |
| 27 release-gate | 20 | `docs/release-readiness.md` | — | code |

Parallel-wave check: wave 4 = {`crates/storage`} vs {`crates/config`, `examples/`, `.env.example`};
wave 5 = {`crates/fs` minus mutate, `testkit/fixture.rs`} vs {`crates/qbit`, `testkit/fake_qbit.rs`}
vs {`crates/arr`, `testkit/fake_arr.rs`} vs {`crates/jellyfin`, `testkit/fake_jellyfin.rs`}; wave 13 =
{`crates/web/src/auth`, login template (edit), router/state/lib/Cargo of web, `crates/app`} vs {`docs/ux`}
vs {Docker, compose, `.github`, `README.md`, `.env.example`, `deny.toml`, `docs/operations.md`}.
All disjoint.

`crates/testkit/Cargo.toml` (02) pre-declares: `removarr-domain`, `tokio`, `axum`, `serde`,
`serde_json`, `tempfile`, `rustix`. Wave-5 agents must not edit it.

### 15.3 Fixture ownership

The parity fixture of `docs/legacy-parity.md` §7: the filesystem tree FX-01..37 and variants V2,
V5, V6, V7 are built by `testkit::fixture` (06). The torrents T_A..T_Q and variants V3, V4, V8 come
from the `testkit::fake_qbit::parity_preset()` (08). ARR and Jellyfin presets live in their fakes
(09, 10). 22 asserts §7.5, §7.6 parity and §7.7 expectations.

Additional variants defined by this architecture (not part of the legacy parity; their expectations
are Rust-only):
- **V9, qBittorrent working files** (CP1-F03). Under `<incomplete_root>`: `Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv.!qB`
  (instead of FX-36), a `.<hash>.parts` file, and a `download_path`-only torrent T_R (`stalledDL`,
  progress 0.2, `save_path` in completed, `download_path` in incomplete) with
  `incomplete/Romeo.Movie/Romeo.Movie.mkv.!qB`. Expected: **no** `incomplete_orphan`, **no**
  `orphan_completed`, `NoTorrentReferences` fails for all three paths, and T_R appears in
  `incomplete_stalled` only when older than the threshold. The tree is built by 06, T_R by 08.
- **V10, ARR replacement** (CP1-F01, F02, F04). A Sonarr season with 3 expected episodes (one
  without a file, one special excluded by default, one absolute-numbered), a manual-import
  preview, and a "move" switch in `fake_arr` that deletes the source on import. It also has a
  `media_management` preset (recycle bin set or unset, hardlinks on or off) and an "upgrade
  deletes old file" behavior. The old media files cover three cases: one still linked from an old
  completed release (`OtherLink`), one recoverable only through the recycle bin, and one
  unrecoverable (must be blocked unless acknowledged; CP1-F20). Built by 09, asserted by 22.

### 15.4 Deviations protocol

An agent that cannot follow this document records in its handoff: the section, what it did
instead, why, and the impact on other agents. CP2 reviewers and 27 check deviations.

### 15.5 ADR index

| ADR | Decision |
|---|---|
| 0001 | Workspace layout, ports in domain, dependency direction |
| 0002 | SQLite via sqlx, single writer with `synchronous=FULL`, intent journal in SQLite |
| 0003 | Dry-run enforced structurally (`LiveMode` token, composition root, plan mode); simulation of any plan with recording mutators (amended CP1) |
| 0004 | Confined filesystem access via directory handles; `RootedPath`; non-UTF-8 fails closed |
| 0005 | Configuration: optional TOML + `REMOVARR_*` env overlay, secrets env-only |
| 0006 | Web: Askama + HTMX progressive enhancement; private-cookie session ids with an in-memory table |
| 0007 | Single execution lane (global serialization of mutations) |
| 0008 | Stalled incomplete cleanup is a manual-only action outside the completed allowlist |
| 0009 | Static musl binary with rustls (ring); no OpenSSL |
| 0010 | ARR is the single importer for release replacements (`importMode="copy"` literal, source inode verified, old coverage must be recoverable before import) and for approved grabs (amended CP1 iterations 1 and 2) |

---

## 16. Resolution of open questions (`docs/legacy-parity.md` §8)

| OQ | Resolution | Owner |
|---|---|---|
| OQ-01 sample rule | `policy::is_sample(rel)`: case-insensitive. True if the file stem, split on `.`, `-`, `_`, space, `[`, `]`, `(`, `)`, contains the token `sample` or `samples`, **or** any ancestor directory component (inside the root) equals `sample` or `samples`. Fixed rule, not configurable: false positives only protect. Only an explicit per-sample acknowledgement in an approved manual plan may delete a sample (INV-10). | 03 (rule), 11, 12 |
| OQ-02 junk list | Defaults in section 11 (`junk_extensions` + `.rNN` parts). Junk = regular file under **`<completed_root>` only**, extension in the list or an orphan subtitle, not a video, not a sample, and **neither its path nor its inode** is held by any torrent. Media roots never have junk (posters and `.nfo` are Jellyfin metadata). | 11 |
| OQ-03 orphan subtitle | Strip trailing tags from the subtitle stem repeatedly (2-3 letter language codes, `forced`, `sdh`, `cc`, `default`, `hi`). The result must equal the stem of a video in the same directory, or, for subtitles in a `Subs`/`Subtitles` subdirectory, of a video in the parent directory. Otherwise it is orphan. | 11 |
| OQ-04 multi-category | One issue per (category, subject). Totals per category. Potential bytes are deduplicated per `PhysicalId` across all categories. A plan dedupes targets. | 11, 15 |
| OQ-05 fail-closed perimeter | **Auto**: any scan error or `MountBoundary`/`NonUtf8Name` protected zone in a root blocks every automatic action touching that root. `QbitInternal` zones do not count for the root, but any unit containing a protected zone of any kind is blocked, manual or auto (same rule as §7.1 step 8, §4.3 and A2; CP1-F19). Any non-Ok required service blocks actions depending on it. **Manual**: block targets whose path, subtree or ancestors carry a scan error (`ScanErrorUnder`). Plans are never built from `Cancelled`/`Interrupted`/`Failed` sessions. qBittorrent `NoRule`/`Invalid` torrents block every torrent-less deletion (orphans, junk, incomplete orphans), manual included, because torrent ownership is then uncertain. qBittorrent `NotConfigured` or `Unavailable` blocks every action that needs torrent ownership (CP1-F10). | 12 |
| OQ-06 stalled incomplete vs allowlist | The allowlist (INV-28) governs actions on **completed** data only. `CleanupStalledIncomplete` is a separate, **manual-only** action type, never available to auto mode (A9). Eligibility: progress < 1, `now - added_on >= incomplete_stalled_days`, state not in {`checkingDL`, `checkingUP`, `checkingResumeData`, `moving`, `allocating`} and not `Unknown`, all files `Mapped` under `<incomplete_root>`. Steps: stop, remove the task with `deleteFiles=false`, revalidate, then the application deletes the data under `<incomplete_root>` (section 7.4). ADR-0008. | 12, 13 |
| OQ-07 torrents outside mappings | `NoRule` (no mapping prefix matches) -> `unmapped_torrent` Critical, which **blocks torrent-less deletions** (OQ-05) until the user adds a mapping or declares the prefix in `qbittorrent.external_prefixes` (then `DeclaredExternal`, Info). Mapped but outside every root -> `OutsideRoots`, Info, no blocker. A prefix match that escapes (`..`, invalid) -> `Invalid`, mapping error, blocks automation (CON-07, V4). | 08 (mapping outcome), 11, 12 |
| OQ-08 torrent-less path sharing an inode | Junk requires that neither path nor inode is torrent-held, so it is not junk. It **is** listed in `orphan_completed` (path-based, matches legacy FX-35) with evidence "inode shared with torrent file X" and attributable gain 0. | 11 |
| OQ-09 expected owner default | Optional. Default = process effective `uid:gid`, logged at startup (the container runs with `user:` aligned with the services, MASTER §8). Permission checks are always on. | 05 |
| OQ-10 min age for auto consolidation | Yes, the same `auto_min_age_hours` (A3). | 12 |
| OQ-11 folder plan with a sample | The release folder is proposed as one unit (CON-23). If it contains a sample, the unit carries blocker `Sample` unless every sample is acknowledged individually at approval (`acknowledged_samples`). Never auto. The user may instead select the non-sample files individually (file-level plan; the sample and its folder stay). | 11, 12, 19 |
| OQ-12 media-root deletions | The application never deletes in `media_roots` automatically. Manually, only the `DeleteMediaFile` step inside an approved `ReplaceRelease` plan, after inode-proven import and ARR evidence that the file is no longer ARR's current file. Consolidation's atomic rename replaces a name with identical content and is not a deletion. | 12, 13 |
| OQ-13 video extensions | `.mkv,.mp4,.avi,.m4v,.ts,.m2ts,.wmv`. The parity fixture overrides with `.mkv`. | 05 |
| OQ-14 reverse restore (CTX-04) | Not in V1. `torrent_missing_files` issues show an informational hint when a media file with the expected size exists. It is recorded as later work. | 11 |
| OQ-15 symlinks | Informational category `symlink`, never actionable, never followed. FX-24 never counts as a hardlink. | 11 |

Fixture note: §7.7 "3 proven pairs" for `duplicate` holds after a `VerifyContent` job (scans stop
at sample level). Before that, the pairs show proof `Sampled` or `NameSize`.

---

## 17. Contradictions register (CONTEXTE wins on business rules)

| # | Topic | Sources | Resolution |
|---|---|---|---|
| C1 | Stalled incomplete removal vs allowlist (CON-06) | MASTER §5.1 vs §5.7 (MASTER contradicts itself); CONTEXTE §3.8 (never remove a task blindly) | OQ-06: separate manual-only action; allowlist untouched. |
| C2 | Default mode (CON-03) | CONTEXTE §3.11 `interactive` default vs MASTER `dry_run=true`, `auto_mode=false` | Compatible: interactive approval is the product, `dry_run` is a global safety switch. |
| C3 | Excluded personal directory (CON-08) | CONTEXTE §1.3.8, §9.7 vs MASTER configured roots | MASTER generalizes. Undeclared paths are never opened, which preserves CONTEXTE's intent by construction. |
| C4 | Privilege elevation (CON-30) | CONTEXTE §6.5 allows targeted elevation vs MASTER §8 no root at all | No elevation. This complies with both (CONTEXTE only forbids whole-process root). Ownership mismatches are report-only. |
| C5 | Media mutations (CON-24) | CONTEXTE §2.2 allows planned media replacement vs MASTER §5.4 never automatic deletion in media | Manual approved replacement only (OQ-12). |
| C6 | Importing old reports (CON-29) | CONTEXTE §7.2.3 (as history) vs MASTER §9 (not as current state) | Not in V1. §7.2 is a migration recommendation, not a business rule. No conflict with INV-42. |
| C7 | Consolidation step order (CON-15) | CONTEXTE §3.5 vs legacy code | CONTEXTE order, plus an intent before each mutation including stop. |
| C8 | Auto consolidation age (CON-21) | CONTEXTE §3.11 vs MASTER §6 | Stricter: minimum age for every auto action. |
| C9 | cross-seed connector | CONTEXTE §6.1 diagram lists cross-seed vs MASTER §9 out of scope | MASTER scope. §6.1 is a recommendation, and no CONTEXTE rule requires the connector. REJ-11..13 hold trivially. |
| C10 | Range selection | CONTEXTE §3.11 "sélection par plages" vs web UI | Replaced by per-item checkboxes and select-all without preselection. Same safety (no implicit selection). |
| C11 | Expected owner (CON-09) | CONTEXTE §2.4 historical UID:GID vs MASTER `expected_owner` | Configurable. Default = process identity. |
| C12 | Config file vs "user edits only compose + .env" | MASTER §4 vs MASTER §8 | The TOML file is optional. Every key has an env var (ADR-0005). |
| C13 | Force-rescan buttons | MASTER §6 (rescan qBittorrent, Radarr, Sonarr, Jellyfin) vs dry-run "no ARR mutating call" | "Refresh" = application re-observation (read-only, all four, allowed in dry-run). The service-side rescan command (ARR/Jellyfin) is separate and live-only. |
| C14 | Jellyfin metadata lock | CONTEXTE §3.10 "désactiver le monitoring ARR **et/ou** les mises à jour de métadonnées" | V1 implements ARR monitoring changes (`arr_set_monitored`). The Jellyfin metadata lock is not implemented ("and/or" is satisfied). Recorded as later work. |
| C15 | Service config backup (CTX-10) | CONTEXTE §6.5 | V1 never modifies service configuration (profiles, CFs). Monitoring changes journal the previous value. |
| C16 | Dry-run needs qBittorrent (CON-20) | legacy exits vs CONTEXTE §3.15 | CONTEXTE: scans continue, dependent items are blocked. |
| C17 | Release folder vs "smallest admissible subfolder" (CON-23) | MASTER §5 vs CONTEXTE §3.7 | Consistent once samples are protected. See OQ-11. |
| C18 | Nested roots | MASTER §2 "sans chevauchement ambigu" vs common layouts with incomplete inside completed | Any nesting is refused at startup (fail-closed). Documented by 20. |
| C19 | Scheduling syntax (CP1-F14a) | MASTER §3 "intervalle/cron configurable" | Interval + UTC anchor (`scan.interval_minutes`, `scan.anchor_utc`). No cron expressions. "intervalle/cron" is satisfied by an interval, and nightly runs are covered. Accepted scope cut. |
| C20 | Who creates hardlinks for a different version, and **when the old version disappears** (CP1-F14b, **CP1-F20**) | CONTEXTE §3.6 "créer les nouveaux hardlinks … retirer l'ancienne version seulement après cette preuve" and INV-35 (global transaction, every episode proven before the old coverage is removed) vs ADR-0010 (ARR imports with `importMode=copy`; ARR's upgrade import removes the old media file **inside** the import, before the app's inode proof) | **This is a real conflict. CONTEXTE §3.6 and INV-35 win.** ARR stays the single importer (CONTEXTE §3.9 "une seule méthode"). The order cannot be changed, so the old coverage must be **recoverable before** the import: the `OldCoverageRecoverable` precondition (§7.4) requires each old media file to keep another retained link, or ARR's recycle bin to be configured (`ArrRead::media_management`). Otherwise the plan carries `OldCoverageUnrecoverable` and is blocked. The only way past it is an explicit, per-file, typed acknowledgement of the risk in a manual approval, never automatic. Verification accounts for every old file (`OldFileAccounted`). CP2-tier4 must verify this rule. |
| C21 | `FixMode` in media roots (CP1-F14c) | CONTEXTE §2.2 media "lecture par défaut" vs MASTER §5.5 targeted fix through an approved plan | Allowed only as a manual, approved, non-recursive chmod of entries owned by the process UID (MASTER §5.5, INV-32). Never auto. |
| C22 | Reverse restore library -> torrent source (CP1-F14d) | CONTEXTE §4.11 (McFarland: "peut réparer") | Deferred (accepted scope cut). `torrent_missing_files` shows an informational hint. The CONTEXTE wording is permissive, so no rule is broken. |
| C23 | Simulation in a live process (CP1-F14e) | CONTEXTE §6.2 Planificateur "simulation complète" vs dry-run as a startup switch | Resolved without a trade-off. `POST /plans/{id}/simulate` runs any plan with the recording mutators in any process (section 6). Live execution still requires `dry_run = false` at startup. |
| C24 | ARR grab (CP1-F06) | MASTER §6 grab as a separately authorized capability, §9 excludes only grabs without approval, §10 "toutes les fonctionnalités sont en V1" | Implemented: `ArrGrabRelease`, cap `arr_grab` (default off), manual approval only, never auto. This is **no longer a scope cut**. |
| C25 | Jellyfin item refresh polling (CP1-F13) | agent 10 task 4 "poll refresh state where supported" | Library refresh is polled through ScheduledTasks (`library_refresh_running`). Item refresh has no status API, so it is not polled, and verification uses `items_by_paths`. |

Accepted scope cuts (complete list): C6 (old report import), C9 (cross-seed connector), C14 (Jellyfin
metadata lock; ARR monitoring changes are reachable through `PlanRequest::Media`, CP1-F05), C18
(nested roots refused), C19 (no cron syntax), C22 (reverse restore). None removes a CONTEXTE rule.

No resolution weakens a CONTEXTE invariant. The only override path, C20's per-file acknowledgement
of an unrecoverable old file, is the explicit, audited manual exception that CONTEXTE §2.11 and
§9.4 reserve to the user. It is never available to automatic mode.

---

## 18. Requirement traceability (one owning component per ID)

Owners are agent numbers (crate::module in section 15.2). "Contributors" are informative. The
**owner** is accountable for implementation and tests.

### 18.1 Invariants INV-01..51

| INV | Owner | Contributors |
|---|---|---|
| 01 hardlink proof `(dev, ino)` | 11 | 03, 06 |
| 02 name+size never proves | 07 | 11 |
| 03 funnel + full compare | 07 | 06, 13 |
| 04 repair direction | 13 | 12 |
| 05 permission probe first | 13 | 06 |
| 06 `deleteFiles=false` | 08 | 13 |
| 07 no `BT_backup` | 06 | 05 |
| 08 intent journal + fsync | 13 | 04 |
| 09 configured roots only | 05 | 06 |
| 10 samples protected | 12 | 03, 11 |
| 11 ARR sole quality authority | 12 | 09 |
| 12 technical metadata explains or blocks only | 07 | 19 |
| 13 immutable plans, revalidation | 13 | 12 |
| 14 qBittorrent transaction | 13 | 08 |
| 15 fail-closed | 12 | 11, 13, 14 |
| 16 single importer | 13 | 09, 14 |
| 17 success only after verification | 13 | 03 |
| 18 never follow symlinks | 06 | 13 |
| 19 task removed only when files gone | 13 | 12 |
| 20 recheck timeout -> manual | 13 | 03 |
| 21 gain accounting | 15 | 03, 04 |
| 22 stage separation | 12 | 11, 14, 16, 09 |
| 23 dry-run real path | 13 | 05, 16 |
| 24 no preselection, identical bulk validation, confirmation | 16 | 12, 19 |
| 25 folder all-or-nothing + annexes | 11 | 12, 19 |
| 26 no automatic deletion in media | 12 | 11 |
| 27 junk only outside torrents | 11 | — |
| 28 allowlist | 03 | 08, 12 |
| 29 content-path attachment, one row per content | 11 | 08, 19 |
| 30 bounded path mapping | 05 | 08, 09, 10 |
| 31 no secrets anywhere | 05 | 15, 19, 04 |
| 32 no implicit recursive chmod/chown | 13 | 12 |
| 33 app data outside roots | 05 | 20 |
| 34 ARR snapshot in plans | 09 | 12, 04 |
| 35 season replacement proof | 12 | 13, 07 |
| 36 numbering mapping (ARR episode assignment stored as `CoverageProof`; parsed numbers explanatory, disagreement = `NumberingMismatch`) | 12 | 09 (data), 07 (explanatory parsing) |
| 37 real audio may block | 07 | 12 |
| 38 ARR capabilities separate | 09 | 12 |
| 39 queue alert != missing hardlink | 11 | 09 |
| 40 interruption/crash recovery | 13 | 14 |
| 41 ENOENT protects the zone | 06 | 14 |
| 42 observations dated, not historical | 03 | 04 |
| 43 same device, single volume | 13 | 06, 20 |
| 44 low I/O scheduling | 14 | 19, 06 |
| 45 similarity is identification only | 07 | — |
| 46 ignore list in DB, respected | 12 | 04, 16, 19 |
| 47 auto off by default, strict rules | 12 | 05, 14 |
| 48 different version = replacement | 07 | 12 |
| 49 parent cleanup under completed | 13 | 12 |
| 50 no shell by concatenation | 07 | 13 |
| 51 web security and deployment | 17 | 20, 16 |

### 18.2 MASTER requirements

| ID | Requirement (MASTER section) | Owner |
|---|---|---|
| M0.1 | Cycle observe -> report, no hidden stage (intro) | 12 (stage boundaries, section 1.1) |
| M2.1 | Hardlink proof `(st_dev, st_ino)` | 11 |
| M2.2 | Byte-by-byte equality via funnel, no full SHA-256 | 07 |
| M2.3 | qBittorrent never deletes data; no `BT_backup` | 08 |
| M2.4 | Configured roots only, validated, confined | 05 |
| M2.5 | Samples protected | 12 |
| M2.6 | Sonarr/Radarr sole quality authority | 12 |
| M2.7 | Repair direction completed -> media, temp link + atomic replace | 13 |
| M2.8 | Intent journal + fsync; immutable hashed plans revalidated | 13 |
| M2.9 | qBittorrent multi-hash transaction | 13 |
| M2.10 | Fail-closed | 12 |
| M2.11 | Single importer per media | 13 |
| M2.12 | Success only after verification | 13 |
| M2.13 | Anti-pattern `st_nlink == 1` | 06 |
| M2.14 | Anti-pattern client-side data deletion | 08 |
| M2.15 | Anti-pattern destructive route without auth/CSRF/approved plan | 17 |
| M2.16 | Anti-pattern `localStorage` exclusions | 16 |
| M2.17 | Anti-pattern hard-coded roots, debug server, root container | 05 |
| M2.18 | Anti-pattern publish on every push, unpinned actions | 20 |
| M3.1 | 100 % Rust server side | 02 |
| M3.2 | Axum + Tokio | 16 |
| M3.3 | Askama + HTMX, only JS = HTMX | 19 |
| M3.4 | Server-side SVG charts | 15 |
| M3.5 | SQLite via sqlx | 04 |
| M3.6 | Scales to 20k+ files, 3k+ torrents | 23 |
| M3.7 | Stores temporal tracking, sessions, evidence, plans, journal, snapshots | 04 |
| M3.8 | Full action history with unique gain ID | 15 |
| M3.9 | SQLite on config volume, WAL, batched commits | 04 |
| M3.10 | Content reads bounded, retained candidates only | 07 |
| M3.11 | Incomplete tracking via qBittorrent API first | 11 |
| M3.12 | Scans scheduled or manual, never looping; UI never wakes disks | 14 |
| M3.13 | No frantic flush; only intent fsync | 04 |
| M3.14 | Background workers, persistent queue, safe cancel, resume after restart | 14 |
| M4.1 | `download_roots` (completed, incomplete) | 05 |
| M4.2 | `media_roots` | 05 |
| M4.3 | `path_mappings` per service, bounded, compose example per service | 05 (compose example: 20) |
| M4.4 | `expected_owner` | 05 |
| M4.5 | Thresholds (stagnation, min age, extensions, scan intervals) | 05 |
| M4.6 | `dry_run` default on | 05 |
| M4.7 | `auto_mode` default off | 05 |
| M4.8 | Jellyfin library = media roots; API for health, refresh, recognition | 10 |
| M5.0 | Issue fields (category, severity, evidence + level, provenance, date, age, blockers); potential never guaranteed, inode counted once | 11 |
| M5.1 | Stagnant incomplete + incomplete orphans; action per OQ-06 | 11 |
| M5.2 | Hardlink defects with three outcomes | 11 |
| M5.3 | Real duplicates, consolidation completed -> media | 11 |
| M5.4 | Orphans (completed without torrent, missingFiles, media-only informational, never auto-deleted in media) | 11 |
| M5.5 | Permissions and owner; targeted fix only via approved plan | 11 |
| M5.6 | Junk only outside torrents; samples excluded | 11 |
| M5.7 | qBittorrent allowlist, 4.x/5.x names | 03 |
| M5.8 | Attach by content path, never torrent name | 08 |
| M5.9 | Several torrents per content: one row, torrent count, all hashes | 11 |
| M5.10 | Release folder unit, all-or-nothing, annex preview, folder vs file marker | 11 |
| M5.11 | Parent cleanup in the same transaction, completed only | 13 |
| M5.12 | ARR replacement: ARR decision, snapshot invalidation, ffprobe, numbering, season coverage | 12 |
| M6.1 | Responsive mobile-first UI | 19 |
| M6.2 | Dashboard metrics with color + non-color urgency | 19 |
| M6.3 | Tab per issue type with filters, search, evidence, blockers | 19 |
| M6.4 | Columns name, age, size, torrent count | 19 |
| M6.5 | Sort by name, size, age | 15 (query) |
| M6.6 | List total and selection total | 16 |
| M6.7 | Select-all without preselection | 19 |
| M6.8 | Hide-recent filter (configurable threshold) | 15 |
| M6.9 | Refresh button | 19 |
| M6.10 | Ignore list in DB, Ignored tab, restore, respected by auto | 16 |
| M6.11 | History & statistics, SVG gains (potential, expected, attributable, observed) | 15 |
| M6.12 | Logs tab + stdout, configurable level, no secrets | 15 |
| M6.13 | Unit or bulk with identical validations; blocked excluded and listed | 12 |
| M6.14 | Folder deletion all-or-nothing with annex preview | 12 |
| M6.15 | Dry-run default, full logic minus mutation adapters | 13 |
| M6.16 | Explicit confirmation modal (target, count, space, irreversible effects) | 19 |
| M6.17 | Force-rescan buttons per service | 16 |
| M6.18 | Auto mode rules, stops at the slightest uncertainty | 12 |
| M6.19 | Official REST APIs with path mapping | 08 (pattern; 09, 10 for theirs) |
| M6.20 | qBittorrent 4.x/5.x, 204 login, pause/stop | 08 |
| M6.21 | ARR search != grab; separate capabilities (`arr_grab`, `arr_manual_import`, `arr_queue_remove`, `arr_set_monitored`); queue alert != missing hardlink (`arr_queue` category) | 09 |
| M7.1 | English code, comments, docs, logs, commits | 02 (lint and docs policy) |
| M7.2 | UI English default, French available | 19 |
| M7.3 | `locales/*.json` embedded, English fallback | 19 |
| M8.1 | Form auth, Argon2 hash in env, session key | 17 |
| M8.2 | Encrypted session HttpOnly SameSite, CSRF, login rate limit | 17 |
| M8.3 | No secrets in logs, reports, HTML, DB | 05 |
| M8.4 | Rootless; `user:` in compose; no PUID/PGID | 20 |
| M8.5 | Multi-stage, static musl, runtime with ffprobe | 20 |
| M8.6 | Multi-arch amd64/arm64 | 20 |
| M8.7 | Single common-parent volume | 20 |
| M8.8 | Separate config volume | 20 |
| M8.9 | User edits only compose + `.env` | 20 |
| M8.10 | Lightweight healthcheck that does not wake disks | 20 (route `/healthz` + CLI `healthcheck`: 16) |
| M8.11 | Port on 127.0.0.1 by default | 20 |
| M8.12 | `.env.example`, commented compose, README, issue templates | 20 |
| M8.13 | CI fmt, clippy, test, build on PR and main | 20 |
| M8.14 | GHCR only on `v*.*.*`, version + latest, multi-arch, never on md/fix/feat | 20 |
| M8.15 | Actions pinned by SHA, minimal permissions | 20 |
| M9.1 | No Transmission or other clients | 08 |
| M9.2 | No Prowlarr or cross-seed integration | 01 (scope; no crate exists) |
| M9.3 | No grab without explicit approval (`ArrGrabRelease`: cap `arr_grab` off by default, manual approval only, never auto) | 09 |
| M9.4 | No import of old reports as current state | 04 |
| M9.5 | No direct ARR SQLite | 09 |
| M10.1 | Risk tiers with crash-point and dry-run tests | 13 |
| M10.2 | Disjoint module ownership for parallel waves | 01 (section 15) |
| M10.3 | Handoffs and quality gates | orchestrator |

### 18.3 Legacy behaviors (each ID exactly once)

| Owner | DR | WF | FH | API | T | Other |
|---|---|---|---|---|---|---|
| 03 domain-model | 01, 04 | — | — | — | 03 | CON-01, CON-05; CTX-12; NEW-08, NEW-11 |
| 04 db-layer | — | — | 02 | — | — | RPT-03; CON-29; REJ-16, REJ-30; NEW-03 |
| 05 config-secrets | 32, 33 | 01 | 01 | — | 18, 26 | CFG-01..20; CON-03, CON-04, CON-08, CON-09, CON-18, CON-31; CTX-11; REJ-17, REJ-28; NEW-07 |
| 06 fs-engine | 02, 07, 13, 18 | — | 13 | — | 01 | CON-27; REJ-01, REJ-05; NEW-09 |
| 07 media-match | 08, 09, 10, 11, 12, 15, 16 | — | — | 12 | 04, 05, 09, 10 | CON-16; REJ-02, REJ-03, REJ-04; NEW-14 |
| 08 qbit-api | 23, 24, 28 | 07 | 04, 07 | 01-09 | 25, 27, 28, 29 | CON-07; CTX-08; REJ-06, REJ-21, REJ-26, REJ-29 |
| 09 arr-api | — | — | — | 10 | — | CON-17; CTX-05, CTX-06, CTX-10; REJ-15, REJ-22; NEW-13 |
| 10 jellyfin-api | — | — | — | 11 | — | NEW-15 |
| 11 issue-engine | 03, 05, 06, 36 | 02, 03 | — | — | 02, 08, 15 | CON-19, CON-20, CON-23; CTX-04 (hint); NEW-06, NEW-10, NEW-12 |
| 12 plan-engine | 14, 25, 26 | 04, 08 | 06 | — | 06, 07, 19, 20, 21 | RPT-06; CON-02, CON-06, CON-10, CON-14, CON-21, CON-22, CON-24, CON-25, CON-28; CTX-07; REJ-07, REJ-09, REJ-10, REJ-24 |
| 13 action-runner | 17, 19, 20, 21, 22, 27, 29, 30 | 11, 12 | 05, 09, 10, 11, 12, 14 | — | 11, 12, 13, 14, 16, 17, 22, 23, 30, 32 | EVT list; CON-11, CON-12, CON-15, CON-26, CON-30; REJ-19, REJ-20, REJ-23, REJ-25; NEW-26 |
| 14 job-runner | 34 | 06, 09 | 08 | — | 31 | CTX-03; REJ-08; NEW-04, NEW-05 |
| 15 reporting | 31 | 05, 13 | — | — | — | RPT-01, 02, 04, 05, 07, 08, 09; CON-13; CTX-01, CTX-02, CTX-09; REJ-27; NEW-02, NEW-19 |
| 16 backend-api | — | 10 | — | — | 24 | CON-32; NEW-01, NEW-18, NEW-20 |
| 17 auth-guard | — | — | 03 (obsolete: replaced by web auth) | — | — | REJ-14; NEW-22 |
| 19 web-ui | 35 | — | — | — | — | NEW-16, NEW-17, NEW-21 |
| 20 devops | — | — | — | — | — | REJ-18; NEW-23, NEW-24, NEW-25 |
| 01 (scope) | — | — | — | — | — | REJ-11, REJ-12, REJ-13 (out of scope, no cross-seed crate) |

HIS-01..10 and ONE-01..13 are historical. They are never imported (REJ-30). Their lessons are
covered by the INV and CTX owners above.

### 18.4 CONTEXTE coverage (CP1 checklist)

CONTEXTE §1.3 (1-11), §2.12 (1-13), §3.1-§3.15 and §9 (1-10) map to the IDs listed in
`docs/legacy-parity.md` Appendix A. Every one of those IDs has an owner in 18.1 or 18.3.
CONTEXTE §6.2-§6.7 capabilities: dashboard (19/15), explorer (15, `/explorer`), bidirectional
inventories (11, including `arr_queue` for "imports ARR en erreur ou obsolètes"), comparator (`IssueDetailVm`, 19), planner (12), executor (13/14), reports (15),
domain model §6.3 (03), state machine §6.4 (03), security §6.5 (17/05), ARR contract §6.6 (09),
acceptance tests §6.7 (22, with each line traced in 22's handoff).

---

## 19. Testing responsibilities (summary)

- Feature agents ship unit tests for their crate. They use `testkit` fakes, temp dirs, the fake
  clock and dummy secrets. No network beyond loopback, no real services.
- 21 keeps the T-01..32 matrix current and adds `parity_*` tests.
- 22 owns the parity fixture assertions, the crash-point matrix (crash = drop the runner after
  intent N, or after mutation N, then run recovery), stale-plan tests, the job and lock tests,
  and route tests with real repositories.
- 23 owns the section 14 budgets and the HTTP abuse flows.
- Tier gating: 13 raises `IMPLEMENTED_TIER` only after crash-point and dry-run-no-mutation tests
  of that tier pass in its own test suite, and CP2 confirms with 22's suite.

---

## 20. CP1 responses (review `docs/reviews/cp1.md`, iteration 1)

Every finding was fixed. None was rejected.

| ID | Severity | Fixed where |
|---|---|---|
| CP1-F01 | blocking | §5 `ArrCommands::manual_import` (no mode field; `importMode="copy"` literal); §4.1 `PathStillHasInode`, `ArrImportMoved`; §7.4 ReplaceRelease steps 1-5 (completed source re-`stat`ed before and after; move -> `FailedNeedsReview`; copy -> `ExecutedUnverified` + `AddedCost`, `DeleteMediaFile` blocked; step 5 consistent); §6.8(c)(d) gate grep + `fake_arr` assertion + 22 move test; §15.3 V10; ADR-0010 |
| CP1-F02 | blocking | §7.3 `DeleteReplaced` planning rule; §13.3 A7; §7.4 delete step 1 (replacement re-`stat`ed + ARR path re-read; mismatch -> `ArrAmbiguous`, manual only) |
| CP1-F03 | major | §4.1 `download_path`, all-candidate list with `.!qB` variants, `is_working_file`; §7.1 step 3 ownership index; §7.4 DeleteIncompleteOrphan; §11 `download_path` mapping note; §15.3 V9 |
| CP1-F04 | major | §4.1 `ArrEpisode`, `CoverageProof`, `episode_ids` on preview decisions; §5 `ArrRead::episodes`; §7.2 coverage definition and 07's explanatory role (`NumberingMismatch`); §18.1 INV-36 single owner 12 |
| CP1-F05 | major | §4.2 `arr_queue` category (INV-39 evidence, `QueueItemHidesError`); §7.3 `PlanRequest::Media`; §7.4 manual ARR commands; §12.1 `/media/...`; §12.2 `DashboardVm.arr_queues`, `MediaDetailVm`; §17 C14 note |
| CP1-F06 | major | §13.2 `arr_grab` (default off); §5 `ArrCommands::grab`; §4.1 `ReleaseSearchResult` (indexer, seeders, size, release guid/indexer id, CF scores, rejections) + `sort_search_results`; §7.4 `ArrGrabRelease` (tier 4, manual, never auto, live, journaled); §13.1, A9; §17 C24; §18.2 M9.3; ADR-0010 |
| CP1-F07 | major | §4.1 `MatchCandidate`; §5 `MatchStore`; §7.1 step 6 + `RefreshService` = classification only, zero fs calls; §9.1 `match_candidates`; §11 `scan.qbit_refresh_minutes`; §8.1, §8.5 item 3; §14 B6 |
| CP1-F08 | minor | §7.2 VerifyContent (`workflows::evaluate::verify_content`, agent 12, output contract, bounds, shared locks) |
| CP1-F09 | minor | §7.4 common rules: affected-hash definition (source **or target** paths; inode-only sharing informational) |
| CP1-F10 | minor | §7.1 (qBittorrent `NotConfigured` -> `ServiceNotConfigured` on every ownership-dependent action and classification); §16 OQ-05 |
| CP1-F11 | minor | §7.6 two-phase auto cycle (consolidations -> rescan -> abort on errors -> deletion planning); §8.1 |
| CP1-F12 | minor | §7.3 companion actions (`RemoveEmptyParent` automatic, `remove_torrent_tasks` opt-in, separate typed count); §4.1 `Approval.confirmed_torrent_removals`; §12.2 `ConfirmVm` |
| CP1-F13 | minor | §5 `ArrCommands::rescan_all`, `JellyfinRead::library_refresh_running`; §7.3 `PlanRequest::ServiceRescan` (one-action plans, so every journal intent has a plan); §12.1 rescan route + `POST /recovery/retry`; §8.4; §17 C25 |
| CP1-F14 | minor | §17 C19-C23 (+ C24, C25 and the accepted-cuts list); (e) resolved by simulation in any process (§6.4-5, `POST /plans/{id}/simulate`, ADR-0003) |
| CP1-F15 | minor | §12.4 HTMX pinned to `htmx.org@2.0.4`, SHA-256 constant + test + SRI, release gate, no ship-without option; handoff TODO updated; ADR-0006 |
| CP1-F16 | minor | §15.1 rules 7-8 (parity tests excluded from crate globs; 21 may add workspace dev-deps between waves; no double "creates"); §15.2 rows 16, 17, 19 |
| CP1-F17 | minor | §8.4 response size limit; §11 `limits.max_response_bytes` |
| CP1-F18 | minor | §4.1 `CandidateState` derived only (`derive_candidate_state`), `PlanStatus::can_transition_to` is the enforced machine; §9.1 `torrent_observations` dropped |
| CP1-F19 | minor | §7.1 step 8 and §13.3 A2 share one rule (`QbitInternal` does not degrade the session or auto; units containing any protected zone are blocked, §4.3); residue fixed in iteration 2: §16 OQ-05 aligned |

### 20.1 Iteration 2 follow-up (PASS with condition)

| ID | Severity | Fixed where |
|---|---|---|
| CP1-F20 | major | §4.1 `ArrMediaManagement` (in `ArrQualitySnapshot` + fingerprint), `OldFileRecovery`, `Precondition::OldCoverageRecoverable`, `VerificationStep::OldFileAccounted`, `Approval.acknowledged_unrecoverable`; §4.3 `OldCoverageUnrecoverable`, `ArrHardlinksDisabled`; §5 `ArrRead::media_management()` (domain contract for wave 3); §7.4 ReplaceRelease steps 1-3; §12.1 approve route; §13.1 tier-4 tests + CP2-tier4 note; §15.3 V10; §17 C20 rewritten (conflict registered, CONTEXTE wins) + closing note; ADR-0010 |
| CP1-F19 (residue) | minor | §16 OQ-05 now states the same single protected-zone rule |
| CP1-F21 | minor | §7.1 step 3: "content directory" defined for multi-file torrents only; single-file torrents own only their candidates (+ `.!qB`) and `.parts` files |
| CP1-F22 | minor | §6.8(e) live-process simulation guard test (owners 13, 22) |
| CP1-F23 | minor | §7.6 step 2: rescan only the roots live-mutated in phase 1 (none in dry-run, none if nothing executed), otherwise reuse the session; §8.1 |
| CP1-F24 | minor | §11 `download_path` paragraph moved below the table |
| CP1-F25 | minor | §15.2 row 19 owns `crates/web/src/assets.rs` and "all templates (login: markup and styling only)"; §12.1 approve route carries `confirmed_torrent_removals` |
