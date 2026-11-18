# Release readiness (wave 20, `27-release-gate`, CP3)

**Verdict: READY WITH CONDITIONS**

In MASTER-PROMPT terms this is a `PASS`, but it holds only while the conditions in section 9 are open. The user must
meet C1 to C6 before pushing a `v*.*.*` tag. If any of C1, C2, C3 or C4 fails, the verdict becomes **BLOCKED**, with
that command as the failing gate.

- Source revision: `38a2294518d5119237a9e3e1831be9038d491501` (branch `autonomous-build`). The worktree was clean
  at the start and at the end of this gate. This gate changed only `docs/release-readiness.md`, `docs/reviews/cp3.md`
  and `docs/handoffs/27-release-gate.md`, and committed none of them.
- Host: macOS 26.5.1 arm64, rustc/cargo 1.98.0 (Homebrew). CI pins Rust 1.91.1 and declares MSRV 1.88.
- Reviewer: `27-release-gate`, an independent agent that wrote none of the deliverables. `crates/domain/src/plan.rs`
  was only grepped.
- Nothing was published, tagged or pushed. No network, Docker or live service was used. The ignored benchmarks were
  not run: performance and container numbers come from the handoffs named below.

---

## 1. Quality gate (run by this gate, one cargo command at a time)

| # | Command | Result |
|---|---|---|
| G1 | `cargo fmt --check` | **pass** (0.7 s) |
| G2 | `cargo clippy --workspace --all-targets -- -D warnings` | **pass**, 0 warnings. Fingerprints were up to date, so cargo replayed the stored diagnostics and found none. |
| G3 | `cargo test --workspace` (log in scratchpad) | **pass**: **1055 passed, 0 failed, 6 ignored**, 132 test result lines, 2 min 54 s wall |
| G4 | `cargo build --release -p removarr` | **pass** (46.6 s). Host binary is 14,964,432 bytes (14.3 MiB, arm64 Mach-O, not musl). |
| G5 | `cargo tree -p removarr -e normal,features --offline \| grep -c tier5` | **0** |
| G6 | `cargo tree -p removarr -e normal,features --offline \| grep -ci test-support` | **0** |
| G7 | `cargo metadata --offline` license inventory of the production closure of `removarr` | 241 third-party crates, all permissive (MIT/Apache-2.0/ISC/BSD/Zlib/Unicode-3.0/BSL-1.0/CDLA-Permissive-2.0/Unlicense). No GPL, AGPL, MPL or unlicensed crate. |

The 6 ignored tests are opt-in, not skipped failures:
- the B1-B11 benchmark;
- the Argon2 timing;
- two heavy stress tests;
- the heap-evidence test;
- the real-`ffprobe` test.

Key binaries in G3:

| Area | Test binary | Result |
|---|---|---|
| Migrations | `storage/tests/migrations.rs` | 3/3 |
| Persistence | `integration-tests/tests/persistence.rs` | 2/2 |
| Journal | `storage/tests/journal.rs` | 10/10 (includes the 817eedc regression test) |
| Auth | `web/tests/auth_flow.rs` | 20/20 |
| Auth over HTTP | `e2e-perf/tests/auth_http.rs` | 6/6 |
| Auth over a real listener | `integration-tests/tests/auth_listener.rs` | 3/3 |
| UI flows | `ui_flows.rs` | 8/8 |
| Dry run | `dry_run.rs` | 2/2 |
| Crash | `crash.rs` | 4/4 |
| Stale plans | `stale_plans.rs` | 6/6 |
| Automatic cycle (integration) | `auto_cycle.rs` | 3/3 |
| Automatic cycle (jobs) | `auto_cycle.rs` | 18/18 |

## 2. Safety invariants (checked in source by this gate)

