# Handoff — 21-unit-tests (append-only log)

## Pass 1 (after wave 3, domain only)

Delivered:
- `docs/parity-matrix.md`: T-01..T-32, one row each. Covered: T-03 only. 31 pending with owners (T-19 superseded, T-24 obsolete, replacements named).
- `crates/domain/tests/parity_props.rs`: 11 proptest properties on public API: `RelPath` (clean paths round-trip, `..`/`.`/empty/leading-trailing slash/NUL/serde rejection, join/parent identity, component-wise `starts_with`), `RootId::media`, `is_sample` case-insensitivity, canonical JSON order independence, `estimate_reclaimable` (inode counted once, only when fully selected, never when held elsewhere), `aggregate_unique` idempotent under duplication, `gain_id` purity, `Timestamp::age_days` monotonic. No Cargo.toml or src edits (proptest was already a dev-dependency).

Review of the 80 existing domain unit tests: deterministic (no clock, no I/O, no randomness), exercise public behavior of state-machine tables (exhaustive pairs), plan hash/approval, recheck evaluation, gain accounting. No weakened assertions found.

Gaps left (not coverable without src or manifest changes or out of scope): `testutil` builders are `cfg(test)` crate-private, so integration-style tests in `tests/` cannot build full `ActionPlan`s; plan approval/hash behavior stays covered by in-crate tests only. If later passes need plan fixtures from `tests/`, 03 or 22 must expose them (testkit).

Domain bugs found: none.

Commands: `cargo test -p removarr-domain` pass (80 unit + 11 props, 0 ignored, about 0.03 s test time); `cargo clippy -p removarr-domain --all-targets -- -D warnings` pass; rustfmt applied to `parity_props.rs`.

Uncovered integration boundaries for `integration-tests`: none from domain (pure crate).

## Pass 2 (after wave 4: config and storage)

Review: the 3 config and 9 storage test files are deterministic (tempdirs, FakeClock, per-test databases, dummy secrets), assert public behavior and use no weakened assertions. Strong existing coverage: mapping confinement with 3 proptests, root validation fail-closed cases, intent WAL durability by file copy, gain double-count, plan hash tamper detection, approval CAS races.

Added (new files only, no Cargo.toml/src edits):
- `crates/config/tests/parity_redaction.rs` (9 tests): `Secret` Debug/Display/`ct_eq` semantics; config errors never echo sentinel secrets (URL userinfo, short key, bad hash); redaction of URL/header/text forms; proptests (sensitive values never survive, `redact_text` idempotent); mapping edge cases (trailing/double slash, case-sensitive component-bounded prefixes, `..` refused even when landing in another root, no identity mapping for services without rules).
- `crates/storage/tests/parity_recovery.rs` (5 tests): unfinished intents across two interleaved plans keep journal order after restart and exclude simulated; executing plan with no open intent still needs recovery; fresh DB needs none; gain cannot be recounted after restart; approved plan hash and approval re-verified after reopen, re-approval and gate-skipping transitions refused.

Matrix: T-26 covered (config side), T-18 partial (data_dir confinement; report directory pending in reporting). Counts: 2 covered, 1 partial, 29 pending.

Bugs found: none. Observation: `redact_text` over-redacts by design (any key containing "key", "sig", "u"); acceptable per its doc.

Commands: `cargo test -p removarr-config -p removarr-storage` all pass, 0 ignored, each binary under 0.5 s; `cargo clippy -p removarr-config -p removarr-storage --all-targets -- -D warnings` clean; rustfmt applied to both new files.

Uncovered boundaries for `integration-tests`: real filesystem symlink races under a root (fs-engine), a real power-cut/fsync behavior (only WAL file-copy simulated), config loaded from the shipped Docker env.

## Pass 3 (after wave 5: fs, qbit, jellyfin)

Review: the existing 33 fs, 37 qbit and 22 jellyfin (+ new below) integration tests are deterministic (own tempdirs or the loopback fakes on `127.0.0.1:0`, dummy secrets, short timeouts, no wall-clock assertions beyond bounds), assert public behavior and contain no weakened assertions. fs has no unit tests by design (everything is under `tests/`).

