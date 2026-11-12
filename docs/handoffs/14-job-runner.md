# Handoff — 14-job-runner (wave 10)

## Delivered scope

`crates/jobs/**` only (plus the dev-dependency edges in `crates/jobs/Cargo.toml` and the matching `Cargo.lock` lines).
Nothing in domain, workflows, storage, root `Cargo.toml` or docs other than this file was touched. Normal dependencies are
unchanged (`removarr-domain`, `removarr-workflows` and the existing third-party crates); the dependency gate for jobs shows
only domain, matching (through workflows) and workflows.

| Module | Contents |
|---|---|
| `runtime` | `JobRuntime`: persistent queue, one Observe worker and one Execute worker (ADR-0007), scheduler task, restart restoration, cancellation, bounded retries, recovery gate, command surface |
| `schedule` | `next_anchored` (interval plus UTC anchor), `next_every`, `parse_anchor`. Pure; the scheduler sleeps until the next due instant (no polling) |
| `backoff` | `delay`, `retry`, `Jitter` (`RandomJitter` over `getrandom`, `NoJitter`); 1 s, 2 s, 4 s, plus or minus 20 percent, at most 3 retries |
| `limits` | `Limits` semaphores: scan 1, compare (`content_read_concurrency`, max 4), probe (`ffprobe_concurrency`, max 4), API per service (max 16), mutation 1 |
| `locks` | `Scope` and `Locks`: one keyed `RwLock` table for roots, ARR media, torrent hashes and action types, acquired in sorted order |
| `events` | `Hub` (broadcast, 2000-event ring, per-job `watch`), `JobEvent`, `JobProgress`, `ProgressHandle`, `redact` |
| `handler` | `JobHandler` trait (`scope`, `run`, `recover`), `JobCtx`, `JobError`, `JobSuccess`, `ScanSummary`, `RecoverySummary` |
| `auto` | `AutoControl` (persisted `auto_paused`), `ActionHistory`, `failed_sets`, `CycleEngine`, `run_cycle`, `assess` |
| `workflow` | `WorkflowHandler`: the production `JobHandler` and `CycleEngine` over `removarr-workflows` |

## Public contracts for `backend-api` (16)

### Wiring

```rust
let cfg = JobsConfig {                       // from Config
    schedule: ScheduleConfig { scan_interval_minutes, anchor_minute_of_day: schedule::parse_anchor(&c.scan.anchor_utc)?,
                               qbit_refresh_minutes },
    limits: LimitsConfig { content_read_concurrency, ffprobe_concurrency, http_concurrency_per_service },
    retry: RetryConfig::default(),
    auto_mode: c.mode.auto_mode,             // read ONCE here; there is no setter anywhere (T5-02)
};
let handler = Arc::new(WorkflowHandler::new(WorkflowDeps { observe, observe_cfg, planner, actions, mutators,
                                                            action_settings, verify, history }));
let rt = JobRuntime::new(cfg, Deps { store: Arc::new(repos.jobs.clone()), state: Arc::new(repos.state.clone()),
                                     handler, clock, ids, jitter: Arc::new(RandomJitter) });
let report: StartReport = rt.start().await?;   // restore, then spawn workers + scheduler
rt.shutdown(Duration::from_secs(45)).await;    // on SIGTERM, after axum's graceful stop
```

16 must fill (the runner fails closed when they are missing): `ActionSettings.capabilities` (including
`AutoRemoveEmptyTorrents`), `ActionSettings.auto_min_age_hours`, `ActionPorts.ignores`, `PlanSettings` (`implemented_tier =
actions::IMPLEMENTED_TIER`, `auto_min_age_hours`, capabilities), and `WorkflowDeps.history` (see "ActionHistory" below).

### Job commands (all methods of `JobRuntime`)

| Command | HTTP use | Behaviour |
|---|---|---|
| `submit(kind) -> Result<Enqueued{id, existing}, JobError>` | scan, refresh, evaluate, verify, execute | Persists a queued job. A request matching an active job (same dedupe key) returns that job with `existing = true`. `AutoCycle` and `Recover` are rejected (`Validation`) |
| `rescan()` | `POST /scan` | `submit(JobKind::Scan)` |
| `cancel(id) -> CancelResult` | cancel button | `CancelledQueued` (queued job ended at once), `Requested` (running job told to stop at its next safe checkpoint), `NotActive`, `NotFound` |
| `retry_recovery()` | `POST /recovery/retry` | Resets the recovery attempts and wakes the execute worker |
| `resume_auto() -> Result<bool>` | `POST /auto/resume` | The only way to clear `auto_paused`; persisted |
| `auto() -> Arc<AutoControl>` | dashboard | `status()` gives `{ enabled, paused: Option<String> }` |
| `status() -> RuntimeStatus` | dashboard poll | `{ jobs: Vec<JobProgress>, auto_enabled, auto_paused, recovery: RecoveryStatus, next_scan, next_refresh }` |
| `progress(id)`, `watch(id)` | job panel (`every 2s`) | `JobProgress` / `watch::Receiver<JobProgress>` |
| `subscribe()`, `events_since(seq)` | log tab, SSE or polling | `broadcast::Receiver<JobEvent>`; late subscribers replay `events_since(0)` first |
| `limits()`, `locks()` | wiring | shared `Limits` and `Locks` |