| Invariant | Evidence | Status |
|---|---|---|
| qBittorrent never deletes data | The only `deleteFiles` literal in non-test source is `crates/qbit/src/control.rs:111`, `form.push(("deleteFiles", "false"))`, inside `Cmd::RemoveTask`. Other hits are doc comments. No `deleteFiles=true` exists anywhere except negative assertions in tests (`qbit/tests/adapter.rs:263`, `parity_safety.rs:119`, `integration-tests/tests/crash.rs:87`). | held |
| ARR import copies, queue removal keeps data | `crates/arr/src/commands.rs:230` sends `"importMode": "copy"`, and the request type has no mode field. Queue removal sends literals `removeFromClient=false`, `blocklist=false` (`commands.rs:252-253`). | held |
| Single live capability | `cfg.live_mode()` is called once in non-test source, at `crates/app/src/wiring.rs:176`. `LiveMode` has a private field. `test-support` is enabled only through `[dev-dependencies]` (G6). | held |
| Dry-run on, auto off by default | `crates/config/src/load.rs:764-770` defaults to `dry_run = true` and `auto_mode = false`. `.env.example` and `docker-compose.yml` repeat `true`/`false`. `app/tests/wiring.rs:114` asserts auto is off by default. | held |
| Samples protected | `issues_categories.rs::sample_videos_are_listed_as_keep_sample_and_protected`, `::junk_is_only_what_no_torrent_holds_and_never_a_sample`, `issues_overlap.rs::samples_are_protected_in_every_category`, `plans_build.rs::samples_need_acknowledgement_in_folders_and_are_never_deleted_alone`, `actions_tier5.rs::t5_13_samples_are_never_automatic`, and the runner `sample_blocker` in `actions/delete.rs`. | held |
| No automatic deletion in media roots | The runner refuses any deletion under a media root (`actions/delete.rs`, `MediaRootDeletion`). Deletion kinds are bound to `Completed`/`Incomplete` (`ProtectedZone`). `is_auto_eligible` (`plan.rs:108`) is limited to Consolidate, DeleteCompletedFile/Folder, RemoveEmptyParent and RemoveTorrentTask. `DeleteMediaFile` is manual only. Tests: `issues_failclosed.rs::files_under_media_roots_are_never_deletion_candidates`, `actions_delete.rs::revalidation_after_the_stop_restores_the_torrent_and_media_targets_are_refused`. | held |
| qBittorrent allowlist, version-aware | `domain/src/torrent.rs:153` requires `progress >= 1` and one of uploading, stalledUP, queuedUP, forcedUP, pausedUP or stoppedUP. `workflows/src/issues/index.rs:135` adds `pausedUP` on 4.x only, `stoppedUP` on 5.x only, other majors protected. The same `protection()` is used by the planner (`plans/facts.rs:89`, `plans/expiry.rs:220`) and at execution (`actions/revalidate.rs:77`). Tests: T-27, `parity_safety.rs::live_listing_allowlist_v4_*` / `v5_*`, `variants.rs::v8_*`. | held |
| Intent journal before mutation | Writer uses `synchronous = FULL` (`storage/src/db.rs:44`), so the intent is durable before `COMMIT` returns. Every crash window has a test (`workflows/tests/actions_crash.rs`, 10 cases; `integration-tests/tests/crash.rs`, 4 cases). | held |
| Auto mode off by default, pauses on uncertainty | `jobs/src/auto.rs::classify_blockers`/`assess` lets only ordinary state changes continue, and any other or unknown code pauses (CP2T5F-F01). Tests: `auto_cycle.rs::f01_*` (14 pause classes, end to end) and integration `auto_cycle.rs` (an incomplete scan pauses, the pause persists, only `POST /auto/resume` clears it). | held |
| Hardlink proof `(st_dev, st_ino)`, never `st_nlink` | `st_nlink` is stored for information only (`fs/src/inventory.rs:20`). Tests: T-01, T-02. | held |
| Full byte comparison, no NAS-wide hash | T-11, T-12, `variants.rs::v7_*`. SHA-256 is used only for the config fingerprint and the HTMX asset digest. | held |
| `BT_backup` never touched | The fs scan marks it `ProtectedReason::QbitInternal` (`fs/src/scan.rs:165`). Tests: `scan_parity.rs`, `issues_failclosed.rs`. | held |
| Confinement to declared roots | Audit run-1 (dirfd `openat(O_NOFOLLOW)`, lexical mapper). `parity_fixture.rs` shows no `outside/` name in any database file. T-17, T-26. | held |
| No shell built from paths | The only non-test `Command::new` is ffprobe (`matching/src/ffprobe.rs:117`). It takes args only, runs with `env_clear`, a fixed `PATH` and `-protocol_whitelist file`. | held |

## 3. Tier 5 gate raise (CP2T5F conditions)

| Condition (`docs/reviews/cp2-tier5-final.md`) | Evidence | Status |
|---|---|---|
| F01 fixed in the same change, with tests | `classify_blockers` is typed by blocker code. Tests: `f01_every_uncertainty_or_defect_code_pauses_and_ordinary_changes_continue`, `f01_each_pause_class_stops_the_cycle_end_to_end_and_an_ordinary_refusal_continues`, `real_runner.rs::f06_*` (real runner, `CapabilityDisabled` pauses). | done |
| `IMPLEMENTED_TIER = T5` | `crates/workflows/src/actions/mod.rs:79`. The planner uses the same constant (`app/src/wiring.rs:232`). | done |
| Tests asserting the T4 refusal rewritten | `actions_stage_c.rs::f01_an_automatic_plan_failing_an_automatic_check_is_refused_before_any_intent`, `actions_tier5.rs::t5_01_the_production_execute_accepts_a_valid_automatic_plan_and_refuses_invalid_ones`. `real_runner.rs` uses the production `execute`. | done |
| F02, F03, F04, F06, F07 | Handoff 13 lines 484-487 and handoff 14 "Gate raise" table, with tests `cp2t5f_f02_f03_*`, `cp2t5f_f04_*`, `cp2t5f_f06_*`, `f07_t4_era_refusals_do_not_block_subjects`. | done |
| `tier5-test-gate` absent from production | G5 = 0. The feature sits under `[dev-dependencies]` of `removarr-workflows` only (resolver 3). jobs no longer enables it. | done |
| Wiring by 16 | `wiring.rs` sets `implemented_tier`, `capabilities`, `auto_min_age_hours`, `ignores`, `history: StoreHistory`, `pending: PendingAuto`, and `auto_mode` from `cfg.mode.auto_mode`. | done |
| F08 and the `create_auto` text match | Kept as deviations. See D2 and D3. | accepted |

