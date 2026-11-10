# Handoff — 01-system-arch (wave 1)

## Delivered scope

- `docs/architecture.md`: the complete architecture.
  - Sections 1-2: stage ownership, layering, crates and dependency-direction gates.
  - Section 3: libraries and versions, profiles, lints.
  - Sections 4-5: domain model and ports.
  - Section 6: dry-run enforcement.
  - Section 7: workflows with step sequences for every action kind, plus recovery and the auto cycle.
  - Section 8: concurrency, cancellation, locking and low-I/O rules.
  - Section 9: persistence and transaction ownership.
  - Section 10: errors and observability.
  - Section 11: configuration contract.
  - Section 12: routes and view models.
  - Section 13: risk tiers and capability flags.
  - Section 14: resource budgets.
  - Section 15: module ownership.
  - Section 16: open-question resolutions.
  - Section 17: contradictions register.
  - Section 18: traceability (INV-01..51, M0.1..M10.3, and every DR, WF, FH, API, T, CON, CTX, NEW,
    REJ, RPT and CFG ID, each with exactly one owner).
- `docs/adr/0001` to `0010`: irreversible choices (index in architecture section 15.5).
- This handoff.

No code and no files outside `docs/` were written.

## Crate list

`removarr-domain`, `removarr-config`, `removarr-storage`, `removarr-fs`, `removarr-qbit`,
`removarr-arr`, `removarr-jellyfin`, `removarr-matching`, `removarr-workflows`, `removarr-jobs`,
`removarr-reporting`, `removarr-web`, `removarr` (binary, `crates/app`). Test-only crates:
`removarr-testkit`, `removarr-integration-tests` (created by 22) and `removarr-e2e-perf`
(created by 23).

## Contracts for `02-rust-scaffold`

1. Root `Cargo.toml`:
   - `members = ["crates/*"]`, `resolver = "3"`.
   - `[workspace.package]` with `edition = "2024"`, `rust-version = "1.88"`, `license` left
     unset (README says "to be defined"), `publish = false`.
   - **Every** dependency of architecture section 3 in `[workspace.dependencies]`, with the
     listed features. sqlx and reqwest use `default-features = false`.
   - `[workspace.lints]` and profiles as in section 3. Do not add `rust-toolchain.toml`.
2. Create every crate of section 2.1 except `integration-tests` and `e2e-perf`. Each one gets
   `Cargo.toml` (its deps limited to the "Allowed deps" column, using `workspace = true`) and a
   minimal `src/lib.rs`. `crates/app` gets `src/main.rs` with binary name `removarr`.
3. `removarr-config` declares a `test-support` feature (empty for now). It is used by later
   agents for `LiveMode::for_tests()`.
4. `crates/testkit`:
   - `Cargo.toml` deps: `removarr-domain`, `tokio`, `axum`, `serde`, `serde_json`, `tempfile`, `rustix`.
   - `src/lib.rs` declares `pub mod clock; pub mod ids; pub mod fixture; pub mod fake_qbit; pub mod fake_arr; pub mod fake_jellyfin;`
     with an empty file for each module. Wave-5 agents own one file each and must not edit
     `lib.rs` or `Cargo.toml`.