`status`, `progress`, `watch`, `subscribe` and `events_since` read memory only: no database call, no filesystem call (test
`status_progress_and_events_read_memory_only` asserts zero job-store calls). Do not add a store read to a status route.
For terminal outcomes after the in-memory window (the last 100 finished jobs), read `JobRepo::get` (SQLite, SSD).

### Event stream (`JobEvent`, serde JSON, no secrets)

`{ seq, at, job, event, ... }` with `event` one of: `queued {label, lane, existing}`, `started {label, lane, attempt}`,
`stage {stage}`, `progress {done, total, current}`, `warning {message}`, `log {level, message}`, `retrying {retry, delay_ms,
reason}`, `cancel_requested`, `finished {status, outcome}` (always the last event of a job), `action {action: ActionEvent}`
(runner events, tag `event` of the runner nests under `action`), `auto_paused {reason}`, `auto_resumed`, `recovery
{attempt, ok, plans, detail}`. Messages go through `redact` (URL-like tokens become `[url]`). Stage changes are the only
progress written to the database; counters, current item and warnings are memory only.

### Job states and restart

`Queued -> Running -> Succeeded | Failed | Cancelled | Interrupted` (domain `JobStatus`). At `start()`: `interrupt_running()`;
interrupted Observe jobs are re-queued **once** (the replacement id is stored under `requeued:<dedupe_key>` in `app_state`;
a job that is itself that replacement is not queued again); Execute jobs are never re-queued; an interrupted `AutoCycle` or a
leftover `auto_cycle_running` marker pauses automatic mode. Queued jobs from the previous run stay queued and are claimed in
order. A shutdown cancels jobs: they end `Interrupted` (user cancels end `Cancelled`).

### ActionHistory (T3-12 / T5-10 input, to wire in 16)

`ports` have no listing of past plan actions, so `auto::ActionHistory::executed_since(since)` carries it. Reference
implementation (tested): `StoreHistory` in `crates/jobs/tests/real_runner.rs`, over `PlanRepo::ids_with_status` for
`Executing, Executed, PartiallyExecuted, Failed, Interrupted, Invalidated`, `PlanStore::get` and `PlanRepo::actions`. It
filters on `plan.created_at` because no port exposes `updated_at`: a plan older than 7 days whose action failed recently is
missed (conservative window is created_at, see deviations).

## Behaviour by task item

1. **Scheduled scans, no aggressive polling.** `scan.interval_minutes` + `scan.anchor_utc` (0 = manual only), `qbit_refresh_minutes`
   (0 = off, API only). One `sleep` until the next due instant; idle workers wait on a `Notify`. A run missed while the process
   was down is not replayed. Tests: `the_scheduler_sleeps_until_due_and_never_polls`, `schedule::tests::*`.
2. **Persistent queue.** Every job is a `jobs` row; manual rescans, executions and the automatic cycle go through `JobStore`.
3. **Restart recovery.** Explicit states above. Tests in `restart.rs` (MemStore and a real SQLite file across a simulated crash).
4. **Concurrency limits.** One worker per lane; per job kind permits (`Scan`, `Api(service)`, `Probe`, `Compare`, `Mutation`) from
   `Limits`; `RuntimeStatus` shows jobs. Tests: `each_lane_runs_one_job_at_a_time_...`, `limits_are_clamped_and_enforced`.
5. **Serialization.** `Scope::for_plan`: exclusive on `plan.lock_roots`, every ARR media key of `plan.arr_snapshots`, every
   affected torrent hash and every action type; a simulation takes the roots shared. Scan, evaluate and verify take all roots
   shared. Keys are acquired sorted (no deadlock). Tests: `locks_limits.rs`, `an_execution_waits_for_a_scan_that_shares_its_root_...`.
6. **Cancellation.** Token per job (child of the shutdown token) bridged to the runner's `CancelFlag`. `HTTP`-only jobs
   (`RefreshService`, `EvaluateMedia`) are dropped at once; every other job, mutating ones included, is never dropped: it stops
   at `JobCtx::checkpoint()` or, for `ExecutePlan`, inside the runner, which stops issuing mutations and compensates (stage
   `batch_interrupted`). Test `cancelling_during_a_mutation_never_drops_the_step`.