## 4. Legacy parity (T-01..T-32) and CONTEXTE §6.7 acceptance scenarios

All 32 legacy rows have Rust tests in `docs/parity-matrix.md`. This gate checked every `::test_name` cited there (104
names) against the source tree. 102 exist. The two missing names, `f01_automatic_plans_are_refused_before_any_intent_live_and_simulated`
and `tier_five_automatic_plans_are_explicit_refusals` (row T-31), were renamed or rewritten by the T5 gate raise
(CP2T5F step 3). Their replacements are listed in section 3, and integration `auto_cycle.rs` covers T-31 end to end.

The matrix bookkeeping is stale, which is a documentation-only problem (R3):
- its count line still says "4 partial";
- the integration line still lists T-18 and T-08 as remaining, although `report_dir.rs` tests and the issues CSV exist.

CONTEXTE §6.7 acceptance scenarios, mapped to evidence:

| Area | Scenario | Evidence |
|---|---|---|
| Filesystem | Same inode is a hardlink | T-01 |
| Filesystem | `nlink=2` with an outside link is refused | T-02 |
| Filesystem | Symlinks are never followed | T-17 |
| Filesystem | A change after the scan blocks | T-16, `stale_plans.rs` |
| Filesystem | A subtree error blocks that subtree | `variants.rs::v2_*` |
| Filesystem | Undeclared paths are never opened | `parity_fixture.rs` (no `outside/` name stored) |
| Filesystem | Block accounting is exact | T-01 |
| Matching | Name+size stays a candidate | T-11 |
| Matching | Samples never bypass the full compare | T-12, V7 |
| Matching | Editions are refused | T-06 |
| Matching | Episode coverage | `plans_arr.rs::replacement_needs_arr_approval_complete_coverage_*`, `domain::arr::coverage_proof_reports_missing_episodes`, `t4_09_*` |
| Matching | Absolute numbering | `plans_arr.rs::parsed_numbering_that_disagrees_with_arr_needs_a_manual_check`, `t4_10_*` |
| Matching | Real ffprobe language | `parity_blockers.rs` language cases, T-09 |
| ARR | Profile archived, score change invalidates | `plans_arr.rs::arr_score_or_profile_change_invalidates_the_plan`, `pipelines_arr.rs::an_arr_profile_change_after_planning_invalidates_*` |
| ARR | Rejected candidate never automatic | T-19, `t5_05_*` |
| ARR | No ARR means manual | `issues/arrgate`, T-19 |
| ARR | No codec hierarchy | `parity_blockers.rs::technical_tags_never_change_any_check` |
| ARR | Search never grabs | `pipelines_arr.rs::a_search_never_grabs_*` |
| qBittorrent | 4.x and 5.x | T-27, V8 |
| qBittorrent | 204 login | T-28 |
| qBittorrent | Mapping outside the root is refused | T-26, V4 |
| qBittorrent | `deleteFiles=false` | T-25 |
| qBittorrent | A partial torrent is left stopped | T-23 |
| qBittorrent | An intact torrent resumes | T-22 |
| qBittorrent | Task removed only when every planned file is gone | T-32 |
| Workflows | Dry run | T-30, `dry_run.rs` |
| Workflows | Durable intent | `journal.rs`, `actions_crash.rs` |
| Workflows | Crash between intent and result | `crash.rs`, `actions_crash.rs` |
| Workflows | Temp link then atomic replace | T-13 |
| Workflows | Net gain of a replacement | `t4_06_t4_08_a_copy_import_is_executed_unverified_with_an_added_cost`, T-03 |
| Workflows | Folder deletion is all or nothing | T-21, T-15 |
| Workflows | Samples | Section 2 |
| Workflows | Two concurrent imports of one media | `pipelines_arr.rs::two_plans_for_the_same_media_import_at_most_once`, `t4_04_*` |

## 5. Migrations, container and configuration

- **Migrations.** `0001_observations.sql` and `0002_plans_journal_jobs.sql` are covered by:
  - `fresh_database_has_pragmas_tables_and_clean_integrity`;
  - `reopening_is_idempotent_and_keeps_data`;
  - `a_database_from_a_newer_version_is_refused`;
  - integration `a_fresh_database_is_migrated_and_a_restart_changes_nothing`;
  - `a_database_from_a_newer_build_is_refused_at_startup`.

  A modified released migration is refused (handoff 04). There are **no upgrade fixtures** because no version has been
  released. This release creates the baseline, so 0001 and 0002 become immutable once tagged.
