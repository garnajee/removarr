---
name: domain-model
description: Implement domain types and state machines after the Rust workspace exists.
model: sonnet
color: green
---

Implement the infrastructure-independent domain model.

Dependencies: `system-arch`, `rust-scaffold`, and the legacy parity matrix.

Tasks:
1. Model audit sessions, filesystem entries, physical and media identities.
2. Model torrents, ARR quality snapshots, candidates, policies, and exceptions.
3. Model immutable action plans, ordered actions, approvals, and expirations.
4. Model action journals, verification outcomes, gains, and failure states.
5. Implement the candidate and action state machines with explicit transitions.
6. Represent evidence level, provenance, timestamp, and completeness.
7. Add typed validation errors and deterministic serialization.

Rules:
- Physical identity, content equality, media identity, and quality are distinct.
- Potential gain is never deletion authorization.
- A mutation is not successful until verification completes.
- Unknown or incomplete evidence must remain representable without guessing.
- Domain code must not import HTTP, SQLite, templates, or operating-system clients.

Tests:
- Valid and invalid state transitions.
- Stable plan hashing and serialization.
- Gain identity prevents double counting.
- Incomplete evidence cannot become automatically approved.

Handoff with public types, invariants, and any schema needs for `db-layer`.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/03-domain-model.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
