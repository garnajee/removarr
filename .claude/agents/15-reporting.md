---
name: reporting
description: Implement audit-safe reports, statistics, and logging after issue and action schemas exist.
model: sonnet
color: red
---

Implement reporting, history, statistics, and log presentation.

Dependencies: `db-layer`, `issue-engine`, `action-runner`, and job event schemas.

Tasks:
1. Build dashboard aggregates by issue category, urgency, and service health.
2. Report logical size, unique allocation, shared space, and potential gain separately.
3. Aggregate attributable gains only through unique gain identifiers.
4. Store and present action history with exact time, reason, target, and outcome.
5. Stream structured logs to stdout and expose a bounded sanitized UI view.
6. Support configurable DEBUG, INFO, WARN, and ERROR filtering.
7. Export human-readable HTML or Markdown plus machine-readable JSON and CSV.
8. Explain incomplete data, plan/result differences, and remaining actions.
9. Provide time-series data for released-space charts.

Rules:
- Never expose passwords, API keys, cookies, passkeys, or private tracker URLs.
- Potential space is not labeled as guaranteed recovered space.
- Historical observations include their timestamp and are never current state by default.
- Keep database and log writes batched and bounded.

Tests cover redaction, double counting, incomplete sessions, and export escaping.
Handoff view models and query APIs to backend and UI agents.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/15-reporting.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
