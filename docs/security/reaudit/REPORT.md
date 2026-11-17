# Security re-audit report: removarr-v2, run-2 (targeted, wave 19)

**Scope: PARTIAL.** Scoped audit of `git diff 6c2256b..4adeac3` and the code paths it touches. Unchanged source is not re-reviewed (see run-1, which covered 38 units at cb62ef1). Not an exhaustive claim.

- Source ref: 4adeac3, branch autonomous-build, clean worktree. Profile: standard (scoped). Skill: project `security-audit` (no fallback used).
- Execution: source-only. The skill's sandbox controls (limits, empty env, read-only target) are not enforceable on this host, so no target code ran, including cargo. Tests added by wave 19 were read, not executed.
- Agents: 3 (1 hunter, 1 candidate verifier, 1 record verifier; all sonnet; finder and verifier differ).
- Validators: `validate-coverage-ledger.cjs` PASS (10 units), `validate-findings.cjs` PASS (2 findings).

## Result
| Verdict | Count |
|---|---|
| Confirmed | 1 (low) |
| Needs validation | 1 (carried lead, deployment dependent) |
| Rejected | 0 |

### Confirmed
- `storage-journal-scrub-mangles-path-leaves-shared-sanitizer` (low): the shared sanitizer now used by `storage::codec::scrub_value` can rewrite path leaves of journal `target`/`observed`; after a crash in the intent/result window, recovery stats the mangled path and reports an unchanged file as `Mutated` (ExecutedUnverified plus extra recheck). No wrong mutation. Largely pre-existing class, widened by 4adeac3. Fix: do not sanitize path-typed leaves, or return Unobservable when the scrubbed target differs.

### Needs validation
- `web-login-limiter-shared-bucket-without-trusted-proxies`: the wave-19 change adds visibility only (one-time warning, docs, test); limits and header trust are unchanged. Status: deployment-dependent, not "fixed". See `NEEDS-VALIDATION.md`.

## Wave-19 remediation map verdicts
| Item | Verdict | Evidence |
|---|---|---|
| Limiter lead (warning, docs, test) | PASS as a mitigation; lead stays NEEDS-VALIDATION (deployment) | login.rs:136-141, mod.rs:143,204, auth_flow.rs:1032 |
| B1 history failure fails ARR service | PASS | services.rs:186-209; arrgate.rs:31-48; planner.rs:212 |
| B2 execution-time ARR rule = plan time | PASS (residual hardening) | auto.rs:409; arrplans.rs:44-66,236 |
| B3a dry-run recovery | PASS (no change is correct) | recovery.rs (no live controller by design) |
| B3b leftover temp links | Open, not fixed (accepted, conservative) | handoff 26 |
| B4 ffprobe env_clear + PATH | PASS | ffprobe.rs:29,120-121 |
| B5 shared redaction | PASS for strictness and no panic; FAIL-minor regression: C1 (journal path leaves) | redact.rs:103-135; codec.rs:70-85; journal.rs:99-100; recovery.rs:734 |
| B6 Dockerfile /config 0700 | PASS; named or anonymous volume with a different uid becomes unwritable (fail-closed, undocumented in compose) | Dockerfile:30-34; docker-compose.yml:41 |
| B6 release.yml latest logic | PASS | release.yml:56 |
| Invariants | PASS | control.rs:111; commands.rs:230; wiring.rs:176 |

## Hardening notes (no boundary violation)
- `redact.rs` still under-redacts: `Authorization:Bearer tok` (no space), quoted values with spaces, `password = x`, percent-encoded or unlisted key names (x-emby-token, authToken, sessionid), `Authorization:` followed by a newline. These existed in the old reporting sanitizer.
- The one-time limiter warning fires before origin/CSRF checks, so any unauthenticated POST with `X-Forwarded-For` consumes it.
- `is_not_upgrade_reason` matches any rejection containing "upgrade"; execution still filters current files by `media.episode_ids` (empty accepts any).
- A single failing history record now disables ARR-dependent proposals for the whole service (safe, availability trade-off).
- `stalled.rs:187` turns a failed qBittorrent listing into empty (later revalidation uses fresh reads; not shown reachable).

## Coverage
10 ledger units: 7 covered, 2 candidate (the two records above), 1 out_of_scope (everything unchanged since cb62ef1). Prior ledger read: run-1 (38 units, standard profile).
