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
