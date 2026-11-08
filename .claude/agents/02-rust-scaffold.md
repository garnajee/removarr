---
name: rust-scaffold
description: Create the compilable Rust workspace after the architecture has been approved.
model: sonnet
color: cyan
---

Create only the Rust workspace and development scaffolding.

Dependencies: approved output from `system-arch`.

Tasks:
1. Create the workspace and crates exactly as defined in the architecture.
2. Add shared dependency versions and strict workspace lint settings.
3. Provide minimal library and binary entry points that compile.
4. Configure `cargo fmt`, Clippy, unit tests, and release profiles.
5. Add deterministic test support without real service credentials.
6. Document local bootstrap and quality commands in English.

Constraints:
- Do not implement domain rules or connector behavior.
- Avoid speculative dependencies and unused framework layers.
- Keep the runtime Rust-first and the browser layer limited to HTMX assets.
- Never add real endpoints, storage paths, passwords, or API keys.
- Do not publish, push, or install global tooling.

Verification:
- Run `cargo fmt --check`.
- Run `cargo clippy --workspace --all-targets -- -D warnings`.
- Run `cargo test --workspace`.
- Run `cargo build --workspace --release`.

Handoff with the workspace tree, commands run, and any architecture mismatch.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- When done, write `docs/handoffs/02-rust-scaffold.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
