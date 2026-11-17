# Handoff 25 - sec-audit (wave 18)

## Outcome

**COMPLETE, 0 confirmed findings, 1 needs-validation lead.** Full audit mode, `standard` profile, Cloudflare `security-audit` skill (project install, commit c1c8a8c), no fallback to `/security-review`. Both skill validators pass. Source-only: no target code was executed (the skill's OS sandbox controls were not established on this host and the orchestrator forbade parallel builds), so nothing could reach `confirmed`. No cargo, Docker, network or live service was used. No source was modified and nothing was committed.

## Delivered scope

| Item | Value |
|---|---|
| Output directory (outside the target) | `/Users/jean/security-audit-skill/removarr-v2/run-1` |
| Source ref | `cb62ef1df38b0a0a1b3042b2ff907dec77414b7c`, branch `autonomous-build`, worktree clean |
| Artifacts | `coverage-ledger.json`, `findings.json`, `REPORT.md`, `FINDINGS-DETAIL.md`, `NEEDS-VALIDATION.md`, plus `run-metadata.json`, `architecture.md`, `hunter-brief.md`, `agents/` (empty scratch/artifacts, nothing promoted) |
| Agents spent | 33: 4 recon, 18 hunters (5 waves), 7 coverage critics, 2 candidate verifiers, 2 record verifiers. All sonnet; finder and verifier always different agents |
| Coverage | 38 units: 36 covered, 2 candidate, 0 blocked, 0 deferred, 0 out_of_scope. Wave 1 seeded 20; critics added 8, 5, 2, 3 units in waves 2-5. Last two critics (post-wave-5 and distinct final-clean) returned `stop: true` |

## Confirmed findings

None (0 critical, 0 high, 0 medium, 0 low).

## Needs validation (owner per `docs/architecture.md` 15.2)

| Id (fingerprint) | Component / owner agent | File:line | Summary |
|---|---|---|---|
| `web-login-limiter-shared-bucket-without-trusted-proxies` | web auth limiter (17-auth-guard); default and warning in config (05-config-secrets); compose/README guidance (20-devops) | `crates/web/src/auth/limiter.rs:135`, `crates/web/src/auth/login.rs:166`, `crates/config/src/load.rs:426,859,867`, `docker-compose.yml:31` | With `trusted_proxies` empty behind a reverse proxy, all clients share one limiter bucket; 5 bad logins with the operator username (or 20 any) per 15 min lock the operator out of login (correct password also refused, `reserve()` runs first). Blockers: proxy presence, `REMOVARR_TRUSTED_PROXIES`, Docker peer address. Local plan: extend `crates/web/tests/auth_flow.rs`. Details in `NEEDS-VALIDATION.md` |

Rejected (retained in `findings.json`, not a finding): `matching-ffprobe-unrestricted-demuxer-local-file-read` (07-media-match): no attacker-visible output channel.

## Hardening notes for `26-sec-fix` (not findings; no reachable boundary violation shown)

Full list in `REPORT.md`. Most useful, with owners:
- 07-media-match, `crates/matching/src/ffprobe.rs`: `env_clear()` and a `-f`/`-formats` allowlist (child inherits all REMOVARR secrets).
- 13-action-runner, `crates/workflows/src/actions/auto.rs` `arr_validated_blockers`: execution-time ARR rejection check weaker than plan-time `is_not_upgrade_reason` (agent 12, `plans/arrplans.rs`, whose predicate is also broad); no replacement episode-coverage recheck.
- 11-issue-engine, `crates/workflows/src/observe/services.rs`: `history_for_download(...).unwrap_or_default()` hides a failed ARR history call.
- 13-action-runner, `crates/workflows/src/actions/recovery.rs`: dry-run recovery leaves app-stopped torrents stopped permanently; single-listing "gone" decision; leftover `.removarr-link-*` hardlinks never swept (visible to scans, add nlink).
- 04-db-layer `storage/src/codec.rs` `scrub` and 14-job-runner `jobs/src/events.rs` `redact`: weaker than `reporting::sanitize`.
- 03-domain-model: `EntrySignature` has no ctime; plan hash is unkeyed.
- 20-devops: `/config` 1777, `latest` moved by prerelease tags, scan/push image split, no mem/pids limits, `./config` not in `.gitignore`, `$$` guidance mismatch between `.env.example` and README.
- 16/17 web: locale and flash cookies lack `Secure`; no connection cap or header-read timeout.

Invariants held in source: qBit `deleteFiles=false` (single send site, literal), ARR `importMode=copy`, `removeFromClient=false`/`blocklist=false`, LiveMode token unreachable in dry-run and absent from the release build graph, hash-bound plan approval, intent-before-mutation journal, dirfd/`O_NOFOLLOW` confinement and per-step signature recheck, no Askama raw output, default-deny route guards, no SSRF redirect/proxy follow, bound-parameter SQL, CI with no `pull_request_target` and SHA-pinned actions.

## Public contracts

None added. Report artifacts are in the external output directory only. `docs/security/` was not used.

## Deviations from the architecture / skill text

- No local execution (sandbox controls unproven). Consequence: confirmed verdicts were impossible; one lead stays needs-validation and the owner can resolve it with the local test described.
- Hunter prompts pointed to a parent-written `hunter-brief.md` and to companion blocks by file and section rather than pasting every block verbatim; the parent transcribed condensed checks from hunter JSON into the ledger; ledger bookkeeping fields `_key`, `_hunter`, `_hardening`, `_added_by` were added (validator accepts them).
- The last two coverage critics answered in about 20-26 s; their clean result is recorded but is weaker assurance than a long review.
- Critic placeholder `reassign_ids` in wave 1 was invalid and treated as empty; the concerns it named (plan hash itemization, matching, resource_dos paths) were covered by the new wave-2/3 units.
- Dependency CVE status was not checked (needs network for `cargo deny`).

## Open TODOs

- `26-sec-fix` (or the owner): decide on the needs-validation lead; add the unit test in `crates/web/tests/auth_flow.rs`; optionally apply hardening notes.
- A targeted re-audit (wave 19) should re-check any changed source against this run's ledger; units carry per-file `reviewed_paths`.
- Orchestrator: update `docs/PROGRESS.md` row 18 and decide whether to commit this handoff.

## Commands run and results

| Command | Result |
|---|---|
| Read `docs/handoffs/24-sec-setup.md`, `MASTER-PROMPT.md` section 10, skill `SKILL.md`, `RECONNAISSANCE.md`, `HUNTING.md`, `VALIDATION-AND-REPORTING.md`, `report-schema.json` | skill loaded, full audit mode selected |
| `node .claude/skills/security-audit/validate-coverage-ledger.cjs <out>/coverage-ledger.json` | `PASS: 38 coverage units valid` |
| `node .claude/skills/security-audit/validate-findings.cjs <out>/findings.json` | `PASS: 2 findings valid` |
| Source-only grep/read through sub-agents (no cargo, rustc, docker, network) | no processes left running |
| `git status --short` in target before handoff | clean (only this handoff file added afterwards) |

The skill's own `*.test.cjs` files were not run (third-party code; not needed).
