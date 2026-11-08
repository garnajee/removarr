---
name: fs-engine
description: Implement safe low-I/O filesystem scanning after domain and configuration foundations.
model: sonnet
color: pink
---

Implement the read-only filesystem inventory engine.

Dependencies: `domain-model`, `config-secrets`, and `db-layer` contracts.

Tasks:
1. Recursively scan configured roots with efficient directory iteration.
2. Never follow symlinks and reject symlinked path components.
3. Capture path, kind, logical size, blocks, device, inode, links, times, mode, UID, and GID.
4. Group physical identities by `(device, inode)` and count allocated blocks once.
5. Record errors per path and mark affected subtrees incomplete.
6. Enforce exclusions before opening a directory, lexically and physically.
7. Detect ownership and permission anomalies without changing them: owner or group
   different from the configured `expected_owner` (for example `1026:100` instead of
   `1030:100`, or `root`), and parent directories the expected identity cannot write,
   rename, or unlink in.
8. Batch persistence and expose cancellation and progress checkpoints.

Performance rules:
- No content hashing or full-file reads during inventory.
- Limit blocking work and concurrency to avoid HDD seek storms.
- Do not poll continuously; scheduling belongs to `job-runner`.

Tests:
- Real hardlinks, external links, symlinks, races, permission failures, and exclusions.
- Unique block accounting and incomplete subtree propagation.
- Synthetic large trees with bounded concurrency.

Never mutate files, permissions, ownership, or external services.
Handoff scanner APIs and performance measurements to `media-match`.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/06-fs-engine.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
