# Handoff 27: release gate (wave 20, CP3)

## Delivered scope

- `docs/release-readiness.md`: verdict **READY WITH CONDITIONS** (MASTER `PASS` with conditions C1-C7). It contains:
  - gate results with numbers;
  - the invariant checklist;
  - the T5 gate-raise check;
  - parity and acceptance mapping;
  - migrations, container and configuration review;
  - 15 deviation decisions;
  - the budget waiver decision;
  - security status;
  - pre-tag conditions, follow-ups and residual assumptions.
- `docs/reviews/cp3.md`: CP3 verdict PASS, iteration 1/3, with the list of deviations.
- No source, test, config, workflow or `docs/PROGRESS.md` change. Nothing committed, pushed, tagged or published. No
  network, Docker or live service was used.

Revision: `38a2294518d5119237a9e3e1831be9038d491501`, branch `autonomous-build`. The worktree was clean before this
gate. Afterwards, only the three files above are untracked or modified.

## Public contracts

None added. The pre-tag condition IDs C1-C7, the follow-up IDs R1-R10 and the deviation IDs D1-D15 are defined in
`docs/release-readiness.md`. The orchestrator should copy the verdict and conditions into `docs/PROGRESS.md`.

## Deviations from the architecture

- The verdict wording follows the orchestrator brief (`READY WITH CONDITIONS`) rather than the bare `PASS`/`BLOCKED`
  of MASTER and the agent definition. The mapping is stated in the verdict line: PASS with conditions, which becomes
  BLOCKED if C1-C4(a) fails.
- B1 and the B2 rescan miss on the macOS host. Per architecture §14, the gate records an explicit conditional waiver,
  not a pass. The waiver depends on a Linux measurement (C2), with a 210 MiB ceiling for the B2 rescan.
- Docker, benchmarks and `cargo deny` were not run, as the brief instructed. Their numbers come from handoffs 11, 23 and
  26. The gaps are carried as conditions C1-C3.

## Open TODOs

- User, before tagging: C1-C7 in `docs/release-readiness.md` section 9.
- Owners, not gating: R1-R10, section 10. The most useful are:
  - R1: recovery returns `Unobservable` for a sanitized `rel` (handoff 04 currently says it already does, which is
    inaccurate);
  - R2: add the `cargo tree | grep tier5` check to CI;
  - R3: refresh the parity matrix;
  - R4: add a `deny.toml` license policy.

## Commands run and results (macOS 26.5.1 arm64, rustc 1.98.0)

| Command | Result |
|---|---|
| `git rev-parse HEAD`, `git status --porcelain` | 38a2294, clean |
| `cargo fmt --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass, 0 warnings (cached diagnostics replayed) |
| `cargo test --workspace` | 1055 passed, 0 failed, 6 ignored; 132 result lines; 2 min 54 s |
| `cargo build --release -p removarr` | pass, 46.6 s, 14,964,432-byte binary |
| `cargo tree -p removarr -e normal,features --offline` piped to `grep -c tier5` / `grep -ci test-support` | 0 / 0 |
| `cargo tree -p removarr -e normal --offline` (looking for testkit, e2e or integration crates) | none |
| `cargo metadata --format-version 1 --offline` plus a Python license tally of the production closure | 241 third-party crates, all permissive, none flagged |
| Greps: `deleteFiles`, `importMode`, `removeFromClient`, `live_mode()`, `IMPLEMENTED_TIER`, allowlist, `is_auto_eligible`, media-root refusal, `BT_backup`, `st_nlink`, `Command::new`, `synchronous` | as listed in release-readiness section 2 |
| Script: every `::test_name` in `docs/parity-matrix.md` (104) checked with `grep -rqw "fn <name>" crates` | 102 found; 2 renamed by the T5 raise, replacements found |
| `git show 817eedc`, read of `recovery.rs::observe_fs_intent` and `domain/src/redact.rs` | fix verified; residual D12 found |
| `git ls-files` and `git grep` for secrets, IP addresses, `/Users/`, `$argon2`, `volume3` | no secret; NAS paths only in `old/` |
| Read: CI and release workflows, Dockerfile, compose, `.gitignore`, `.dockerignore`, `deny.toml`, `.env.example`, README | as in sections 5 and 8 |
| Docker, benchmarks, `cargo deny`, network | not run, as instructed |