- **Container** (not run by this gate, as instructed). Handoff 23 records the wave-16 smoke test, 29/29:
  - static musl binary 14 MiB, image 55 MiB;
  - runs as uid 1000:1000 with `--read-only`, `cap-drop ALL`, `no-new-privileges` and CapEff 0;
  - port on 127.0.0.1 only;
  - HEALTHCHECK `healthy`;
  - 180 real hardlinks under a single mount;
  - tree fingerprint unchanged by the scan;
  - no secret in the logs.

  Static review at 38a2294:
  - the Dockerfile has no root process (`USER 65532:65532`, no entrypoint script, no PUID/PGID);
  - `/config` is `0700` and owned by 65532 (wave 19);
  - `healthcheck` loads no config and only sends `GET /healthz` on loopback;
  - compose has `user: "${UID}:${GID}"`, one parent mount `/srv/media:/data`, `127.0.0.1:8012:8080`, a read-only
    rootfs, tmpfs `/tmp`, `cap_drop ALL` and `no-new-privileges`.

  The smoke test has **not** been re-run since the wave-19 Dockerfile change (C3).
- **Configuration examples.** `config/tests/load.rs::shipped_examples_load` loads `examples/removarr.toml`, and
  `env_example_has_no_values_for_secrets` checks `.env.example`. The compose file was validated against a fresh
  `.env.example` copy (handoff 20 follow-up).

## 6. Recorded deviations: decisions

| ID | Deviation | Decision | Justification |
|---|---|---|---|
| D1 | `workflows` keeps the `tier5-test-gate` feature for its own tests | **Accept** | The feature is a dev-dependency only, and G5 shows it is absent from the production tree. With `IMPLEMENTED_TIER = T5`, `execute_with_gate(.., T5)` equals `execute`. Adding the G5 check to CI is recommended (R2). |
| D2 | F08: the automatic cycle rescans every root after a live phase-1 mutation | **Accept** | Correct and conservative, because phase 2 plans from fresh data. The I/O cost happens only when automatic mode is on (off by default) and a live action touched a root. Owner 11 for a root-restricted scan (R9). Not a data-safety issue. |
| D3 | `create_auto` refusal classified by text ("n item(s) excluded") | **Accept** | Fails closed: unknown text pauses. The benign case applies only when the last scan or rescan has no perimeter uncertainty, and `Refusal` now carries typed blockers, where any pause-class blocker stops the cycle (`f05_*` test). Typed planner reasons are a follow-up (R5). |
| D4 | RPT-05, RPT-08 and RPT-09 legacy report files not produced | **Accept** | MASTER excludes importing the old reports and does not require their file formats. The same facts are in the UI and the exports: categories, evidence, probe blockers, issues/history CSV, JSON, MD, HTML. Missing columns: nlink/links-seen (RPT-05), per-pair duration JSON (RPT-08), DR-36 audit folder (RPT-09). List them in the release notes. |
| D5 | `/settings` and `/explorer` not built; `/media/{service}/{id}` exists but no page links to it | **Accept, list in the release notes** | `dry_run`/`auto_mode` are visible through the dry-run banner and the automatic-mode page, so the read-only settings page is not needed for safety. The explorer (CONTEXTE §6.2, CTX-01) is a read-only capability gap with no safety impact; the issue lists and details carry search and evidence. The media page is reachable by URL only. Grab stays gated by `arr_grab` (off by default), a fresh search, an opaque token and plan approval. Not a MASTER §2 invariant. |
| D6 | Numbers and dates not localized (binary units with a dot, UTC ISO times) | **Accept** | MASTER §7 asks for EN by default, FR available and external `locales/*.json` with fallback. That holds: 583 keys, equal sets. Localized number and date formatting is not required. |
| D7 | testkit `fake_qbit::parity_preset` does not match the parity tree | **Accept** | Test-only helper. The integration harness uses its own exact `parity_torrents`. No production impact. Follow-up for 22 (R8). |
| D8 | Requeued job id not linked from the interrupted job view | **Accept** | UX only. The interrupted job is persisted and visible with a localized critical alert, and the re-queued scan runs (handoff 23 verified convergence). |
| D9 | 320 px layout not re-checked in a real browser after the wave-16 selection-bar fix | **Accept with condition C6** | Covered by CSS and markup tests (44 px `--tap`, breakpoints, `sel-more`, 55vh cap) and earlier headless screenshots at 320/360 px. MASTER asks for full smartphone usability, so the user should do a real-device check before tagging. |
| D10 | Dashboard lacks the CONTEXTE §6.2 extras: capacity/free, ARR queue counts, problem qBittorrent states. `torrents_left_stopped` is hard-wired to 0 (`web/src/handlers/dashboard.rs:146`). | **Accept, list in the release notes** | The MASTER §6 dashboard metrics are present. Left-stopped torrents stay stopped in qBittorrent. The plan is persisted as `PartiallyExecuted`, its actions show `ExecutedUnverified`, and automatic mode pauses on them. So they stay visible, but not as a dashboard counter. Follow-up for 14/15/16 (R7). |
| D11 | `docs/parity-matrix.md` bookkeeping stale after the T5 raise | **Accept** | Evidence exists (section 4). Documentation fix only (R3). |
| D12 | Handoff 04 says a sanitized `rel` path leaf makes recovery "see an unobservable target". In fact a sanitized leaf still deserializes, stats as absent and is labelled `Mutated` (`recovery.rs:734`). | **Accept as a residual (LOW)** | Affects only file names containing `://` or a sensitive `key=` pair, and only after a crash inside the intent/result window. Outcome: `ExecutedUnverified` plus an extra recheck. Recovery never mutates and no gain is counted. Fix recommended (R1). |
| D13 | User-provided `old/` (tracked since the initial commit) contains real NAS paths (`/volume3/chill/...`). Audit artifacts (`docs/security/*.json`, ledger) are tracked as evidence copies. | **User decision (C5)** | No credential, IP address, hash or tracker domain was found. `old/` is excluded from the image by `.dockerignore`. Whether these belong in a public repository is the owner's call. |
| D14 | README has no screenshot (MASTER §8), and its license section says "To be defined" | **Condition C5** | Documentation deliverables. The autonomous build had no real browser. The license is an owner decision. |
| D15 | CI uses Rust 1.91.1, while this gate ran on 1.98.0 | **Condition C1** | Lint sets and MSRV behaviour differ, so only a green CI run on the exact commit proves the pinned toolchain. |

