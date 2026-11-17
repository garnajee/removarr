# Handoff 25 - sec-audit re-audit (after wave 19)

## Outcome
COMPLETE (scoped, partial coverage). Full audit mode of the project `security-audit` skill, standard profile, scoped to `git diff 6c2256b..4adeac3` at 4adeac3 (clean). No fallback. Source-only: sandbox controls not enforceable, so no cargo or target code ran; wave-19 tests were read, not executed. 1 confirmed (low), 1 needs-validation (carried lead), 0 rejected. Validators pass (ledger 10 units, findings 2). 3 sonnet agents (hunter, candidate verifier, record verifier, all distinct). No source modified, nothing committed.

Artifacts: `/Users/jean/security-audit-skill/removarr-v2/run-2/` (coverage-ledger.json, findings.json, REPORT.md, FINDINGS-DETAIL.md, NEEDS-VALIDATION.md, run-metadata.json, architecture.md). Copies: `docs/security/reaudit/REPORT.md`, `docs/security/reaudit/findings.json`.

## Per-item verdicts
| Item | Verdict | Evidence |
|---|---|---|
| Limiter lead: warning + docs + test; status | PASS as mitigation. Lead stays NEEDS-VALIDATION: deployment-dependent, not fixed (limits and XFF trust unchanged). Warning logs only the peer IP, once per process (no spam, no header leak); it fires before origin/CSRF checks so any unauthenticated POST with XFF can consume it (hardening) | crates/web/src/auth/login.rs:136-141; auth/mod.rs:143,204; tests/auth_flow.rs:1032 |
| B1 ARR history failure | PASS: any Err or panic fails whole service, empty observation; consumers gate on state; no fail-open ARR path found | workflows/src/observe/services.rs:186-209; issues/arrgate.rs:31-48; plans/planner.rs:212; arr/src/read.rs:838 |
| B2 auto revalidation | PASS: same pure rule as plan time, same first-subject-match. Residuals (hardening): `is_not_upgrade_reason` broad; execution filters by `media.episode_ids` | actions/auto.rs:409; plans/arrplans.rs:34,44-66,236 |
| B3a dry-run recovery | PASS (no change correct) | actions/recovery.rs |
| B3b leftover temp links | OPEN, accepted (conservative) | handoff 26 |
| B4 ffprobe env | PASS: env_clear plus PATH=/usr/local/bin:/usr/bin:/bin; bare program resolved via child PATH, relative paths with slash rejected in config | matching/src/ffprobe.rs:29,120-121 |
| B5 shared redaction | PASS on strictness, no UTF-8 panic, linear; FAIL (minor) regression: finding `storage-journal-scrub-mangles-path-leaves-shared-sanitizer` (low) | domain/src/redact.rs:103-135; storage/src/codec.rs:70-85; repos/journal.rs:99-100; actions/recovery.rs:714,734. Over-redaction in job messages is display-only. Under-redaction forms (Authorization:Bearer tok, quoted values with spaces, spaced `=`, unlisted/percent-encoded keys) pre-exist: hardening |
| B6 Dockerfile /config | PASS: 65532 0700, VOLUME after; compose bind mount shadows it. Named or anonymous volume with a different uid becomes unwritable (fail-closed; Docker not run, NEEDS-VALIDATION for that scenario only) | Dockerfile:30-34; docker-compose.yml:41 |
| B6 release.yml latest | PASS: expression in `with:`, no shell, tag-only trigger, `-rc` tags get no `latest` | .github/workflows/release.yml:56 |
| Invariants | PASS: `deleteFiles=false` only control.rs:111; `importMode` copy at arr/src/commands.rs:230; single `cfg.live_mode()` at app/src/wiring.rs:176; qbit, arr, app, config untouched by the diff | as listed |

## Public contracts / deviations
None added. Docs in `docs/security/` (run-1 copy) not edited; new copies under `docs/security/reaudit/`. Deviations: no target execution; hunter prompt was a focused brief, not full companion text.

## Open TODOs
- Fix C1 (do not sanitize path leaves of journal target/observed, or fail closed in `observe_fs_intent`).
- Run `cargo test -p removarr-web --test auth_flow` in a conforming sandbox to close the lead locally; owner checks deployment facts.
- Optional hardening: redaction forms above, burnable once-flag, episode_ids filter, compose note on volumes and uid.
- Then `release-gate`.

## Commands run
| Command | Result |
|---|---|
| `git diff 6c2256b..4adeac3` reads, grep invariants | as above |
| `node .claude/skills/security-audit/validate-coverage-ledger.cjs run-2/coverage-ledger.json` | PASS: 10 coverage units valid |
| `node .claude/skills/security-audit/validate-findings.cjs run-2/findings.json` | PASS: 2 findings valid |
| cargo, docker, network | not used |
