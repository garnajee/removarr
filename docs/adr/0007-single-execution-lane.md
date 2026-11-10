# ADR-0007: Single execution lane

Status: accepted (wave 1). Details: `docs/architecture.md` section 8.

## Context
INV-16 requires a single importer per media. The jobs agent must serialize conflicting work
by media, path scope and action type. HDDs suffer under concurrent heavy I/O.

## Decision
- Exactly two workers: one Observe lane (scan, refresh, evaluate, verify-content) and one Execute
  lane (recover, execute-plan, auto-cycle). All mutations are globally serialized.
- Root `RwLock`s: observe jobs take shared locks, execution takes exclusive locks on the plan's roots.
- The external importer (Sonarr/Radarr) is checked during revalidation through the ARR queue.
  Configured but unreachable ARR blocks the action.

## Consequences
- No per-media lock table is needed: the single lane is a stronger guarantee.
- Plans execute one after another. This is acceptable for a single-user tool.
- Raising execution concurrency later requires adding per-media and per-hash locks first.
