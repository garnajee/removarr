---
name: job-runner
description: Implement persistent scheduling and background work after action interfaces stabilize.
model: sonnet
color: pink
---

Implement the persistent scheduler and worker runtime.

Dependencies: `db-layer`, `issue-engine`, `action-runner`, and connector interfaces.

Tasks:
1. Schedule configurable scans without aggressive polling.
2. Queue manual rescans and approved action jobs persistently.
3. Restore queued and interrupted jobs after restart using explicit states.
4. Limit concurrent scans, comparisons, probes, API calls, and mutations.
5. Serialize conflicting jobs by media identity, path scope, and action type.
6. Support cooperative cancellation and safe cancellation checkpoints.
7. Publish progress, current stage, warnings, logs, and terminal outcomes.
8. Apply backoff with jitter to transient API failures without infinite retries.
9. Prevent two importers from owning the same media transaction.

Rules:
- Scheduling never grants action approval.
- Automatic mode stops on incomplete scans, mappings, permissions, or APIs.
- Cancellation during mutation delegates compensation to `action-runner`.
- Do not wake disks merely to refresh UI status.

Tests:
- Restart recovery, lock contention, cancellation, backoff, and concurrency limits.
- Manual and scheduled jobs cannot duplicate the same work.

Handoff stable job commands and event streams to `backend-api`.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/14-job-runner.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
