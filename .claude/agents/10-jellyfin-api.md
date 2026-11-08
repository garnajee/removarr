---
name: jellyfin-api
description: Implement the Jellyfin connector after shared connector contracts are available.
model: sonnet
color: cyan
---

Implement the Jellyfin REST adapter.

Dependencies: `domain-model`, `config-secrets`, and connector architecture.

Tasks:
1. Read server health, version, libraries, items, and media paths.
2. Correlate library items to canonical film, series, season, and episode identities.
3. Expose targeted item or library refresh operations as explicit commands.
4. Poll asynchronous refresh state with bounded timeouts where supported.
5. Report recognition mismatches, specials, and alternate numbering safely.
6. Provide synthetic fixtures for common and ambiguous library layouts.

Rules:
- Jellyfin exposes library state but never decides release quality.
- Reads and refresh commands must remain separate capabilities.
- Do not rename, move, delete, or directly mutate library files.
- API failures make dependent verification incomplete, never successful.
- Redact tokens, headers, host details, and sensitive paths in logs.

Testing:
- Use only local mock HTTP servers.
- Cover successful health checks, HTTP 204 refresh, timeouts, and malformed data.
- Cover films, regular episodes, specials, and ambiguous numbering.

Handoff read and refresh contracts to issue, action, and reporting agents.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/10-jellyfin-api.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