7. **Progress, stage, warnings, logs, terminal outcomes.** See the event stream.
8. **Backoff with jitter, bounded.** Observe jobs failing with `Unavailable`: 3 retries at 1 s, 2 s, 4 s plus or minus 20 percent.
   Execute-lane jobs are never retried (REJ-21). Recovery: every 5 minutes, 12 attempts, then `NeedsManual`. Tests in `backoff.rs`.
9. **One importer per media.** The media key lock plus the single execute lane; the runner's `NoArrImport` revalidation is the
   external-importer guard (unchanged, 13).

## Automatic mode (tier 5)

* `auto_mode` is a constructor argument of `JobRuntime` and `AutoControl::enabled()` is read-only (T5-02). `auto_paused` is the
  `app_state` key `auto_paused` (empty = not paused), loaded at `start()`; the first reason is kept; only `resume_auto()` clears it.
* The runtime queues `AutoCycle { session }` only after a **scheduled** `Scan` that succeeded, is `Complete` and has no
  whole-root perimeter blocker (`Classified.perimeter.auto_blockers(all roots)`: scan, mappings, permissions, APIs). Otherwise it
  pauses with the first uncertainty. Manual scans never start a cycle; a scheduled tick that lands on an active manual scan
  adopts it. Tests: `auto_flow.rs`.
* `run_cycle` (architecture 7.6): A1 check, crash marker (`auto_cycle_running`), phase 1 (`create_auto(Consolidations)` with
  the journal-derived `AutoContext`), per plan: ignore re-check, `recover`, exclusive locks, `execute`, `assess`; rescan of the
  roots touched by a live `Done` step (none in a dry-run); abort on a rescan that is not `Complete`/clean; phase 2 from the
  new session with failed paths from phase 1 added; stop and pause at the first uncertainty; unused approved plans are
  discarded so they cannot run later.
* `WorkflowHandler` calls the production `actions::execute` (gate `IMPLEMENTED_TIER = T4`): until the final CP2-tier5 review
  every automatic plan is refused by the runner (`NotExecuted`, plan `Invalidated`), the cycle records it as a failed
  consolidation path and continues without pausing. Nothing automatic executes today, which is the intended state.

### T5 requirements owned by 14