Added (new files only, no `src/**` or Cargo.toml edits, only already-declared dependencies):
- `crates/fs/tests/parity_confinement.rs` (6): symlink loops (`.`, `..`, to sibling, to another root, dangling) recorded and never entered; symlinks add no blocks, no nlink and no identity to their target; hardlinks to files outside every root (nlink 2 and 3 observed) leave unique-block accounting and `shared_between` correct against an independent `st_blocks` sum; a sibling whose name shares a prefix with a root (mode 000) is never opened and is not a configured root; `stat`/`snapshot_dir` refuse symlinked components in the media root and never open a mode-000 target; a FIFO is recorded as `Other` without being opened.
- `crates/qbit/tests/parity_safety.rs` (9): allowlist on live listings for 4.x and 5.x against an independent state table (pausedUP only 4.x, stoppedUP only 5.x, checking/missing/unmapped/incomplete protected); 5.2.0 with 204 login end to end (T-28); no request of any kind carries `deleteFiles=true`, `deleteFiles` only on delete and equal to false (4.x and 5.x); empty set sends nothing, remove touches only listed hashes; stop/start/recheck/remove/recheck_and_wait all refused for an unmapped torrent (and a mixed set is refused whole); a `files()` query alone marks mapping uncertain; recheck with zero observed checking polls is never `Verified`; password never in Debug or errors over 9 HTTP statuses.
- `crates/jellyfin/tests/parity_incomplete.rs` (3): every read endpoint (items, libraries, info, tasks, health) x 10 error statuses gives `Err`, `completeness_of` incomplete, never `Ok(None)`/idle/Completed, with no token, host or upstream body in the error; hostile 200 bodies rejected and not echoed; refresh commands fail for 301/302/4xx/5xx and are sent exactly once even with read retries configured.

Matrix: now 8 covered (T-01, T-02, T-03, T-25, T-26, T-27, T-28, T-29), 4 partial (T-17, T-18, T-30, T-32), 20 pending. Rows for media-match, arr and the domain patch are left for those agents' passes.

Findings (source not touched):
1. Design gap, not a failing test: `QbitController::mutate` only consults `mapping_uncertain`, which is populated by a previous listing or `files()` call on the same reader. A controller on a fresh reader that never listed will `stop`/`remove` an unmapped hash. Callers (13) must run `snapshot()` or `torrents()` first; consider making `mutate` list once when no listing has been seen, or documenting it as a precondition for 13. Tests assert the documented precondition (mutation refused after listing).
2. qbit errors echo the upstream response body (truncated, password redacted). A body containing the SID value or the server host is not redacted (`HTTP 400: [REDACTED] <sid> <host>`). Jellyfin errors carry no upstream body at all, which is safer. Low severity (qBittorrent does not echo SIDs); consider dropping or length-limiting the body in qbit `detail` or also redacting the known SID.
3. `fs` carries unused declared dependencies (`serde`, `tokio`, `tracing`, `thiserror`, `async-trait`); noted by 06 already, 13 to use or remove.

Not covered here, for `integration-tests`: real mount boundary (needs a second filesystem), Linux `fadvise`/`fstatvfs` behavior, real qBittorrent 5.x/4.x servers and Jellyfin servers (fakes only), dry-run end to end (T-30), FsMutator confinement (13 not delivered), non-UTF-8 names (APFS refuses them; run on ext4/btrfs). The 20k-file large_tree test takes about 3 s in debug.

Commands: `cargo test -p removarr-fs -p removarr-qbit -p removarr-jellyfin` all pass, 0 ignored (fs 39 tests, qbit 46, jellyfin 25), slowest binary `large_tree` 3.2 s, others under 0.7 s; `cargo clippy -p removarr-fs -p removarr-qbit -p removarr-jellyfin --all-targets -- -D warnings` clean; rustfmt applied to the three new files.
