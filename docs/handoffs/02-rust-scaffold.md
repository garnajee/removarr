# Handoff — 02-rust-scaffold (wave 2)

## Delivered scope

Cargo workspace per architecture section 2.1 and 3. No domain rules or connector behavior.

```
Cargo.toml  Cargo.lock  rustfmt.toml  clippy.toml
crates/
  domain/     removarr-domain      src/lib.rs
  config/     removarr-config      src/lib.rs   (feature `test-support = []`)
  storage/    removarr-storage     src/lib.rs
  fs/         removarr-fs          src/lib.rs
  qbit/       removarr-qbit        src/lib.rs
  arr/        removarr-arr         src/lib.rs
  jellyfin/   removarr-jellyfin    src/lib.rs
  matching/   removarr-matching    src/lib.rs
  workflows/  removarr-workflows   src/lib.rs
  jobs/       removarr-jobs        src/lib.rs
  reporting/  removarr-reporting   src/lib.rs
  web/        removarr-web         src/lib.rs
  app/        removarr (bin `removarr`)  src/main.rs, src/lib.rs
  testkit/    removarr-testkit     src/lib.rs + clock, ids, fixture, fake_qbit, fake_arr, fake_jellyfin (placeholder one-line files)
docs/development.md
```
`removarr-integration-tests` and `removarr-e2e-perf` are not created (owned by 22/23). README.md has a
short "Development" section.

## Public contracts

- Root `Cargo.toml`: `members = ["crates/*"]`, resolver 3, edition 2024, rust-version 1.88,
  `publish = false`, every section-3 dependency (with features) plus all internal crates in
  `[workspace.dependencies]`; `[workspace.lints]` and release/dev profiles exactly as section 3.
  Each crate has `[lints] workspace = true`. Version `0.1.0`; license unset.
- Crate `Cargo.toml` deps: allowed workspace crates plus the third-party deps of the "Used by"
  column, all `workspace = true` (tokio `test-util` added per crate as a dev-dep feature). `removarr-testkit`
  is a dev-dep of config, storage, fs, qbit, arr, jellyfin, workflows, jobs, web. `uuid` `v7` is enabled in `app` only.
- testkit modules are declared in `lib.rs` as required; wave-5 agents must not edit `lib.rs` or `Cargo.toml`.
  `lib.rs` has one offline smoke test (tempdir).
- No `rust-toolchain.toml`. No endpoints, paths or secrets anywhere.

## Resolved versions (Cargo.lock)

tokio 1.54.1, tokio-util 0.7.20, axum 0.8.9, axum-extra 0.10.3, tower 0.5.3, tower-http 0.6.11,
askama 0.14.0, sqlx 0.8.6, reqwest 0.12.28, url 2.5.8, serde 1.0.229, serde_json 1.0.154,
toml 0.8.23, thiserror 2.0.21, tracing 0.1.44, tracing-subscriber 0.3.23, async-trait 0.1.92,
uuid 1.26.1, sha2 0.10.9, rustix 1.1.6, argon2 0.5.3, getrandom 0.3.4 (0.2/0.4 are transitive),
ipnet 2.12.2, regex 1.13.1, unicode-normalization 0.1.26, time 0.3.55, http-body-util 0.1.5,
tempfile 3.27.0, proptest 1.12.0.

## Deviations from the architecture

None: every requirement resolved within its stated caret range, and no feature was dropped. Notes:
- The architecture says testkit modules are "empty files"; each holds a one-line `//!` placeholder comment (equivalent).
- Crate-level third-party deps follow the "Used by" column, which means some crates declare deps they
  do not yet use (intentional, so later agents need not edit root or crate manifests).

## Open TODOs

- Release profile `lto = "thin"` makes a cold release build slow (about 2-3 minutes).
- Verify later (devops) that static musl builds work with `sqlx` bundled sqlite and `reqwest` rustls.

## Commands run and results (host target only, rustc/cargo 1.98.0)

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass, no warnings.
- `cargo test --workspace`: pass (29 result lines ok; 1 real test, in testkit).
- `cargo build --workspace --release`: pass; `target/release/removarr` produced.
- Dependency gates (architecture 2.2: domain, workflows, web greps) print nothing; no openssl/native-tls in the tree.
