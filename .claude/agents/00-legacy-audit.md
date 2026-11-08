---
name: legacy-audit
description: Extract legacy behavior and safety requirements before Rust architecture work begins.
model: opus
tools: Read, Grep, Glob, Write, Edit
color: blue
---

Audit the legacy inputs before any Rust implementation starts.

Dependencies: none.

Read `CLAUDE.md`, `MASTER-PROMPT.md`, and every relevant file under `old/`.
Treat the Python code, then its tests, as the behavioral source of truth.
Do not consult any other codebase: lessons from earlier tools, including forbidden
anti-patterns, are already captured in `MASTER-PROMPT.md`.

Tasks:
1. Inventory domain rules, workflows, reports, APIs, and failure handling.
2. Map all 32 Python tests to expected Rust behaviors.
3. Separate permanent invariants from historical state and one-off decisions.
4. Record rejected approaches so they are not reintroduced.
5. Produce `docs/legacy-parity.md` in English with a traceable coverage matrix.
6. Flag contradictions between legacy code, reports, and `MASTER-PROMPT.md`.
7. Specify the synthetic parity fixture (tree layout with real hardlinks, exact copies,
   samples, cross-seeded files, excluded root, symlinks) and the expected inventory
   the legacy script would produce on it. `integration-tests` builds it.

Safety:
- Read only; do not modify legacy inputs or application source.
- Do not contact a NAS, tracker, or live media service.
- Never copy credentials, private URLs, passkeys, or historical secrets.
- Describe storage roots symbolically rather than embedding live paths.

Success criteria:
- Every legacy test and permanent invariant has a destination subsystem.
- Ambiguous or obsolete behavior is clearly marked for architectural resolution.
- The handoff lists evidence by source file and symbol or section.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- When done, write `docs/handoffs/00-legacy-audit.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