## 7. Performance budgets (architecture §14)

The latest measurements come from handoff 11 "Wave 16 fixes" and handoff 23. They were taken on macOS arm64 with the
system allocator, the release build and the reference fixture: 24,360 files, 3,000 torrents, 2,000 movies.

| ID | Target | Measured | Result |
|---|---|---|---|
| B1 idle RSS | <= 40 MiB | **68.7 MiB** on macOS (30.7 MiB right after start). Linux/musl container, fs-only 20k files: 22.7 MiB | **MISS on host**, conditional waiver (below) |
| B2 peak, first scan | <= 160 MiB | 130.9 MiB | PASS |
| B2 peak, unchanged rescan | <= 160 MiB | **209.7 MiB** on macOS (container fs-only peak: 87 MiB) | **MISS**, conditional waiver (below) |
| B3 idle CPU / zero fs calls while idle | <= 1 %, roots untouched | 0.00 %, roots renamed away and untouched | PASS (cannot be waived) |
| B4 scan / rescan | <= 60 s / <= 30 s | 3.92 s / 3.37 s | PASS |
| B5 db writes | <= 64 MiB / <= 4 MiB | 43.2 MiB (64.9 MiB with WAL) / 0.00 MiB | PASS |
| B6 | zero fs reads from refresh and UI | covered by the B3 run | PASS |
| B7 dashboard / list 50 / fragment | <= 100 / 100 / 50 ms | 1.11 / 16.3 / 2.69 ms (p95; jobs and history 0.1 ms) | PASS |
| B8 binary / image | <= 30 / 250 MiB | 14 MiB static musl / 55 MiB | PASS |
| B9 startup | <= 3 s | 0.02-0.5 s | PASS |
| B10 log lines / ring | <= 200 / 2,000 | 0 per scan / 2,000 | PASS |
| B11 writes in roots | 0 | 0 | PASS (cannot be waived) |

**Decision: conditional waiver for B1 and B2-rescan. Not BLOCKED, because neither is a data-loss budget.** B3 and
B11 pass.

**B1.** The shipped target is the Linux/musl container. On macOS, about 38 MiB of the 68.7 MiB is freed-but-resident
heap from the scan peak, held by the system allocator (live data after a scan is small). The only Linux measurement
(22.7 MiB) used a fixture without torrents. The waiver is therefore granted for the host figure only. **Condition C2:**
measure B1 on Linux with the musl release binary and the full reference fixture. If idle RSS there is above 40 MiB, the
waiver lapses and the release is **BLOCKED** until memory is reduced or the architect decides on an allocator.

**B2 rescan.** The peak is 31 % over budget. It is transient, occurring only during a scheduled or manual rescan, and the
first full scan is within budget. The cause is known: the whole previous match set is passed to `matching::run_funnel`
(owner 07), plus the classifier indexes in `workflows/issues` (owner 11). It carries no correctness or I/O risk.

The waiver ceiling is **210 MiB**, the measured host value. **Condition C2** also covers the Linux rescan peak:
- at or below 160 MiB: PASS;
- between 160 and 210 MiB: the waiver stands, and the release notes must list it as a known limitation;
- above 210 MiB: **BLOCKED**.

Reducing the peak is a follow-up (R9).

## 8. Security status

