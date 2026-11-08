# Agent execution order

Agent numbers are identifiers, not execution order. Run agents wave by wave; agents in
the same wave may run in parallel because they own disjoint modules (module ownership
is published by `system-arch` in `docs/architecture.md`).

Every agent reads its dependencies' handoffs in `docs/handoffs/` before starting and
writes its own handoff when done.

## Waves

| Wave | Agents (parallel) | Depends on |
|---|---|---|
| 0 | `00-legacy-audit` | — |
| 1 | `01-system-arch` | 00 |
| 2 | `02-rust-scaffold` | 01 |
| 3 | `03-domain-model` | 01, 02, 00 |
| 4 | `04-db-layer`, `05-config-secrets` | 03 (05 also 02) |
| 5 | `06-fs-engine`, `08-qbit-api`, `09-arr-api`, `10-jellyfin-api` | 03, 05 (06 also 04 contracts) |
| 6 | `07-media-match` | 06, 03, 09 interfaces |
| 7 | `11-issue-engine` | 06, 07, 08, 09, 10, 04 |
| 8 | `12-plan-engine` | 11, 03, 04 |
| 9 | `13-action-runner` | 12, 04, 07, 08, 09, 10 |
| 10 | `14-job-runner` | 04, 11, 13 |
| 11 | `15-reporting` | 04, 11, 13, 14 event schemas |
| 12 | `16-backend-api` | 13, 14, 15 |
| 13 | `17-auth-guard`, `18-ux-design`, `20-devops` | 16 (17 also 05, 04; 20 needs health routes) |
| 14 | `19-web-ui` | 16, 17, 18, 15 |
| 15 | `22-integration-tests` | 04–16 |
| 16 | `23-e2e-perf` | 17, 19, 20, 22 |
| 17 | `24-sec-setup` | all tests passing |
| 18 | `25-sec-audit` | 24 (or explicit `/security-review` fallback) |
| 19 | `26-sec-fix`, then `25-sec-audit` targeted re-audit | 25 |
| 20 | `27-release-gate` | everything above |

`21-unit-tests` runs incrementally from wave 3 onward: after each wave it updates the
legacy parity matrix and fills test gaps. Feature agents ship their own unit tests.

## Risk tiers inside the full V1 scope

All features ship in V1, but destructive capabilities are built and gated in order of
increasing risk (`old/CONTEXTE_RACINE_APP_NAS.md` §7.2). `plan-engine`,
`action-runner`, and `integration-tests` implement and verify one tier before starting
the next:

1. **Read-only**: scan, issues, reports, full dry-run path.
2. **Exact-copy consolidation**: permission probe, full compare, temporary hardlink,
   atomic replace, qBittorrent stop/recheck/resume.
3. **Deletions under completed**: unlinked files, junk outside torrents, stagnant
   incomplete torrents, task removal with `deleteFiles=false`.
4. **ARR release replacement**: quality snapshots, coverage proof, ffprobe checks,
   episode numbering mappings.
5. **Automatic mode**: only plans satisfying every automatic rule; disabled by default.

A tier is complete only when its crash-boundary and dry-run-no-mutation tests pass.