| Req. | Status | Tests |
|---|---|---|
| T5-02 | done: `auto_mode` read once, no toggle; `auto_paused` persisted, cleared only by the user; A1 before every phase (`run_cycle`, planner `check_enabled`) | `auto_cycle.rs::off_or_paused_does_nothing_at_all`, `auto_flow.rs::manual_scans_off_and_paused_never_start_a_cycle`, `restart.rs::an_unfinished_cycle_marker_pauses_auto_mode_and_the_pause_survives_restarts` |
| T5-08 (cycle level) | done: any outcome other than `Verified`/`Simulated`/`NotExecuted`, an interruption, a left-stopped torrent, a recovery need or error, an execute error (recover, then stop), a non-benign refusal, an unreadable ignore list or journal, a dirty rescan or non-`Ok` service stops the cycle and pauses | `auto_cycle.rs::every_uncertainty_stops_the_cycle_and_pauses_with_a_reason`, `::an_execute_error_triggers_recovery_before_the_pause`, `::planning_failures_and_policy_errors_...`, `::assess_applies_the_t5_08_rule_per_outcome` |
| T5-09 (14 part) | done: ignore list re-read just before each plan; an ignored subject discards the plan; unreadable list fails closed | `auto_cycle.rs::an_item_ignored_after_planning_is_never_executed` |
| T5-10 / T3-12 | done: `failed_sets` from the journal (7-day window, `Simulated` excluded, missing outcome counts as failed), phase-1 failures added in memory, both handed to `AutoContext` | `auto_cycle.rs::failed_sets_are_rebuilt_from_the_journal_within_seven_days`, `::a_failed_consolidation_is_never_followed_by_a_deletion_...`, `real_runner.rs::t5_with_the_production_gate_nothing_runs_and_the_refusal_feeds_the_failed_sets` (real SQLite tables) |
| T5-11 | done: consolidations, rescan of mutated roots only when a live `Done` step exists, abort on a dirty rescan, deletions from the new session; dry-run simulated with no rescan | `auto_cycle.rs::two_phases_with_a_rescan_...`, `::no_live_mutation_means_no_rescan_...`, `::a_dry_run_cycle_simulates_every_plan_and_never_rescans`, `real_runner.rs::t5_the_cycle_runs_an_automatic_consolidation_end_to_end_once_the_gate_is_raised` (real runner through `tier5-test-gate`) |
| T5-12 / T3-19 | done: cycle only after a scheduled complete scan, execute lane, one plan at a time, exclusive locks held only while a plan executes, `recover` before every execute-lane job and every plan and after any error, nothing executes while recovery is unresolved, cancellation bridge | `auto_flow.rs`, `backoff.rs::nothing_executes_while_recovery_is_unresolved_...`, `::recover_runs_before_every_execute_job_and_after_an_execute_error`, `::twelve_failed_recoveries_...`, `runtime.rs::cancelling_during_a_mutation_never_drops_the_step` |
| T5-15 (cycle level) | the cycle holds no mutator; dry-run cycle executes with `simulate = true` on the recording set (guards themselves are 13's) | `auto_cycle.rs::a_dry_run_cycle_...` |
| T5-16 | done: a crash inside a phase leaves `auto_cycle_running`, the next start pauses, `recover` resolves the plan first and stops the next cycle; nothing restarts by itself | `real_runner.rs::t5_16_a_crash_inside_the_cycle_...`, `auto_cycle.rs::a_crash_inside_the_cycle_leaves_the_marker_and_the_next_start_pauses`, `restart.rs` |
| T5-17 (cycle parts) | stop at first failure, fail closed on incomplete scan/API/mapping/permission (pause), ignored never executed, failed path never deleted | as above |
| T5-19 | event side done: `recovery`, `auto_paused` and `action` events; `Approver::AutoPolicy` is asserted in `real_runner.rs`; history and dashboard display belong to 15 and 16 (`RuntimeStatus.auto_paused` carries the reason) | `real_runner.rs` (first test) |

Not 14's: T5-01, T5-03..T5-07, T5-13, T5-14 (12/13, done), T5-18, T5-20..T5-22 (13/12/22).

## Deviations from the architecture

1. **Rescan observes every root.** `observe::run_scan` has no root restriction (architecture 7.6 names `observe::run(roots = Some(R))`).
   The cycle computes R and rescans only when R is not empty, but `WorkflowHandler::rescan` runs a full scan. A restricted scan
   in 11 must reuse stored inventories of the other roots (`ScanStore::load_inventory`) for classification; then only
   `WorkflowHandler::rescan` changes.
2. **Recovery is a gate, not a queued job.** The execute worker runs `recover` before claiming; `JobKind::Recover` jobs are
   not user-requestable (`retry_recovery()` replaces it). A stored `Recover` row (not produced by this crate) is run as a recovery pass.
3. **Re-queue once** uses an `app_state` key per dedupe key. A shutdown-interrupted job that is itself a replacement is not
   queued a third time (intentional).
4. **`ActionHistory` is a trait** (no port lists past actions); `at` is the plan's `created_at`.
5. **Benign refusal detection** is a string suffix check (`"item(s) excluded"`, set by `Planner::create_auto`); any other
   refusal text pauses. A typed refusal reason in 12 would remove the string match.
6. **One Observe worker**, as ADR-0007 says: a qBittorrent refresh waits behind a running scan.
7. **No catch-up** of a scan missed while the process was down.
8. **ServiceCache is in memory** (11 gap): after a restart the automatic cycle needs the next scheduled scan (`AutoCycle` is not
   re-queued and `plan_phase` refuses a session it has no in-memory result for).
9. **`EvaluateMedia` results are not retained**: the job reports counts only. Media detail pages should call
   `evaluate::evaluate_media` directly (read-only GET) rather than wait for a job.
10. Dev-dependencies added to jobs: `removarr-storage`, `removarr-fs`, `removarr-config` (`test-support`), `removarr-matching`,
    `removarr-workflows` with `tier5-test-gate`, `tempfile`. `cargo tree -p removarr -e normal,features | grep tier5` prints nothing.

## Open TODOs

* 16: wiring above, `ActionHistory` implementation, map `JobError` to HTTP, SSE or polling over the event stream.
* 11: root-restricted `run_scan` (deviation 1).
* 12: a typed refusal reason in `AutoOutcome.refused` (deviation 5).
* 22: end-to-end tests of `WorkflowHandler` scan, refresh, evaluate and verify (they compile and delegate to tested workflow
  functions, but jobs has no fixture for them); HTTP-level simulation guards with the real `JobRuntime`.
* Raising `IMPLEMENTED_TIER` to T5 is not done here; after that review, `WorkflowHandler` needs no change.

## Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 789 passed, 0 failed (jobs: 6 unit + `runtime` 13, `restart` 4, `backoff` 10, `locks_limits` 6,
  `auto_cycle` 13, `auto_flow` 3, `real_runner` 3 = 58).
* `cargo tree -p removarr-jobs -e normal --prefix none | grep '^removarr-'`: domain, matching, workflows only.
* Tests use fake and paused clocks (`tokio` `start_paused`) except `real_runner.rs` and the SQLite restart test (real time, temp
  dirs, loopback-free). No network beyond crates.io; nothing committed.
