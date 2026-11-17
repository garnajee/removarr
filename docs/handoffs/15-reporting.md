# Handoff — 15-reporting (wave 11)

## Delivered scope

`crates/reporting/**` only. No change to Cargo manifests, other crates or the root `Cargo.toml`.

| Module | Contents |
|---|---|
| `reporter` | `Reporter` (read-only SQL read model), `DashboardInputs`, `IssueQuery`, `HistoryQuery`, `StatsQuery`, `compute_urgency` |
| `model` | All view models (serde, no secrets, ISO time next to every ms instant) |
| `explain` | Structured explanations: incomplete data, plan vs result differences, remaining actions, Btrfs caveat |
| `export` | `render(bundle, ExportFormat) -> ExportFile`, `to_json`, `issues_csv`, `history_csv`, `series_csv`, `to_markdown`, `to_html`, `csv_cell`, `md_escape`, `html_escape` |
| `logs` | `LogLevel`, `LogBuffer` (bounded ring, default 2000), `LogLayer` (tracing layer: sanitized JSON lines to stdout or any writer, plus the buffer), `LogQuery`, `LogsVm` |
| `redact` | `sanitize`, `sanitize_json`, `Redactor` (registered exact secret values), `truncate`, `is_sensitive_key` |
| `svg` | `chart_geometry(series, w, h) -> ChartGeometry` (coordinates only) |
| `report_dir` | `ReportDir::new(data_dir, roots)`, `write(name, bytes)` (T-18) |
| `timefmt` | `iso(ms)` exact UTC with milliseconds |

## Public contracts

### Query API (backend 16)

```rust
let rep = Reporter::open(&data_dir.join("removarr.db"), clock).await?;   // own read-only pool, 2 connections
rep.dashboard(&DashboardInputs { scan_interval_minutes, auto_enabled, dry_run, recovery_pending,
                                 torrents_left_stopped, live_services }) -> DashboardVm
rep.issues(&IssueQuery { category, min_severity, offset, limit }) -> IssueListVm   // limit <= 1000
rep.history(&HistoryQuery { offset, limit, plan, session, origin, include_dry_run }) -> HistoryVm
rep.stats(&StatsQuery { since, max_points }) -> StatsVm                            // <= 730 daily points
rep.plan_report(plan_id) -> PlanReportVm          // planned vs recorded, differences, remaining
rep.auto_status(enabled) -> AutoVm                // paused flag + sanitized reason (T5-19)
rep.session(Some(id) | None, interval) -> Option<SessionVm>
rep.report(Option<SessionId>, &inputs) -> ReportBundle ; export::render(&bundle, ExportFormat::{Json,CsvIssues,CsvHistory,CsvSeries,Markdown,Html})
```
`/export/{session}/{json|csv|md}`: csv maps to `CsvIssues`; add `html`, `csv history` and `csv series` as extra
variants if wanted. The file name (`removarr-<kind>-<stamp>Z.<ext>`) and content type come back in `ExportFile`.
Errors are `PortError` (never sqlx text).

### Logs (app 16 and web 19)

```rust
let buf = LogBuffer::new(DEFAULT_CAPACITY, level);          // level from config log_level
buf.redactor().add_secret(cfg_secret.expose());              // register API keys, passwords, session key at startup
tracing_subscriber::registry().with(LogLayer::stdout(buf.clone()))   // + EnvFilter if RUST_LOG is wanted
buf.view(&LogQuery { min_level, q, after_seq, limit }) -> LogsVm       // <= 500 entries, newest last
buf.set_capture_level(..)                                    // runtime DEBUG/INFO/WARN/ERROR
buf.push(level, target, message, fields)                     // for job events (the jobs `Hub` log events) if backend forwards them
```
TRACE is dropped. Messages, targets, field names and values are sanitized, bounded (2048/128/512 chars, 16 fields)
before storage and before the stdout write. The stdout line is written per event (stdout is line-buffered; the
ring is the bound, nothing accumulates).

### View model notes for UI

