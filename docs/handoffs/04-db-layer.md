# Handoff — 04-db-layer (wave 4)

## Delivered scope

`crates/storage/**` only (no edit to domain, root `Cargo.toml`, other crates). Dependencies unchanged;
no missing dependency. Added `crates/storage/build.rs` (re-embeds migrations when they change).

```
crates/storage/
  build.rs
  migrations/0001_observations.sql        meta, app_state, sessions, fs_entries, torrents, matches, arr, issues, ignores
  migrations/0002_plans_journal_jobs.sql  plans, plan_actions, approvals, simulation_runs, journal, gains, deltas, jobs, audit triggers
  src/lib.rs        Repos bundle + re-exports
  src/db.rs         Db (writer + reader pools, pragmas, MIGRATOR, close/optimize/pragma)
  src/error.rs      sqlx -> PortError mapping (no sqlx type is public)
  src/codec.rs      canonical JSON columns, enum text, u64<->i64, scrub() for URLs/credentials
  src/repos/*.rs    scan, snapshot, matches, issues, ignores, plans, journal, gains, jobs, state
  src/retention.rs  preview / prune / audit_counts
  src/recovery.rs   RecoverySnapshot
  tests/*.rs        migrations, scan, plans, journal, gains_jobs, observations, retention, concurrency, secrets
```

## Public contracts

```rust
let db = removarr_storage::Db::open(&data_dir.join("removarr.db")).await?;   // creates + migrates
let repos = removarr_storage::Repos::new(&db, clock /* Arc<dyn Clock> */);
// repos.scans: ScanStore   repos.snapshots: SnapshotStore   repos.matches: MatchStore
// repos.issues: IssueStore repos.ignores: IgnoreStore       repos.plans: PlanStore
// repos.journal: JournalStore repos.gains: GainStore         repos.jobs: JobStore
// repos.state: StateStore  repos.retention  repos.recovery
db.optimize().await?;   // end of each Scan and at shutdown
db.close().await;       // PRAGMA optimize + one PASSIVE checkpoint, then close
```

One repo type per port (several ports share method names such as `get`). Each is `Clone` and can be wrapped
in `Arc<dyn Port>`. All errors are `PortError`.

Error mapping: UNIQUE -> `Conflict`; FK / NOT NULL / CHECK / trigger abort -> `Integrity`; BUSY/LOCKED/pool
timeout -> `Unavailable`; missing row -> `NotFound`; domain validation -> `Validation` (or the
`DomainError::class()`); other -> `Storage`. Details contain constraint names only, never bound values.

Inherent (non-port) methods, for callers that need more than the port:
- `JobRepo::enqueue_outcome -> Enqueued::{New(id), Existing(id)}`, `JobRepo::get`, `JobRepo::running`.
- `PlanRepo::ids_with_status`, `PlanRepo::actions(plan) -> Vec<ActionRow>`, `PlanRepo::record_simulation_run`.
- `ScanRepo::session(id)`, `SnapshotRepo::latest_arr(media_key)`, `arr_media_key(&ArrQualitySnapshot)`.
- `GainRepo::totals()` (SQL sums; every `gain_id` exists once, so `duplicates` is 0).
- `Retention::{preview, prune, audit_counts}`, `Recovery::snapshot -> RecoverySnapshot`.

## Schema documentation

Conventions: instants = UTC unix ms; u64 stored as the same 64 bits in INTEGER (`from_ne_bytes`); JSON columns =
`canonical_json_string`; paths are `(root, rel)`, never absolute; all tables `STRICT`; FKs on.

