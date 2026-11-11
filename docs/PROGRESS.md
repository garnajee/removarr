# Build progress

Maintained by the orchestrator (see section 10 of `MASTER-PROMPT.md`). Update after every agent.
Status values: `todo`, `running`, `done`, `blocked`.

- Branch: `autonomous-build` (created from `main` at 4fa9e32)
- Last commit: none
- Blocking cause: none
- Orchestrator interpretation: downloading pinned build dependencies from public package registries (crates.io, the pinned HTMX asset with a checksum) is build tooling, not "real-world access" under rule 9 / stop condition (c). No access to the user's services, NAS or secrets.

## Agents

| Wave | Agent | Status | Notes |
|---|---|---|---|
| 0 | `00-legacy-audit` | done | 32 tests mapped, 51 INV, 30 REJ, 32 CON, 7 open questions for system-arch (OQ-01/05/06/07/09/11/12) |
| 1 | `01-system-arch` | done | architecture.md (19 sections), ADR 0001-0010, 13 crates, OQs resolved; versions from memory, 02 must confirm |
| 2 | `02-rust-scaffold` | done | 14 crates, Cargo.lock, all versions resolved in range, no deviations |
| 3 | `03-domain-model` | done | 80 domain tests; added ActionTarget, evidence_level/completeness; domain frozen |
| 4 | `04-db-layer` | done | 60 tests; STRICT tables, intent durable (synchronous=FULL), 7 documented deviations |
| 4 | `05-config-secrets` | done | 44 tests; LiveMode token, PathMapper, redaction, examples/removarr.toml + .env.example |
| 5 | `06-fs-engine` | done | 33 tests; read-only confined walk, 19.8k files in 59 ms (warm); mutate.rs left to 13 per §15.2 |
| 5 | `08-qbit-api` | done | 37 tests; deleteFiles literal false once; 4.x/5.x; fake_qbit |
| 5 | `09-arr-api` | done | 51 tests; importMode copy literal; grab gated; opaque release tokens; snapshot/correlate to move into domain (option A) |
| 5 | `10-jellyfin-api` | done | 22 tests; reader/commander split, LiveMode-gated refresh |
| 6 | `07-media-match` | done | 63 tests; funnel (inode, name+size, parse, samples-reject), ContentVerifier only proof, ffprobe args-only; resumed after rate limit |
| 6 | `03-domain-model` patch (arr snapshot/correlate into domain) | done | option A; single implementation in removarr_domain::arr |
| 7 | `11-issue-engine` | done | 96 tests; 13 categories; refresh zero fs calls; orchestrator confirmed strict per-version qBit allowlist (pausedUP 4.x only, stoppedUP 5.x only) and `note:` evidence convention |
| 8 | `12-plan-engine` | done | plans for all tiers, tier-tagged and capability-gated; no mutation port referenced |
| 8 | `03-domain-model` patch 2 (ARR-validated auto deletion evidence, service-command verification steps) | done | 603 tests; golden hash unchanged; gap 3 accepted as deviation |
| 8 | `12-plan-engine` follow-up (adopt patch 2, torrent staleness by eligibility class) | done | 607 tests; wanted-file set checked live by 13 at revalidation (accepted); auto plans omit companion actions (confirmed, cleanup stays manual) |
| 9 | `13-action-runner` | running | stage A done (tiers 1-2, 634 tests); CP2-tier3 PASS w/ condition; stage B step 1 done (F01, F02, F04-F14 fixed, 647 tests); F03 needs domain patch 3; tier 3 pending |
| 9 | `03-domain-model` patch 3 (TorrentWantedFiles precondition, ChangedSincePlanning/Interrupted blocker codes) | running | required by CP2T3-F03/F10 |
| 9 | `12-plan-engine` emit TorrentWantedFiles | todo | |
| 9 | `13-action-runner` stage B step 2 (tier 3) | todo | |
| 10 | `14-job-runner` | todo | |
| 11 | `15-reporting` | todo | |
| 12 | `16-backend-api` | todo | |
| 13 | `17-auth-guard` | todo | |
| 13 | `18-ux-design` | todo | |
| 13 | `20-devops` | todo | |
| 14 | `19-web-ui` | todo | |
| 15 | `22-integration-tests` | todo | |
| 16 | `23-e2e-perf` | todo | |
| 17 | `24-sec-setup` | todo | |
| 18 | `25-sec-audit` | todo | |
| 19 | `26-sec-fix` | todo | |
| 20 | `27-release-gate` | todo | |
| continuous | `21-unit-tests` (from wave 3) | running | pass 1 done (domain, 11 proptests, parity-matrix.md); pass 2 done (+14 tests); pass 3 done (+18 tests; found qbit fail-closed gap, fixed by 08); pass 4 done (+13 tests, no bugs); next pass after wave 8 (workflows issues + plans) |

## Checkpoint reviews

| Checkpoint | Verdict | Iterations | Report |
|---|---|---|---|
| CP1 architecture (after wave 1) | PASS (iter 2); follow-up F20 + F19/F21-F25 fixed | 2/3 | `docs/reviews/cp1.md` |
| CP2 tier 3 (deletions) | PASS with condition (iter 1): fix F01-F04 before raising IMPLEMENTED_TIER to T3; CP2-tier4 re-checks them | 1/3 | `docs/reviews/cp2-tier3.md` |
| CP2 tier 4 (ARR replacement) | pending | 0/3 | `docs/reviews/cp2-tier4.md` |
| CP2 tier 5 (automatic mode) | pending | 0/3 | `docs/reviews/cp2-tier5.md` |
| CP3 release gate | pending | — | `docs/release-readiness.md` |

## Last quality gate

After wave 9 stage A: fmt OK, clippy -D warnings OK, tests OK (634 passed, 1 ignored = opt-in real-ffprobe test). Safety greps OK (deleteFiles literal false only in qbit/control.rs; importMode copy literal). Toolchain: cargo/rustc 1.98.0 (Homebrew, no rustup), Docker 29.7.2, ffprobe available.