| Item | Status |
|---|---|
| Full audit, run-1 at cb62ef1 (standard profile, 38 units) | **0 confirmed**. 1 needs-validation lead, 1 rejected. Source-only. |
| Wave-19 fixes B1, B2, B4, B5, B6 (handoff 26) | Re-audit at 4adeac3: PASS. B3a is not a defect, and leaving it unchanged was correct. |
| Re-audit confirmed LOW `storage-journal-scrub-mangles-path-leaves-shared-sanitizer` | **Fixed in 817eedc.** `storage/src/codec.rs::scrub_value` keeps `rel` leaves verbatim unless they contain `://` or `=`, so URLs and secrets elsewhere are still redacted. Tests: `codec.rs::rel_path_leaves_are_verbatim_but_urls_and_secrets_are_not` and `tests/journal.rs::journal_paths_roundtrip_exactly_but_urls_and_secrets_are_redacted`, both passing in G3. `RootedPath` serializes as `{root, rel}` (`domain/src/path.rs:200`), so the key match is correct. Narrow residual and an inaccurate handoff statement: D12 / R1. |
| Lead `web-login-limiter-shared-bucket-without-trusted-proxies` | **Mitigated, still deployment-dependent.** The limits are unchanged: fail closed, the limiter is reserved before Argon2, and the per-IP 20 and per-(IP, user) 5 windows are enforced. Mitigations: a once-per-process security warning when `X-Forwarded-For` comes from an untrusted peer, plus docs in the README, `.env.example` and the example config. The local behaviour is now proven by `auth_flow.rs::empty_trusted_proxies_behind_a_proxy_share_one_bucket_and_trusting_the_proxy_fixes_it` (passed in G3), which closes the re-audit TODO to run it locally. This is login availability, not an authentication bypass: sessions, CSRF and Argon2 are unaffected. Operator action: C4. |
| Sessions, CSRF, redaction, proxy trust | Pass in G3: `auth_flow` 20/20, `auth_http` 6/6 (rotation, fixation, idle/absolute expiry, CSRF on every command route, both limits with `Retry-After`), `auth_listener` 3/3 (spoofed XFF ignored), redaction tests in domain, storage, jobs and reporting. |
| B3b leftover `.removarr-link-*` temp links | **Open, LOW, accepted.** Conservative: an extra link only blocks deletions, it lives in a media root where nothing is ever auto-deleted, and recovery notes name it (`actions_crash.rs::f06_leftover_temporary_links_are_named_in_the_recovery_notes`). A sweep needs a new destructive port method and a CP2-style review (R6). List it in the release notes. |
| Dependency CVEs | **Not checked here** (`cargo deny` needs the network). CI `audit` and the release `verify` job run `cargo deny check advisories bans sources` before any publish, so a vulnerable lockfile cannot reach GHCR through the tag workflow. Required green run: C1. |
| Licenses | Inventory in G7: all permissive. `deny.toml` has no `[licenses]` policy, so CI does not enforce this (R4). The project has no license of its own (C5). The runtime image ships Alpine `ffmpeg` (GPL/LGPL): source obligations are met through Alpine's public package sources (assumption). |
| SBOM and provenance | `release.yml` passes `sbom: true` and `provenance: mode=max` to `docker/build-push-action`. Trivy scans an amd64 build and fails on fixable HIGH/CRITICAL before login and push. |
| CI permissions and pinning | Workflow-level `contents: read`. `packages: write` only on the `publish` job. No `pull_request_target`. `persist-credentials: false`. All actions pinned by 40-hex SHA with version comments. CI never pushes (`push: false`). |
| Release tag behaviour | `release.yml` triggers only on `push: tags: ["v*.*.*"]`, publishes only after `verify` (fmt, clippy, test, deny), and moves `latest` only for tags without `-`. Semver tags come from the git tag (Cargo `version = 0.1.0`). Branches, PRs and Markdown pushes never publish (`ci.yml` `paths-ignore`). |
| Supply chain of pinned actions | **Not verifiable offline.** There were public reports in 2026 of a tag-hijack compromise of `aquasecurity/trivy-action` and `setup-trivy`. A SHA pin protects only if the SHA is an original commit and the composite action does not itself call other actions by a mutable tag. `trivy-action` v0.28.0 runs in the job that holds `packages: write`. Check: C4. |
| Secrets and tracked files | Tracked files contain no `.env`, database, key or certificate. The only Argon2 strings are dummy test hashes. The only passkey-like literal is a planted test value in `testkit/src/fake_arr.rs`. No private IP addresses. `/Users/...` appears only in handoff prose (24, 25). Real NAS paths appear only in the user-provided `old/` (D13). |

**No confirmed vulnerability remains open.** The only confirmed finding (LOW) is fixed. The narrow residual in D12 is
not a vulnerability: no mutation, no secret, no confinement impact. Nothing in data loss, authentication, secrets or
path confinement is waived.

## 9. Conditions before the user tags a release

The user pushes and tags. This gate did neither.

