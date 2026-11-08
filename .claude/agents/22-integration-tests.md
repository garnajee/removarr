---
name: integration-tests
description: Verify cross-component workflows after persistence, connectors, and execution are complete.
model: opus
color: pink
---

Own integration tests across real local components and fake external services.

Dependencies: persistence, scanners, connectors, plans, actions, jobs, and backend.

Tasks:
0. Build the synthetic parity fixture specified by `legacy-audit` and assert the Rust
   inventory matches the expected legacy inventory on it.
1. Test migrations and repositories against temporary SQLite databases.
2. Run filesystem workflows in isolated temporary trees with real hardlinks.
3. Use local mock servers for qBittorrent, Sonarr, Radarr, and Jellyfin.
4. Verify dry-run performs no filesystem or service mutation.
5. Verify hardlink repair direction, atomic replacement, and inode result.
6. Verify stop, recheck, timeout, safe removal, and state restoration sequences.
7. Inject crashes after intent and mutation boundaries, then verify recovery.
8. Test stale plans from filesystem, mapping, service, and ARR profile changes.
9. Test job locking, cancellation, restart, and conflicting media imports.
10. Test backend routes with real repositories and fake connectors.

Rules:
- Bind mock servers to isolated loopback only.
- Use dummy credentials and never inherit ambient service configuration.
- No live roots, services, downloads, or external network access.
- Assert persisted journal and final verification state, not only HTTP status.

Run serially only where the test documents a real shared-resource constraint.
Handoff failing scenarios and reproducible commands to owning agents.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- When done, write `docs/handoffs/22-integration-tests.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