| Table | Key / constraints | Notes |
|---|---|---|
| `meta` | `key` PK | `match_session`, `issues_session` = session that produced the current set |
| `app_state` | `key` PK | `StateStore`; unchanged value = no write; values are scrubbed |
| `scan_sessions` | `id` PK, CHECK status, CHECK `(status='running') = (finished_at IS NULL)` | metrics JSON |
| `session_roots` | `(session_id, root)` | scanned/error_count/protected_count; `root_sig`, `inventory_saved`, `inventory_finished` |
| `session_services` | `(session_id, service)` | state JSON (`ServiceState`, reason scrubbed) |
| `fs_entries` | `(root, rel)` PK, idx `(dev, ino)` | signature columns, `allocated`, `first_seen_at`, `changed_session` FK, `gone_session`/`gone_at` |
| `scan_errors`, `protected_zones` | idx `(session_id, root)` | per session; message scrubbed |
| `torrents` | `hash` PK | `info` and `files` canonical JSON (deviation: no `torrent_files` table); `torrent_snapshot` single row (id=1) |
| `match_candidates` | `(a_root,a_rel,b_root,b_rel)` PK | `sig_a/sig_b/identity/content/reasons` JSON; no session column (see below) |
| `arr_snapshots` | `UNIQUE(media_key, fingerprint)` | JSON body; repeat only advances `captured_at` |
| `issues` | `id` PK (32 hex) | typed columns + JSON; `first_seen_at` kept; `resolved_at`; `body_hash` for skip-unchanged |
| `ignores` | `subject_key` PK | `restored_at` keeps history |
| `plans` | `id` PK, `hash` UNIQUE (64), CHECK status/mode/origin, `session_id` FK | `body` = canonical `ActionPlan`; `updated_at` |
| `plan_actions` | `(plan_id, idx)` PK | `kind`, `state` (CHECK, snake_case `ActionState`), `outcome` JSON (`ActionOutcome`) |
| `approvals` | `plan_id` PK FK, `plan_hash` FK -> `plans(hash)` | one per plan; ack lists JSON |
| `simulation_runs` | autoinc PK, `plan_id` FK | prunable |
| `journal_intents` | `seq` autoinc, `id` UNIQUE, FK `(plan_id, action_idx)` -> `plan_actions` | `mode` live\|simulated; target/observed/expected_gain/hashes JSON |
| `journal_results` | `intent_id` PK FK | one result per intent |
| `gains` | `gain_id` PK (UNIQUE, 32 hex), FK `(plan_id, action_idx)` | `dev/ino/bytes/kind`; the double-count guard |
| `observed_deltas` | autoinc | `plan_id` nullable FK |
| `jobs` | `id` PK; partial UNIQUE index `dedupe_key WHERE status IN ('queued','running')` | kind/outcome JSON; ordered by rowid within `enqueued_at` |

Audit triggers (migration 0002): `plans`, `plan_actions`, `approvals`, live `journal_*`, `gains`,
`observed_deltas` can never be deleted; their identifying columns (plan body/hash/…, journal rows, approvals,
gains) cannot be updated. A plan status of approved/executing/executed/partially_executed requires a matching
`approvals` row. A **live** intent requires its plan to be `executing`. Only simulated journal rows may be deleted
(retention).

Migrations: forward-only, `CREATE ... IF NOT EXISTS`, embedded through `sqlx::migrate!`. sqlx verifies checksums
(a modified released migration is refused) and refuses a database that has migrations this binary does not know
(newer database). Never edit a released migration; add `0003_*.sql`.

## Repository transaction boundaries

Every port method is exactly one `BEGIN IMMEDIATE` transaction on the single writer connection (reads use one
deferred read transaction on the reader pool for a consistent snapshot). No port exposes a transaction handle.

| Method | Transaction content |
|---|---|
| `ScanStore::begin_session` | session + roots + services |
| `save_inventory` | whole root diff in ONE transaction (see deviations): load current rows, write added/changed/revived only, mark gone (finished scan, not under an error/protected path), replace that root's errors/protected zones, upsert `session_roots` |
| `finish_session` | CAS `running -> terminal` + roots + services |
| `SnapshotStore::save_torrents` | upsert-if-changed, delete missing, snapshot meta row |
| `save_arr` | verify `fingerprint_is_valid`, upsert by `(media_key, fingerprint)` |
| `MatchStore::replace_for_session` | diff vs current (write changed, delete stale), session marker |
| `IssueStore::replace_current` | diff by `body_hash`, keep `first_seen_at`, set `resolved_at` for absent issues, session marker |
| `PlanStore::insert` | `HashedPlan::from_parts` check, plan row + all action rows |
| `PlanStore::approve` | read plan; require Draft, hash equal, `now < expires_at`; insert approval; `UPDATE ... WHERE status='draft'` |
| `PlanStore::transition` | domain `can_transition_to`, then CAS `UPDATE ... WHERE status = from`; `-> Approved` refused (use `approve`) |
| `JournalStore::record_intent` | one insert; commit = WAL fsync (`synchronous=FULL`) before return |
| `record_result` | intent must exist (`NotFound`); unique per intent (`Conflict`) |
| `record_outcome` | state machine check; idempotent for an identical outcome |
| `GainStore::record` | all gains + optional delta, or nothing; any duplicate `gain_id` rolls everything back (`Conflict`) |
| `JobStore::enqueue / claim_next / set_stage / finish / interrupt_running` | one transaction each; `finish` validates `JobStatus::can_transition_to` |