| ID | Condition | Owner | Fails to |
|---|---|---|---|
| C1 | Push the branch and get a **green `ci.yml` run on the exact commit to tag**. This covers fmt, clippy `-D warnings`, test and the workspace release build on Rust 1.91.1, `cargo deny check advisories bans sources` (closes the CVE gap), and the Docker job (amd64 build, non-root `id -u`, `ffprobe -version`, healthcheck fail-fast). Fix any advisory before tagging. | user / owners of the failing crate | **BLOCKED** |
| C2 | Linux B1/B2 measurement with the musl release binary and the full reference fixture. Example: `cargo build --release --target <arch>-unknown-linux-musl -p removarr`, then `PERF_BIN=<that binary> PERF_IDLE_SECS=60 cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1` on a Linux host or VM. Use loopback fakes only. B1 must be <= 40 MiB. The B2 rescan must be <= 210 MiB (waiver ceiling, section 7). | user (owners 07/11/14 if it fails) | **BLOCKED** |
| C3 | Re-run `crates/e2e-perf/scripts/container-smoke.sh` after the wave-19 Dockerfile change (`/config` 0700). Expect 29/29. Also confirm that a bind-mounted `./config` owned by `${UID}` is writable, and that a named volume for another uid fails closed with a clear error. Stop Colima afterwards (incident rule). | user (owner 20 if it fails) | **BLOCKED** |
| C4 | Supply chain and deployment hardening before the first publish: (a) check every pinned action SHA against its upstream tag, review the `uses:` lines inside `aquasecurity/trivy-action@915b19b…`, and bump or replace it if it pulls a mutable or compromised ref; (b) add tag protection so only the owner can push `v*.*.*`; (c) behind a reverse proxy, set `REMOVARR_TRUSTED_PROXIES` to the proxy address and keep `REMOVARR_COOKIE_SECURE=true`. | user (owner 20 for a workflow change) | **BLOCKED** for (a); (b) and (c) are operator settings |
| C5 | Owner decisions and docs before a public release: choose a license (README "To be defined"; add `LICENSE` and the `license` field), add the README screenshot (MASTER §8), and decide whether `old/` (real NAS paths) and `docs/security/*` stay in the public repository. | user | not a gate failure, but required before announcing |
| C6 | Real smartphone check at 320-390 px: dashboard, issue list with the selection bar and `sel-more`, plan confirmation, tab bar. | user (owner 19 if it fails) | fix before tagging if unusable |
| C7 | Release notes must list the known limitations: D4, D5, D10, the B2 waiver (if it applies), B3b, D12, D2 (full-root rescan in automatic mode), D6, and the 7-day failed-subject window. Also: start in dry run (the default) and enable automatic mode only after reviewing dry-run plans. | user | — |

## 10. Recommended follow-ups (not gating)

- **R1** (04/13): make recovery return `Unobservable` when a stored `rel` was sanitized, and correct the handoff 04 text.
- **R2** (20): add `cargo tree -p removarr -e normal,features | grep tier5` (must be empty) to CI. Optionally remove the
  feature (13).
- **R3** (21): refresh `docs/parity-matrix.md`: T-31 test names, count line, T-08/T-18 line.
- **R4** (20): add a `[licenses]` allowlist to `deny.toml` and run `cargo deny check licenses` in CI.
- **R5** (12/13/14): typed sub-reasons for `ChangedSincePlanning`, `Ignored` and `ArrNotUpgradeEvidence`, and typed
  `create_auto` refusals. The F01/F05 classification still reads detail text for those codes, and an unrecognized
  `ChangedSincePlanning` detail continues the cycle. Each plan is still fully revalidated, so this is not a data risk.
- **R6** (13 + CP2-style review): temp-link sweep (B3b).
- **R7** (14/15/16): persist the left-stopped set, show it on the dashboard, and fill the CONTEXTE dashboard extras.
- **R8** (19/16/22): explorer and settings pages, links to the media page, requeued-job link, number and date
  localization, testkit `parity_preset`.
- **R9** (07/11): stream the previous match set, shrink the classifier indexes, add a root-restricted rescan (F08).
- **R10** (various): audit hardening notes:
  - `Secure` on the locale and flash cookies;
  - connection caps and a header-read timeout;
  - ctime in `EntrySignature`;
  - keyed plan hash;
  - redaction forms;
  - compose mem/pids limits;
  - qBittorrent N+1 requests per scan.

## 11. Residual assumptions

- The Linux/musl memory behaviour (C2) is assumed, not shown, to be at least as good as macOS.
- Container behaviour after the wave-19 Dockerfile change is assumed from static review (C3).
- Upstream action SHAs and the dependency advisory status are unverified offline (C1, C4).
- Real HDD cold-cache behaviour, a real second filesystem, real ffprobe, real browsers and real services were never
  exercised. Only loopback fakes, temp trees and dummy secrets were used, as MASTER §10 rule 9 requires.
- Proxy deployment facts (whether a proxy exists, what `trusted_proxies` is set to, the Docker peer address) are unknown
  to this gate.
- Clippy G2 relied on cargo's cached diagnostics, valid because no source changed since the last full build.

## Post-gate updates (orchestrator, after CP3)

- **C3 container smoke, run on 2026-11-17: first FAILED (9/29), then PASSED (29/29) after a fix.** Cause: the wave-19
  Dockerfile change (`/config` owned by 65532, mode 0700) is copied by Docker onto an empty named volume at every
  mount, so the app running as another uid (`--user 1000:1000` in the smoke test, `user: "1030:100"`-style in
  compose with a named volume) could not open `/config` and exited with `config_file: cannot be opened`. This
  confirms the re-audit's NEEDS-VALIDATION note on B6. Fix: `/config` back to mode 1777 (sticky) with a comment
  explaining why; the default compose bind mount is unaffected either way. The audit's 1777 remark stays a
  documented, accepted hardening trade-off (single-process container, runtime uid unknown at build time).
