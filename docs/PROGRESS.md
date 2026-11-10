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
| 4 | `04-db-layer` | running | |
| 4 | `05-config-secrets` | running | |
| 5 | `06-fs-engine` | todo | |
| 5 | `08-qbit-api` | todo | |
| 5 | `09-arr-api` | todo | |
| 5 | `10-jellyfin-api` | todo | |
| 6 | `07-media-match` | todo | |
| 7 | `11-issue-engine` | todo | |
| 8 | `12-plan-engine` | todo | |
| 9 | `13-action-runner` | todo | |
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
| continuous | `21-unit-tests` (from wave 3) | running | pass 1: domain crate |

## Checkpoint reviews

| Checkpoint | Verdict | Iterations | Report |
|---|---|---|---|
| CP1 architecture (after wave 1) | PASS (iter 2); follow-up F20 + F19/F21-F25 fixed | 2/3 | `docs/reviews/cp1.md` |
| CP2 tier 3 (deletions) | pending | 0/3 | `docs/reviews/cp2-tier3.md` |
| CP2 tier 4 (ARR replacement) | pending | 0/3 | `docs/reviews/cp2-tier4.md` |
| CP2 tier 5 (automatic mode) | pending | 0/3 | `docs/reviews/cp2-tier5.md` |
| CP3 release gate | pending | — | `docs/release-readiness.md` |

## Last quality gate

After wave 3: fmt OK, clippy -D warnings OK, tests OK (83 passed). Toolchain: cargo/rustc 1.98.0 (Homebrew, no rustup), Docker 29.7.2, ffprobe available.