- `DashboardVm.urgency` has `level`, `label_key`, `label`, `icon` and `reasons` (never colour only).
- `SpaceVm` carries logical, unique allocated, shared and potential separately; `potential_label_key =
  "space.potential_not_guaranteed"` must be shown with the potential number. Potential counts each inode once across
  issues (greedy by potential then id); `potential_overlap_excluded_bytes` is what was left out. Per-issue
  `potential_counted_bytes` shows what counted.
- Historical data: `ServiceVm.current=false` unless `live_services` is given; `EvidenceVm.current` is true only for
  filesystem evidence of the latest session; every row has `*_iso` timestamps. `IssueListVm` says which session the
  issues are a snapshot of.
- `ExplanationVm { code, params, text }`: translate `code`+`params`; `text` is the English export fallback.
  Codes: `scan.none|complete_with_errors|not_complete|root_not_scanned|root_errors|stale`, `issues.from_older_session|incomplete_evidence`,
  `service.not_ok`, `auto.paused`, `plan.dry_run|not_finished|unfinished_intents`, `action.not_verified`,
  `gain.not_recorded|differs_from_plan|total_differs_from_plan`, `cost.differs_from_plan`,
  `observed.differs_from_attributable|missing`, `caveat.btrfs_snapshots_may_retain_space`.
  `RemainingVm.next_step`: `wait_or_recover|run_or_discard|verify_manually|review_manually|rescan_and_replan`.
- Series: `potential` (single current point, dated at the issues session end), `expected` (live plans in
  draft/approved/executing/executed/partially_executed/interrupted at `created_at`), `attributable` and `added_cost`
  (from `gains`, each `gain_id` once; `GainTotalsVm.duplicate_gain_ids_ignored` is the guard counter), `observed`
  (`free_after_local - free_before`, may be negative, never added to attributable). Only attributable/added are `guaranteed`.

### SVG: who renders

`reporting::svg::chart_geometry` computes coordinates, ticks, dash pattern and marker per series. It produces **no
markup**. **web-ui should reuse it** (call it from the `/stats/chart/{kind}.svg` handler and write the `<svg>` in a
template), not recompute scales. Series are also distinguishable by dash and marker, and the same `SeriesVm.points`
feed an accessible table.

## Deviations and gaps

1. **Own read pool.** `Db::reader()` is `pub(crate)` in storage, so `Reporter::open(path, clock)` opens a second
   read-only pool (2 connections). `Reporter::from_pool` exists if storage later exposes one.
2. **Dashboard fields not available to reporting**: root capacity/used/free, ARR queue counts, problem torrents,
   torrents left stopped, running-job counters. Left-stopped is a `DashboardInputs` field (not persisted anywhere);
   capacity needs an fs probe from 16. `running_jobs` reads queued/running `jobs` rows (stage only).
3. **Issues are a single current set** (storage keeps no per-session issues). An export for another session says so
   (`issues.from_older_session`) instead of presenting them as that session's.
4. **No scan = Red** (`scan_missing`): a fresh install shows Red until the first scan. Ignored issues are excluded
   from counts and urgency (`ignored_issues` is shown).
5. **Incomplete-evidence count** in the dashboard uses a text match on the canonical evidence JSON
   (`"completeness":"Unknown"`, `"Incomplete":`); a JSON string cannot contain those unescaped, so no false positives.
6. **History is action-level** (`plan_actions` joined to `plans`, `approvals`, `gains`); the target comes from the plan body
   (`actions[i].target`: only rooted paths and torrent hashes are shown). Reason = origin + approver + approval time
   (plans carry no per-action reason text). Simulated runs are not in history (13 records no outcome for them).
7. **Legacy CSVs**: only the issues CSV (RPT-02-like), history and series CSVs exist. RPT-05 columns nlink, links seen
   in tree, possible external links and parsed fields are not exported (not in the issue row). RPT-08 duration-check
   and RPT-09 directory layout are not produced; `ReportDir` is provided for writing files if backend wants them.
8. Exports are rendered in memory (bounded: 1000 issues, 1000 history rows, 50 plans), not streamed.
9. `redact` is a local copy of the rules (reporting must not depend on config); it also redacts `/announce` paths.
   `Redactor::add_secret` must be fed by the app for exact-value removal.
10. No log batching beyond the ring: the only write path is a single line to stdout; reporting writes nothing to SQLite.