- **C4(a) action pins, checked on 2026-11-17 (read-only GitHub API):** the 6 actions `actions/checkout`, `docker/build-push-action`,
  `docker/login-action`, `docker/metadata-action`, `docker/setup-buildx-action` and `docker/setup-qemu-action` match their upstream release tags.
  `aquasecurity/trivy-action` pinned SHA matched v0.28.0, but that version calls `aquasecurity/setup-trivy@v0.2.1`
  and `actions/cache@v4` by mutable tag, and the `setup-trivy` `v0.2.1` tag **no longer exists** (old tags removed;
  oldest remaining release v0.2.6, 2026-03-19). The release scan step would have failed, and mutable inner tags
  defeat the SHA pin. Bumped to trivy-action v0.36.0 (`ed142fd`, released 2026-04-22), which pins
  setup-trivy v0.2.6 (`3fb12ec`) and actions/cache v5.0.5 (`27d5ce7`) by SHA (both verified against their tags);
  inputs `image-ref`, `severity`, `ignore-unfixed`, `exit-code` unchanged. C4(b) tag protection and C4(c) proxy
  setting remain for the user.
- **C2 Linux/musl full-fixture measurement, run on 2026-11-17: PASS.** Static musl binary built like the Dockerfile
  (`rust:1.91.1-alpine3.22`, `+crt-static`, `--locked`), `perf_budgets` run inside the container on loopback only,
  same reference fixture (24,360 files, 8,585 dirs, 1,680 hardlinks, 3,000 torrents, 2,000 Radarr movies).
  B1 idle RSS **26.1 MiB** (limit 40), B2 first scan **96.3 MiB** and unchanged rescan **106.5 MiB** (limit 160),
  B4 8.2 s / 4.5 s, B5 39.1 / 0.12 MiB, B3 0 % CPU and 0 idle log lines, B7 dashboard 2.0 ms, issue list 8-25 ms,
  selection summary 5.6 ms, B9 0.29 s, B10 0 lines, B11 0 entries changed, Argon2 verify p95 23.6 ms. The macOS
  B1/B2 misses were host allocator behaviour; no waiver is needed on the target platform.
- **C1 CI, 2026-11-17: PASS** on `main` at `f76dd8c` (run 44130981688: dependency advisories, fmt/clippy/test/release
  build, image build all green). Fixes needed on the way, all found only on the Linux/amd64 runner with Rust 1.91.1:
  clippy `unnecessary_cast` (`st_mode` is u32 on Linux) and three `nonminimal_bool` lints (rewritten with identical
  logic, including the torrent-removal approval check); the Dockerfile static check (`ldd` false failure on amd64
  static-pie, replaced by `readelf` checks for an interpreter and `NEEDED` entries); cargo-deny 0.18.5 could not parse
  CVSS 4.0 advisories (now 0.20.2); runner disk full while building test binaries (CI and release workflows now build
  without debug info or incremental compilation). cargo-deny policy: internal path dependencies allowed
  (`allow-wildcard-paths`, every crate `publish = false`), and two documented advisory exceptions without an upstream
  fix: RUSTSEC-2026-0459 (bundled SQLite < 3.54; removarr only opens its own database; no sqlx release accepts
  libsqlite3-sys >= 0.39 yet: re-check at every sqlx update) and RUSTSEC-2026-0374 (basic-toml unmaintained,
  compile-time only via askama_derive).
- **C5 license: done.** AGPL-3.0-or-later (`LICENSE`, workspace and crate manifests, README). README rewritten with
  step-by-step installation. Still open for the user: README screenshot, and whether `old/` (real NAS paths) and
  `docs/security/` stay if the repository becomes public.
- **C4(b) tag protection: not available.** Tag rulesets need GitHub Pro or a public repository (API: "Upgrade to
  GitHub Pro or make this repository public"); the legacy tag-protection API no longer exists. Mitigation while the
  repository is private: the owner is the only collaborator (no invitations, no deploy keys). Apply the ruleset
  (creation/update/deletion of `refs/tags/v*.*.*`, admin bypass) when the repository becomes public or moves to Pro.
- **First release v0.1.1, 2026-11-18: published.** v0.1.0 never published (QEMU-emulated arm64 build exceeded the
  90-minute job limit; tag left in place, nothing deleted). Release workflow now builds natively per architecture
  (`ubuntu-24.04`, `ubuntu-24.04-arm`), scans each image with Trivy, pushes by digest with SBOM and provenance, and
  merges one OCI index: `ghcr.io/garnajee/removarr-v2:0.1.1` and `:latest` (linux/amd64, linux/arm64; run
  44350181742, index `sha256:dae03df01f35128433c301f0b37f47a167dd4a79e11d8290361a14b92d4f50d7`). The package is
  private like the repository: `docker login ghcr.io` is needed to pull it.