Durability ordering (INV-08): the intent insert is committed and fsynced before `record_intent` returns, so the
caller may then mutate. Test `intent_is_durable_in_the_wal_before_record_intent_returns` copies the db + WAL files
(no checkpoint, no clean shutdown) and finds the intent. A result can never precede its intent (FK + `NotFound`).

Recovery queries: `JournalStore::unfinished()` (live intents with no result, oldest first),
`Recovery::snapshot()` = `{ unfinished, executing_plans, running_jobs }`, `needs_recovery()`,
`unfinished_by_plan()`. Read-only; resolving (results, `Executing -> Interrupted`, `interrupt_running`) is 13/14's job.

Pragmas: writer `max_connections=1`, WAL, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=5000`,
`temp_store=MEMORY`, `journal_size_limit=64MiB` (bounds the WAL file; not a checkpoint). Reader: 4 connections,
`read_only`, FK on, busy timeout 5000. `wal_autocheckpoint` stays 1000 (verified by test). No checkpoint loops:
one PASSIVE checkpoint + `optimize` in `Db::close()`, `Db::optimize()` for end-of-scan.

Low I/O: `save_inventory`/`save_torrents`/`replace_for_session`/`replace_current`/`StateStore::set`/stage
updates skip unchanged rows (tests assert unchanged `changed_session`, one commit per call via `PRAGMA data_version`).

Retention: `Retention::preview(cutoff)` counts, `prune(cutoff)` deletes in one transaction: old per-session
`scan_errors`/`protected_zones` (never those of the current inventory of a root), `gone` `fs_entries`, **simulated**
journal results/intents, `simulation_runs`, resolved `issues`. There is no API to delete plans, actions, approvals,
live journal rows, gains or deltas, and the triggers refuse raw SQL deletes. Sessions themselves are never pruned.

Secrets: `scrub()` removes URLs (`://`, `magnet:`), `Bearer`/`Basic` tokens and `key=value` credentials from every
free-text field that is persisted (service state reason, scan error message, ignore reason, app_state value, job
stage and failure message, journal target/observed/final_state strings, step failure/skip reasons, action
unverified reason). Domain types carry no tracker URL field. `sqlx` statement logging is disabled and errors never
include bound values. Test `no_url_or_secret_reaches_any_table_or_the_wal_file` greps every table and the db/WAL bytes.

## Deviations from the architecture

1. `save_inventory` is one transaction per root call, not 1000-row transactions: atomic, and one fsync per root.
   Revisit only if a root exceeds memory budget (existing rows of the root are loaded into a hash map).
2. `torrent_files` table folded into `torrents.files` (canonical JSON per torrent). `action_outcomes` folded into
   `plan_actions.state/outcome` (as the 03 handoff described). `arr_queue_items` not created (no port reads or
   writes it); add in 0003 when 09/15 need it.
3. `app_state` serves as the settings store (task wording "settings").
4. `match_candidates` and `issues` have no per-row session column. `MatchCandidate.session` / `Issue.session` returned
   on read are the session of the last `replace_*` (`meta`), so unchanged rows need no write.
5. `JobStore::enqueue` on a duplicate active `dedupe_key` returns the existing job id (idempotent), not an error;
   `enqueue_outcome` distinguishes. `get_many` returns only unresolved issues.
6. Added guards beyond the ports: live intent requires an `executing` plan; plan statuses beyond draft require an
   approval row; `transition(.., Approved)` is refused (approval is the only way); `record_outcome` obeys
   `ActionState` (open states take any final outcome, `ExecutedUnverified` only `Verified`/`FailedNeedsReview`).
   Action-runner (13) must therefore move the plan to `Executing` before its first live intent.
7. `PlanStore::approve` checks expiry with the repository clock, not `Approval.approved_at`.

## Open TODOs / notes for later waves

- Reporting (15) reads SQL directly: columns above are the contract; JSON columns are canonical JSON of the domain types.
- The jobs crate (14) owns lane concurrency; `claim_next(Execute)` does not refuse while another execute job runs.
- `Db::open` takes a file path (no `:memory:` support); the data dir must exist.
- `ArrQualitySnapshot` rows are never pruned (small; audit references them by fingerprint).
- Retention of `sessions`, `torrents`, `arr_snapshots` is intentionally not provided.

## Commands run and results

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test --workspace`: pass. Storage: 6 unit tests (scrub, casts, upgrade migration 1 -> 2 with data, tampered
  migration refused) + 54 integration tests (60 total) (migrations fresh/reopen/newer-db, scan, plans, journal incl. durability and
  restart recovery, gains/jobs, observations, retention, concurrent readers/writers, secrets).
- Dependency gates (domain has no tokio/sqlx/reqwest/axum; workflows has no direct storage dep): print nothing.
