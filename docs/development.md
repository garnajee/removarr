# Development guide

## Prerequisites

- A Rust toolchain with `cargo`, `rustc` (1.88 or newer; the project is built with 1.98), `rustfmt` and
  `clippy`. There is no `rust-toolchain.toml` and no rustup requirement.
- Local builds and tests use the **host target only**. Static musl and multi-arch builds happen in the
  Docker builder (ADR-0009), never locally.
- No service credentials are needed. Tests use fakes from `removarr-testkit` and temporary directories.
  Never put real endpoints, storage paths, passwords or API keys in the repository.

## Bootstrap

```sh
cargo build --workspace      # first build downloads crates from crates.io
cargo test --workspace
cargo run -p removarr        # currently a stub that exits immediately
```

`Cargo.lock` is committed. Do not edit it by hand.

## Quality gate

All four commands must pass without warnings:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

Configuration: `rustfmt.toml` (edition 2024, width 100), `clippy.toml` (unwrap/expect allowed in tests
only), and `[workspace.lints]` in the root `Cargo.toml` (`unsafe_code = "forbid"`, plus clippy
`unwrap_used`, `expect_used`, `dbg_macro`, `print_stdout`, `todo`). Every crate opts in with
`[lints] workspace = true`.

## Dependency direction gates

Each command must print nothing (architecture section 2.2):

```sh
cargo tree -p removarr-domain -e normal --prefix none | grep -E '^(tokio|sqlx|reqwest|axum|askama|rustix|hyper) '
grep -rnE 'std::(fs|process|net)' crates/domain/src
cargo tree -p removarr-workflows -e normal --prefix none | grep -E '^removarr-(storage|fs|qbit|arr|jellyfin|config) '
cargo tree -p removarr-web -e normal --depth 1 --prefix none | grep -E '^removarr-(storage|fs|qbit|arr|jellyfin) '
```

## Workspace rules

- Third-party versions are declared once in the root `[workspace.dependencies]`. Crates use
  `{ workspace = true }`.
- Crate ownership and the files each agent may edit: `docs/architecture.md` section 15.
- `removarr-testkit` is a dev-dependency only. `removarr-config` has an empty `test-support` feature
  reserved for test-only constructors.
