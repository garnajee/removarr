---
name: plan-engine
description: Build immutable action plans after issue evidence and quality snapshots exist.
model: opus
color: orange
---

Implement planning and approval without executing mutations.

Dependencies: `issue-engine`, `domain-model`, `db-layer`, and connector snapshots.

Tasks:
1. Convert selected issues into explicit ordered actions and dependencies.
2. Support one-item and bulk selection without implicit preselection.
3. Capture filesystem signatures, scan completeness, service state, and ARR quality.
4. Calculate logical, attributable, observed, added, and net gain fields.
5. Hash canonical plan content and persist plans immutably.
6. Define expiry from file, configuration, mapping, service, or ARR changes.
7. Validate that every action has evidence, preconditions, and verification steps.
8. Produce a complete dry-run preview using the same planning path.
9. Require explicit approval records for destructive plans.

Rules:
- Plans never adapt silently to changed state.
- Bulk folder deletion is all-or-nothing and includes every sidecar in the preview.
- Hardlink repair direction is completed source to organized media destination.
- Quality replacement requires ARR approval and complete media coverage.
- No deletion plan may target protected samples or incomplete scan areas.

Tests:
- Stable hashes, expiration, bulk atomicity, approval, and double-count prevention.
- ARR profile changes and filesystem changes invalidate old plans.

Handoff the execution contract to `action-runner`.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/12-plan-engine.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
