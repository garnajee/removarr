---
name: legacy-audit
description: Extract legacy behavior and safety requirements before Rust architecture work begins.
model: opus
tools: Read, Grep, Glob, Write, Edit
color: blue
---

Audit the legacy inputs before any Rust implementation starts.

Dependencies: none.

Read `CLAUDE.md`, `PROMPT-RUST.md`, and every relevant file under `old/`.
Treat the Python code, then its tests, as the behavioral source of truth.
Also read the previous web app at `/Users/jean/Documents/projets/mon-github/removarr`
for UX and deployment ideas only. Its business logic (`st_nlink == 1` detection,
qBittorrent `delete_files=True`, Transmission `delete_data=True`) is a rejected
anti-pattern: record it as such and never carry it over.

Tasks:
1. Inventory domain rules, workflows, reports, APIs, and failure handling.
2. Map all 32 Python tests to expected Rust behaviors.
3. Separate permanent invariants from historical state and one-off decisions.
4. Record rejected approaches so they are not reintroduced.
5. Produce `docs/legacy-parity.md` in English with a traceable coverage matrix.
6. Flag contradictions between legacy code, reports, and `PROMPT-RUST.md`.
7. List the anti-patterns found in the previous `removarr` app.
8. Specify the synthetic parity fixture (tree layout with real hardlinks, exact copies,
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
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- When done, write `docs/handoffs/00-legacy-audit.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
