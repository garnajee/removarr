# ADR-0001: Cargo workspace, ports in the domain, dependency direction

Status: accepted (wave 1). Details: `docs/architecture.md` section 2.

## Context
CONTEXTE §6.1 requires an interface-independent domain. MASTER §10 runs agents in parallel and
requires disjoint ownership. CONTEXTE §9.5 requires that dry-run cannot reach mutation adapters.

## Decision
- One workspace with `members = ["crates/*"]`. The crates are `domain`, `config`, `storage`,
  `fs`, `qbit`, `arr`, `jellyfin`, `matching`, `workflows`, `jobs`, `reporting`, `web`, `app`, plus the
  test-only `testkit`, `integration-tests` and `e2e-perf`.
- Ports (traits) live in `removarr-domain`. Adapters implement them. Use cases (`workflows`,
  `jobs`) consume `Arc<dyn Port>`. Only `app` constructs adapters.
- `removarr-domain` has no tokio, sqlx, reqwest, axum, askama or rustix dependency, and no
  `std::fs`/`process`/`net`. `workflows` depends on no adapter crate and not on `config`.
  `reporting` is the only read-side consumer of `storage`.
- One connector crate per external service, because they are built in parallel in wave 5.

## Consequences
- Fakes for every port make the workflows testable without I/O.
- Boundaries are checked by `cargo tree`/`grep` gates (architecture section 2.2).
- Adding a crate never edits the root `Cargo.toml`.
- Moving a port later is costly. Ports are therefore defined completely in wave 3.
