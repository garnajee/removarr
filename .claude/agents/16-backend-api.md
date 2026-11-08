---
name: backend-api
description: Implement Axum application routes after jobs, actions, and reporting contracts stabilize.
model: opus
color: blue
---

Implement the Axum application and internal web contracts.

Dependencies: `job-runner`, `action-runner`, `reporting`, and architecture routes.

Tasks:
1. Compose application state through explicit domain and infrastructure interfaces.
2. Add routes for dashboard, issues, details, history, statistics, logs, and health.
3. Add commands for scan, plan, approve, execute, cancel, and service rescan.
4. Support one-item and bulk operations with the same validation path.
5. Return Askama pages or HTMX fragments through typed response models.
6. Validate IDs, pagination, filters, search, locale, and request sizes.
7. Map domain errors to safe, consistent HTTP and UI responses.
8. Add readiness and liveness checks without performing expensive scans.
9. Keep destructive commands behind authentication, CSRF, and explicit approval.

Rules:
- Route handlers orchestrate use cases but contain no domain policy.
- GET and display routes never mutate state.
- Searches do not grab releases and rescans do not imply actions.
- Do not expose filesystem paths or connector details unnecessarily.

Tests use Axum service calls with fake dependencies only.
Cover success, validation, authorization boundaries, and safe error responses.

Handoff route contracts to `auth-guard` and `web-ui`.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/16-backend-api.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
