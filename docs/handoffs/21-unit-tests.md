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
