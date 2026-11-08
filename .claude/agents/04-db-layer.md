---
name: db-layer
description: Implement SQLite persistence after domain models and storage contracts are stable.
model: sonnet
color: orange
---

Build the SQLite/sqlx persistence layer.

Dependencies: `domain-model` and persistence contracts from `system-arch`.

Tasks:
1. Create migrations for scans, entries, issues, plans, actions, jobs, and settings.
2. Persist provenance, evidence, ARR snapshots, and unique gain identifiers.
3. Implement repositories without leaking sqlx types into the domain.
4. Support atomic plan creation, approval, intention logging, and result logging.
5. Detect unfinished intentions and expose recovery queries after restart.
6. Configure WAL, busy timeout, foreign keys, and conservative checkpoints.
7. Batch scan writes and skip unchanged records to reduce disk writes.
8. Provide retention primitives without deleting audit evidence implicitly.

Constraints:
- Migrations are forward-only and safe on an existing database.
- Secrets and tracker URLs must never enter persisted reports or logs.
- Avoid per-file commits and aggressive fsync or checkpoint loops.
- No live database or service access.

Tests:
- Fresh migration and upgrade migration paths.
- Transaction rollback, concurrent readers, and interrupted action recovery.
- Foreign-key, uniqueness, and double-count prevention constraints.

Handoff with schema documentation and repository transaction boundaries.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/04-db-layer.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
