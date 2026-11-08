---
name: qbit-api
description: Implement the qBittorrent connector after configuration and domain contracts are stable.
model: sonnet
color: blue
---

Implement a version-aware qBittorrent Web API adapter.

Dependencies: `domain-model`, `config-secrets`, and connector interfaces.

Tasks:
1. Implement login, version discovery, torrent listing, and file listing.
2. Accept documented empty and HTTP 204 authentication success responses.
3. Support pause/resume semantics for 4.x and stop/start semantics for 5.x.
4. Map torrent paths through validated configured mappings. Match torrents to disk
   content by content path (`save_path` + `content_path`), never by torrent name.
5. Index host paths to all associated hashes and preserve cross-seed relations.
6. Implement recheck, polling with timeout, resume, and safe task removal.
7. Always send `deleteFiles=false`; expose no API that permits true.
8. Preserve initial state and report torrents intentionally left stopped.
9. Classify states with a fail-closed allowlist: a torrent is eligible for destructive
   consideration only when `progress == 1` and its state is one of `uploading`,
   `stalledUP`, `queuedUP`, `forcedUP`, `pausedUP` (4.x) or `stoppedUP` (5.x).
   Every other or unknown state is protected.

Rules:
- Mapping or authentication uncertainty blocks mutation capability.
- A partially affected torrent stays stopped pending manual review.
- Remove a task only when every wanted file is absent and the plan authorizes it.
- Treat `stalledUP`, `stoppedUP`, and `missingFiles` as distinct states.

Use only local mock HTTP servers in tests.
Cover 4.x, 5.x, 204 login, mapping escape, timeout, interruption, and safe deletion.

Handoff adapter capabilities and compatibility assumptions to workflow agents.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/08-qbit-api.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