## Open TODOs

- 16: construct `Reporter`, `LogBuffer`/`LogLayer` in app, feed job events with `LogBuffer::push` if desired, pass `DashboardInputs`
  (left-stopped from the runner report, recovery flag from `RuntimeStatus`), handle `/export`.
- 19: templates for the view models, SVG markup from `chart_geometry`.
- 04: expose the reader pool (`Db::reader_pool`) to drop deviation 1; add an `arr_queue_items` table if the ARR queue
  dashboard block is required.
- 22: legacy parity for RPT-05/08/09 if kept.

## Commands run and results

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test --workspace`: pass, 838 passed, 0 failed (reporting: 32 unit + 8 integration in `tests/read_model.rs` on a
  real SQLite file: space quantities and potential dedupe, incomplete/stale sessions, empty db, auto pause reason,
  history, stats and unique gain ids, plan report, export escaping and secret sweep over JSON/CSV/MD/HTML).
- Parity matrix: T-08 (CSV export) and T-18 (report directory) updated. Nothing committed.

## Wave 16 fixes

Changed files (reporting only): `crates/reporting/src/logs.rs`, `crates/reporting/src/reporter.rs`,
`crates/reporting/tests/read_model.rs`.

1. Log ring ordering. `LogBuffer::push` now assigns `seq` (`next_seq.fetch_add`) while holding the ring mutex, so
   ring order is always seq order and `after_seq` polling cannot skip entries. New unit test
   `concurrent_pushes_enter_the_ring_in_seq_order` (8 threads x 2000 pushes, asserts strictly increasing seq in the
   ring and in `view`). The e2e test was not touched.
2. B7 per-request cost. `Reporter` holds a `Cache` with two slots: the derived issue cores (including the
   `evidence LIKE` incomplete flag) and the `fs_entries` space aggregates (roots + distinct-inode totals). Key =
   `PRAGMA data_version` read on one dedicated read-only connection (built from the pool's connect options).
   SQLite bumps it whenever another connection commits (scan, evaluation, attach_evidence, action result, job
   write), so any commit invalidates; the version is read before the queries, so a racing commit costs one extra
   recompute and never serves stale data. If the probe fails, caching is skipped (slower, never wrong). Ignores are
   never cached (small query, partitioned per request), so ignore/restore are immediately visible. Per-request cost
   after a warm cache no longer depends on evidence size or `fs_entries` rows. Cost remaining: one recompute after
   each commit (the first request after a scan/evaluation, and after unrelated writes such as job progress).
   Test: `cached_aggregates_follow_every_commit_ignore_resolution_and_new_files` (repeat read, new fs entry,
   ignore, restore, issue resolved, file gone).
3. Selection-summary fragment. Computed by `crates/web/src/handlers/issues.rs::selection_summary` ->
   `find_issues` -> `Reporter::issues` (so it was paying `issue_cores()` + `counted_potential` over all active
   issues per request). The cache removes the evidence scan; the remaining per-request work (greedy
   `counted_potential` O(n log n) over all cores, 50-id SQL) is in reporting and unchanged. No web change needed.

Not routed: no schema column/index change was required. Optional future improvement for owner db-layer (04) /
issue-engine (11): persist an `incomplete` flag column at issue write time to make the cold (post-commit) recompute
cheaper.

### Verification status (INCOMPLETE, read this)
- Before the cache: `cargo test -p removarr-reporting` passed (33 unit, 6 followup, 9 read_model incl. the new tests)
  and `cargo fmt -p removarr-reporting` was applied.
- Not completed: the measurement `perf_budgets` (B7 before: dashboard p95 146 ms, list 118 ms, summary 54 ms, from
  handoff 23) and the workspace gate (`cargo fmt --check`, `clippy --workspace`, `test --workspace`). During this
  session newly spawned processes (cargo, rustfmt, ugrep) hung at 0% CPU, and an orphaned `cargo test ...
  perf_reference` (ppid 1) and a stale `target/release/removarr serve` from another run were present, so the
  runs could not finish. B7 after numbers are therefore NOT measured. Re-run:
  `cargo build --release -p removarr && cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1`
  with no other cargo/removarr processes running.