5. `docs/development.md`: local bootstrap, quality gate commands
   (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`, `cargo build --workspace --release`), and the dependency gate greps of
   architecture section 2.2. Note: the host target only, no rustup.
6. Record the resolved versions from `Cargo.lock` in the 02 handoff. If a listed requirement does
   not resolve, pick the nearest compatible release of the same major version and record it as a
   deviation.

## Contracts for `03-domain-model`

1. Implement **all** of architecture section 4 (modules, types, functions) and section 5 (every
   port trait with the given names and signatures), including the connector DTOs, `JobRecord`,
   `JobKind`, `Lane`, `JobOutcome`, `SampleSpec`, `CompareOutcome`, `ManualImportRequest`,
   `CommandState` and `DiffStats`. The domain becomes read-only after wave 3. Wave-5 agents cannot
   add types.
2. Allowed dependencies: `serde`, `serde_json`, `thiserror`, `sha2`, `uuid` (`serde` feature),
   `async-trait`. Nothing else.
3. Required pure functions:
   - `torrent::destructive_eligible`, `is_checking`, `was_active`, `evaluate_recheck` (CON-12);
   - `policy::is_sample` (OQ-01);
   - `gain::gain_id`, `gain::estimate_reclaimable` (DR-04, T-03);
   - `ids::IssueId` derivation;
   - `PlanHash` computed over the canonical `serde_json` serialization;
   - `ActionKind::tier()` / `capability()` (section 13);
   - `candidate::derive_candidate_state` (CTX-12, derived only) and
     `PlanStatus::can_transition_to` (enforced machine);
   - `torrent::is_working_file`, `arr::sort_search_results`.

   Also the types added at CP1: `TorrentInfo.download_path`, `ArrEpisode`, `ReleaseRef`,
   `ReleaseSearchResult`, `CoverageProof`, `MatchCandidate`, `Approval.confirmed_torrent_removals`,
   the new `VerificationStep`/`BlockerCode`/`JournalStep` variants, `IssueCategory::ArrQueue`,
   `ActionKind::ArrGrabRelease`, `Capability::ArrGrab`, and the ports `MatchStore`,
   `ArrRead::episodes`, `ArrCommands::{grab, rescan_all}`, `JellyfinRead::library_refresh_running`.
4. Deterministic serialization: `BTreeMap`/`BTreeSet`/`Vec` only in hashed or persisted types.
   `Timestamp` = UTC unix milliseconds.
5. Also own `crates/testkit/src/clock.rs` (`FakeClock` with manual advance) and `ids.rs`
   (`SeqIds`, deterministic UUIDs).
6. Tests (in addition to the agent file): every `TorrentState` string from qBittorrent 4.x and 5.x
   parses, and unknown strings parse to `Unknown` and are never eligible; `is_sample` covers FX-07,
   FX-27 and negative cases; `estimate_reclaimable` covers FX-34/FX-35 (T-03); a plan hash is stable
   across serialize and deserialize round trips.

## Other public contracts (summary; details in `docs/architecture.md`)

- Dry-run: `LiveMode` token (05), live adapter constructors take `&LiveMode` (06/13, 08, 09, 10),
  `Mutators` built only in `app::wiring` (16), and plans carry `PlanMode`.
- Journal: SQLite `synchronous = FULL` single writer. `JournalStore::record_intent` is durable
  on return (04).
- Issue category names: section 4.2. They are stable for URLs, DB and i18n keys.
- Capability flags and defaults: section 13.2. `IMPLEMENTED_TIER` is raised by 13 only after a
  tier's crash-point and dry-run tests pass and CP2 passes.
- Config env variable names: section 11 (public contract for 05, 16, 17, 20).

## Deviations from agent files (MASTER and this architecture win)

| Agent file | Instruction | Architecture decision |
|---|---|---|
| 08 qbit-api task 6 | "recheck, polling with timeout" in the connector | 08 provides the `recheck` call and state parsing. The polling loop lives in `workflows::actions` (13) and uses the domain `evaluate_recheck`, so cancellation and journaling stay in one place. |
| 07 media-match | "test target-directory write capability before full comparisons" | The probe creates a temporary hardlink, which is a mutation. It belongs to the 13 transaction (live only). Scans never run full compares, and 07's funnel stops at samples. |
| 14 job-runner task 5/9 | serialize by media identity, path scope, action type | A single execution lane plus root `RwLock`s, plus the ARR queue check (ADR-0007). No per-media lock table. |
| 15 reporting task 4 | "store" action history | Storage is 04 (written by 13). 15 queries and presents it. |
| 09 arr-api task 4/9 | searches and grabs as separate capabilities | Implemented as specified. Searches are read-only. `ArrGrabRelease` requires cap `arr_grab` (default off), manual approval only. Manual import always uses `importMode="copy"` (ADR-0010). |
| 17 auth-guard task 3 | `Secure` cookies | `Secure` defaults to on and is configurable (`cookie_secure`) for plain-HTTP LAN setups. |
| 06 fs-engine task 7 | permission anomalies | Detected from mode bits, uid/gid and the sticky bit versus `expected_owner`. ACLs are invisible (CONTEXTE §7.1.11) and documented as a limitation. |
| MASTER §3 | "intervalle/cron configurable" | Interval + UTC anchor (`scan.interval_minutes`, `scan.anchor_utc`). No cron expressions. Nightly runs are covered. |
| CONTEXTE §4.11 / CTX-04 | reverse restore library -> torrent source | Deferred (informational hint only). |
| CONTEXTE §3.10 | Jellyfin metadata lock | Deferred. ARR monitoring change only (CONTEXTE says "and/or"). |

All contradictions with CONTEXTE are in architecture section 17. None weakens a CONTEXTE
invariant.

## Open TODOs

1. **HTMX asset (19, orchestrator).** Pinned to `htmx.org@2.0.4`. 19 performs the one pinned
   download (the orchestrator has approved it), cross-checks it against the jsDelivr per-file hash
   and the official SRI, records the SHA-256 constant and SRI, and adds the checksum unit test.
   A release without it is not an option (27 gate). Working without JavaScript remains a
   degradation property only.
2. **Docker builder tag (20).** Verify that `rust:1.98-alpine` exists at build time. Otherwise use
   the nearest tag >= 1.88 and record it.
3. **ARR endpoint details (09).** Exact v3 field names for `manualimport` preview rejections,
   `customFormatScore` on file resources, and command status. Fakes in `testkit::fake_arr`
   encode the assumed contract, which must be documented in the 09 handoff.
4. **Jellyfin `items_by_paths` (10).** Choose a paged `/Items?Recursive=true&Fields=Path` strategy
   or an equivalent, bounded by `http_concurrency_per_service`.
5. **Parity fixture (06, 08, 22).** Build the fixture per `docs/legacy-parity.md` §7 in `testkit`.
   The §7.7 "3 proven pairs" requires a `VerifyContent` run.
6. **21.** Use architecture section 18.3 as the owner reference for T-01..32 when adding Rust test names.

## CP1 iteration 1 changes

The review `docs/reviews/cp1.md` (FAIL: 2 blocking, 5 major, 12 minor) is fully addressed. No
finding was rejected. The per-finding map is in `docs/architecture.md` §20.
- Blocking:
  - F01: ARR manual import always uses the `importMode="copy"` literal. The completed source
    inode is verified after import. A move is detected as `FailedNeedsReview`. A copy gives
    `ExecutedUnverified` + `AddedCost`.
  - F02: replacement presence (mapped, scanned, size, distinct inode) is required for
    `DeleteReplaced` and auto deletions, and revalidated as `EntryUnchanged`. A mismatch gives
    `ArrAmbiguous`, which means manual only.
- Major:
  - F03: qBittorrent working files (`.!qB`, `.parts`, `download_path`) belong to their torrent.
  - F04: `ArrRead::episodes` and the `CoverageProof` definition. INV-36 now has the single owner 12.
  - F05: `arr_queue` category, dashboard queue counts, `PlanRequest::Media`.
  - F06: `arr_grab` capability and `ArrGrabRelease` (manual only), plus `ReleaseSearchResult`.
  - F07: funnel results persisted (`MatchStore`). `RefreshService` reclassifies only, with zero fs
    calls. New key `scan.qbit_refresh_minutes`.
- Minor F08-F19: VerifyContent owner and contract, affected-hash definition, qBittorrent
  not-configured rule, two-phase auto cycle, companion actions and separate torrent-removal count,
  `rescan_all`, one-action service-rescan plans, `/recovery/retry`, Jellyfin library-refresh polling,
  register entries C19-C25, pinned HTMX 2.0.4 with checksum, ownership rules 7-8, response size
  limit, derived `CandidateState` (and `torrent_observations` dropped), and a single
  `QbitInternal` rule.
- ADRs amended: 0003 (simulation in any process), 0006 (pinned HTMX), 0010 (copy literal, source
  inode check, approved grab).
- Ownership for waves 4, 5 and 13 is still disjoint. New fixture variants V9 (06/08) and V10 (09)
  stay inside each agent's own testkit file.

## Commands run and results

- No shell commands were run. Tools used: Read, Glob, Grep (inputs) and Write, Edit (outputs).
- Files read: `MASTER-PROMPT.md` (all), `CLAUDE.md`, `docs/PROGRESS.md`,
  `docs/handoffs/00-legacy-audit.md`, `docs/legacy-parity.md` (all 861 lines),
  `.claude/agents/01..27*.md` (all), `README.md`, `old/CONTEXTE_RACINE_APP_NAS.md` §1.2-§1.4,
  §2.1-§2.4, §2.11-§2.12, §3 (all), §6 (all), §7.1-§7.3, §9.
- `Glob` confirmed that no `Cargo.toml`, `crates/`, `locales/` or `docs/adr/` existed before this wave.
- CP1 iteration 1: read `docs/reviews/cp1.md` in full (not edited). Edited `docs/architecture.md`,
  ADR-0003, ADR-0006, ADR-0010 and this handoff. `Grep` checks found no stale "grab not
  implemented", "ship without HTMX" or `torrent_observations` statements left in the architecture
  or ADRs.
- No network, no live storage, no services, no secrets.
