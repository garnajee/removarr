---
name: issue-engine
description: Classify all storage issues after scanners, matching, and connectors are implemented.
model: sonnet
color: green
---

Implement issue detection and explanation without performing actions.

Dependencies: `fs-engine`, `media-match`, all three connectors, and `db-layer`.

Detect these categories:
1. Incomplete files stagnant beyond the configured threshold.
2. Completed downloads lacking a verified media-tree hardlink.
3. Content-identical physical copies that should be consolidated.
4. Completed files without torrents, torrents with missing files, and media files
   without a completed source (informational only: never a deletion candidate).
5. Owner/group different from the configured `expected_owner`, mode, or
   parent-directory permission anomalies.
6. Configured junk files, archives, and orphan sidecars that belong to no qBittorrent
   torrent (deleting torrent content breaks seeding). Samples are never junk.
7. Torrents outside the qBittorrent eligibility allowlist (see `qbit-api`), which
   prohibits destructive consideration.

For every issue, store category, severity, evidence, provenance, age, and blockers.
Distinguish observed facts, candidates, proven matches, and recommendations.
Aggregate dashboard counts and potential space without double counting inodes.

Rules:
- Samples are protected by default, even when categorized as junk-like content.
- An incomplete scan or path mapping blocks automatic action in that scope.
- Only allowlisted complete states are eligible; every other or unknown state is protected.
- Files under media roots are never proposed for automatic deletion.
- Unknown ARR identity or quality requires manual review.
- Never mutate files, database state outside issue persistence, or services.

Tests must cover every category, overlap, precedence, and fail-closed condition.
Handoff deterministic issue IDs and evidence to `plan-engine` and `reporting`.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/11-issue-engine.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
