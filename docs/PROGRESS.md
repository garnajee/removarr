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
| 9 | `13-action-runner` | done | tiers 1-4 executable; tier 5 runner checks built behind T4 gate (raised after CP2 tier 5 final) — stage A done (tiers 1-2, 634 tests); CP2-tier3 PASS w/ condition; stage B step 1 done (F01, F02, F04-F14 fixed, 647 tests); F03 needs domain patch 3; tier 3 pending |
| 9 | `03-domain-model` patch 3 (TorrentWantedFiles precondition, ChangedSincePlanning/Interrupted blocker codes) | done | 649 tests; golden hash unchanged |
| 9 | `12-plan-engine` emit TorrentWantedFiles | done | 651 tests; StaleReason::WantedFilesChanged |
| 9 | `13-action-runner` stage D (CP2T5 fixes + runner-side T5, gate stays T4) | done | 721 tests; tier5-test-gate feature absent from production tree; one-word visibility change in plans/mod.rs |
| 9 | `12-plan-engine` tier-5 policy requirements | done | 727 tests; auto_plan_defects, per-action subjects, planner-built queue removal executed end to end |
| 9 | `13-action-runner` stage C (CP2T4 fixes + tier 4) | done | 706 tests; IMPLEMENTED_TIER=T4; automatic plans refused; domain trait addition FsMutator::remove_media_file (default refuse) flagged for CP2-tier5 |
| 9 | `13-action-runner` stage B step 2 (tier 3) | done | 682 tests; IMPLEMENTED_TIER=T3; F03/F10 fixed; FixMode exempt from live-holder check (for CP2 review); T3-12 deferred to tier 5, T3-19 owned by 14; agent model switched to sonnet (user request) |
| 10 | `14-job-runner` | done | 58 jobs tests; single execute lane, recover-before-execute, two-phase auto cycle, failed sets from journal; tier5 gate absent from production |
| 11 | `15-reporting` | done | 40 tests; read-only SQL read model, gain kinds separated, CSV formula-neutralized, bounded sanitized logs, svg.rs geometry for web-ui reuse; RPT-05/08/09 not produced (deviation for CP3) |
| 12 | `16-backend-api` | done | 878 tests; release build OK; live_mode() called once (app wiring); default guard refuses all protected routes until auth-guard; plan list/media/explorer/settings/charts/static assets left to web-ui |
| 13 | `17-auth-guard` | done | Argon2 off-thread constant-time, encrypted session cookie, CSRF on every POST, IP and (IP,user) limits with Retry-After, trusted proxies, security headers, `hash-password` subcommand |
| 13 | `18-ux-design` | done | docs/ux-spec.md + docs/ux/wireflows.md, ~330 new translation keys, open questions for 16/19 |
| 13 | `20-devops` | done | musl static image + ffprobe, non-root, hardlink under single mount verified, compose 127.0.0.1, CI + release (v*.*.* only, multi-arch, SBOM, Trivy), actions pinned by SHA, image ghcr.io/garnajee/removarr-v2 |
| 14 | `19-web-ui` | done | 943 tests; no custom JS, pinned HTMX 2.0.4 with SHA-256/SRI, 499 keys en=fr, CSRF on forms; resumed after session end |
| 14 | `16-backend-api` follow-up (web-ui backend gaps 1-5, 8) | running | settings/explorer/media pages accepted as scope cut (not required by MASTER §6) |
| 14 | `20-devops` README follow-up (hash-password, $$ in compose, cookie_secure, trusted proxies) | done | compose config validated against fresh .env.example copy |
| 15 | `22-integration-tests` | todo | |
| 16 | `23-e2e-perf` | todo | |
| 17 | `24-sec-setup` | todo | |
| 18 | `25-sec-audit` | todo | |
| 19 | `26-sec-fix` | todo | |
| 20 | `27-release-gate` | todo | |
| continuous | `21-unit-tests` (from wave 3) | running | pass 1 done (domain, 11 proptests, parity-matrix.md); pass 2 done (+14 tests); pass 3 done (+18 tests; found qbit fail-closed gap, fixed by 08); pass 4 done (+13 tests, no bugs); pass 5 done (+4 tests, no bugs, matrix re-checked) |

## Checkpoint reviews

| Checkpoint | Verdict | Iterations | Report |
|---|---|---|---|
| CP1 architecture (after wave 1) | PASS (iter 2); follow-up F20 + F19/F21-F25 fixed | 2/3 | `docs/reviews/cp1.md` |
| CP2 tier 3 (deletions) | PASS with condition (iter 1): fix F01-F04 before raising IMPLEMENTED_TIER to T3; CP2-tier4 re-checks them | 1/3 | `docs/reviews/cp2-tier3.md` |
| CP2 tier 4 (ARR replacement) | PASS with condition (iter 1): fix CP2T4-F01 (runner must refuse automatic plans until tier 5) first; CP2-tier5 re-checks it + T4-01..T4-20 | 1/3 | `docs/reviews/cp2-tier4.md` |
| CP2 tier 5 (automatic mode) | PASS with condition (iter 1): build T5-01..T5-22 across 13/12/14, keep IMPLEMENTED_TIER=T4 until a dedicated final CP2-tier5 review (also checks CP2T5-F01) | 1/3 |
| CP2 tier 5 final (before raising IMPLEMENTED_TIER to T5) | PASS with condition (iter 1): raise gate in the same change as CP2T5F-F01 fix + gate-raise steps; CP3 checks it | 1/3 |
| 10 | gate raise to T5: `13-action-runner` (constant, F02-F04, F06, tests) + `14-job-runner` (F01, F05, F07, F08, tests), one commit | done | 798 tests; IMPLEMENTED_TIER=T5; auto mode still off by default; for CP3: workflows keeps tier5-test-gate for its own tests only (absent from production tree), F08 full-root rescan and create_auto text match are recorded deviations | `docs/reviews/cp2-tier5-final.md` | `docs/reviews/cp2-tier5.md` |
| CP3 release gate | pending | — | `docs/release-readiness.md` |

## Last quality gate

After wave 14: fmt OK, clippy -D warnings OK, release build OK, tests OK (943 passed, 1 ignored = opt-in real-ffprobe test). Safety greps OK (deleteFiles literal false only in qbit/control.rs; importMode copy literal). Toolchain: cargo/rustc 1.98.0 (Homebrew, no rustup), Docker 29.7.2, ffprobe available.
