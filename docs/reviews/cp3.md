# CP3 review: final release gate

Reviewer: `27-release-gate`, an independent agent that wrote none of the reviewed deliverables. It ran at
`38a2294518d5119237a9e3e1831be9038d491501` (branch `autonomous-build`) with a clean worktree. Full evidence is in
`docs/release-readiness.md`.

Verdict: PASS (release verdict **READY WITH CONDITIONS**; becomes BLOCKED if C1, C2, C3 or C4(a) fails)

Iteration: 1/3.

## Gate results

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass, 0 warnings |
| `cargo test --workspace` | 1055 passed, 0 failed, 6 ignored (all opt-in benchmark, heavy or real-binary tests) |
| `cargo build --release -p removarr` | pass, 14.3 MiB host binary |
| `cargo tree -p removarr -e normal,features` | 0 `tier5` lines, 0 `test-support` lines |
| Safety invariants in source | all held (release-readiness section 2) |
| T5 gate raise (CP2T5F-F01 and the gate-raise steps) | done (release-readiness section 3) |
| Legacy parity T-01..T-32 | all 32 rows backed by existing tests. 2 stale names in the matrix were replaced by the T5 raise. |
| Security | 0 confirmed open. The re-audit LOW is fixed in 817eedc. The limiter lead is mitigated but depends on the deployment. B3b is open (LOW, accepted). |
| Budgets | B1 and B2-rescan miss on the macOS host, under a conditional waiver that depends on a Linux measurement (C2). All other budgets pass, including B3 and B11, which cannot be waived. |

## Deviations

| ID | Severity | Decision |
|---|---|---|
| D1 `tier5-test-gate` kept for workflows' own tests | minor | accept |
| D2 F08 full-root rescan in the automatic cycle | minor | accept |
| D3 `create_auto` refusal by text match | minor | accept (fails closed) |
| D4 RPT-05/08/09 not produced | minor | accept, release notes |
| D5 settings/explorer pages missing; media page unlinked | minor | accept, release notes |
| D6 numbers and dates not localized | minor | accept |
| D7 testkit `parity_preset` mismatch | minor | accept |
| D8 requeued job id not linked | minor | accept |
| D9 320 px layout not re-checked in a real browser | minor | accept with C6 |
| D10 CONTEXTE dashboard extras missing; left-stopped counter hard-wired to 0 | minor | accept, release notes, R7 |
| D11 parity matrix bookkeeping stale | minor | accept, R3 |
| D12 handoff 04 recovery claim inaccurate; narrow residual of the re-audit LOW | low | accept, R1 |
| D13 `old/` contains real NAS paths; audit JSON tracked | user decision | C5 |
| D14 no README screenshot, no license | documentation | C5 |
| D15 CI toolchain (1.91.1) differs from the gate host (1.98.0) | gate | C1 |

## Conditions before tagging (detail in release-readiness section 9)

- **C1:** a green `ci.yml` run on the exact commit to tag, including `cargo deny` advisories.
- **C2:** a Linux/musl B1/B2 run on the full fixture, with B1 <= 40 MiB and the B2 rescan <= 210 MiB.
- **C3:** a container smoke re-run after the wave-19 Dockerfile change.
- **C4:** (a) verify the pinned action SHAs and the `trivy-action` transitive refs; (b) tag protection; (c)
  `REMOVARR_TRUSTED_PROXIES` behind a proxy.
- **C5:** license, README screenshot, and a decision on `old/` and `docs/security/` in a public repository.
- **C6:** a real smartphone check.
- **C7:** release notes listing the known limitations.

No data-loss, authentication, secret or path-confinement item was waived.
