---
name: arr-api
description: Implement Sonarr and Radarr quality-authority connectors after domain contracts exist.
model: opus
color: purple
---

Implement Sonarr and Radarr REST connectors behind shared domain interfaces.

Dependencies: `domain-model`, `config-secrets`, and connector architecture.

Tasks:
1. Read series, movies, episodes, files, monitoring, queues, and history.
2. Read quality profiles, upgrade settings, cutoffs, Custom Formats, and scores.
3. Correlate download hashes and paths to canonical media identities.
4. Run interactive release searches without grabbing any result.
5. Capture accept/reject decisions, reasons, qualities, scores, and seed metadata.
6. Expose targeted rescan and command-status operations separately from searches.
7. Snapshot all quality evidence used by an action plan.
8. Detect profile or score changes that invalidate an existing plan.
9. Model manual import and queue removal as separately authorized capabilities.

Rules:
- Never implement a local codec, group, or resolution quality hierarchy.
- No ARR data or API outage may silently become automatic approval.
- A search must never trigger a grab.
- Queue warnings do not prove that a hardlink is absent.
- Redact API keys and sensitive URLs from every error and log.

Use mock servers and recorded synthetic fixtures only.
Test queue ambiguity, score changes, rejected releases, and search-without-grab.

Handoff stable quality snapshot and correlation APIs to `issue-engine`.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/09-arr-api.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
