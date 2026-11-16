# Handoff 24 - sec-setup (wave 17)

## Outcome

**INSTALLED.** The official Cloudflare `security-audit` skill is installed at project scope for Claude Code and is discoverable in a fresh session. `sec-audit` (`25-sec-audit`) may use it; no fallback to `/security-review` is needed.

## Delivered scope

| Item | Value |
|---|---|
| Repository URL | `https://github.com/cloudflare/security-audit-skill` (official, as required) |
| Skill selected | `security-audit` only (repo ships exactly one skill, `skills/security-audit/SKILL.md`) |
| Resolved commit | `c1c8a8c1471069fb0e188eeaff69b8e8db6564a8` (`git ls-remote ... HEAD`, read-only; matches `skills-lock.json`) |
| License | MIT, Copyright (c) 2025-2026 Cloudflare, Inc. (LICENSE at pinned commit) |
| Scope | project only (no `-g`), Claude Code agent only (`-a claude-code`) |
| Installed skill path | `/Users/jean/Documents/projets/mon-github/removarr-v2/.claude/skills/security-audit/` |
| Entry file | `/Users/jean/Documents/projets/mon-github/removarr-v2/.claude/skills/security-audit/SKILL.md` |
| Install mode | copy (`--copy` semantics chosen by the installer for Claude Code); no `.agents/` directory was created in the project |
| Lock file | `/Users/jean/Documents/projets/mon-github/removarr-v2/skills-lock.json` (created by the installer; records source, commit, skillPath, computedHash) |

## Public contracts (for `25-sec-audit`)

- Discovery evidence: a fresh `claude -p` session listed `security-audit` among the available skills (exact output: "Skills: security-audit, dataviz, ..., security-review, ... Yes, `security-audit` is one of them.").
- SKILL.md frontmatter: `name: security-audit`; description starts "Security guidance and vulnerability review for codebases, APIs, services, CLI tools, libraries, and daemons."
- Operating modes: guidance by default; the full six-phase audit (which writes run artifacts) runs only on an explicit audit/pen-test/full review request. Run artifacts that the skill itself creates (`run-metadata.json`, `architecture.md`, `coverage-ledger.json`, `findings.json`, `FINDINGS-DETAIL.md`, `NEEDS-VALIDATION.md`, `REPORT.md`) are absent at install time, as expected.
- Companion files present (all under the installed path): `ATTACK-CLASSES.md`, `AI-AND-LLM.md`, `CLIENT-SIDE.md`, `CLOUD-AND-DEPLOYMENT.md`, `DATA-ISOLATION-AND-LIFECYCLE.md`, `DESKTOP-MOBILE-AND-LOCAL-IPC.md`, `HUNTING.md`, `MEMORY-SAFETY-AND-BINARY.md`, `PROTOCOLS-RPC-AND-MESSAGING.md`, `RECONNAISSANCE.md`, `RESOURCE-EXHAUSTION-AND-AVAILABILITY.md`, `SUPPLY-CHAIN-AND-RELEASE.md`, `VALIDATION-AND-REPORTING.md`, `WEB-PROTOCOL-AND-AUTH.md`.
- Schema: `report-schema.json` (parses as valid JSON).
- Validators: `validate-findings.cjs`, `validate-coverage-ledger.cjs` (with their `.test.cjs` files shipped alongside; present, NOT executed here).

## Verification

- `SKILL.md` SHA-256 `5e3e96a1e438d8f35fef0a1e38f02d4f00e2bdf910401b7dfe00e6779c6dac85` equals the upstream file at the pinned commit.
- Every companion file referenced by SKILL.md that is a shipped file exists (checked by grep on SKILL.md names). Names not present are run artifacts listed above, not missing companions.
- Validators parsed with `node --check` (syntax only). Their test files were deliberately not run: they are third-party code and the rules forbid running tests/third-party code in this wave.
- Installed copy matches upstream SKILL.md hash.

## Deviations from the architecture

- The installer reported a canonical path `.agents/skills/security-audit` but did not create it; the project copy is only under `.claude/skills/security-audit/`. No effect on discovery.
- `skills-lock.json` is a new untracked file at the project root (installer artifact, needed for reproducible `skills experimental_install`). Not committed in this wave; the orchestrator decides whether to commit it with the wave-17 commit.
- `docs/PROGRESS.md` was not edited by this agent (status update left to the orchestrator).

## Open TODOs

- Orchestrator: decide whether `.claude/skills/security-audit/` and `skills-lock.json` are committed in the wave-17 commit (both currently untracked).
- Orchestrator: update `docs/PROGRESS.md` row 17 to `done` with outcome INSTALLED and the commit above.
- Validators' own tests have not been run; `25-sec-audit` should run them only if its own policy allows executing third-party code.

## Commands run and results

| Command | Result |
|---|---|
| `command -v node npm npx; node --version; npm --version; npx --version` | `/opt/homebrew/bin/{node,npm,npx}`; node v26.8.1, npm 11.19.0, npx 11.19.0 |
| `npx --yes skills --help` | Project-local options: default scope is project; `-g` is global (not used); `-a/--agent`, `-s/--skill`, `-l/--list`, `-y/--yes`, `--copy` |
| `npx --yes skills add https://github.com/cloudflare/security-audit-skill --list` | Cloned repo; "Found 1 skill": `security-audit` |
| `git ls-remote https://github.com/cloudflare/security-audit-skill HEAD` | `c1c8a8c1471069fb0e188eeaff69b8e8db6564a8 HEAD` |
| `npx --yes skills add https://github.com/cloudflare/security-audit-skill --skill security-audit -a claude-code -y` | "Installed 1 skill: security-audit (copied)" to `.claude/skills/security-audit` |
| `find .claude/skills/security-audit`, `cat skills-lock.json` | 22 files present; lock pins commit `c1c8a8c1...` |
| `node -e 'JSON.parse(...report-schema.json)'` | schema JSON valid |
| `node --check validate-findings.cjs`, `node --check validate-coverage-ledger.cjs` | syntax OK (not executed) |
| `shasum -a 256 SKILL.md` (installed and upstream via raw.githubusercontent.com at pinned commit) | identical hash `5e3e96a1...6dac85` |
| `curl -fsSL .../c1c8a8c1.../LICENSE` | MIT License, Cloudflare, Inc. |
| `claude -p "List the skill names ..."` (fresh session) | `security-audit` listed among available skills: PASS |
| `git status --short` | `?? .claude/skills/`, `?? skills-lock.json`; no tracked file changed; no application source touched |

Not run (by rule): the audit, the application, cargo, tests, Docker/Colima, validator test files, any target code. Nothing installed globally. No commit made.
