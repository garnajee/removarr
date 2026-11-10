# Handoff — 06-fs-engine (wave 5)

## Delivered scope

`crates/fs` (crate `removarr-fs`) read side, plus `crates/testkit/src/fixture.rs`. No other crate, root manifest,
`Cargo.lock` or domain file was touched by me. `crates/fs/Cargo.toml` is unchanged (it still lists `serde`, `tokio`,
`tracing`, `thiserror`, `async-trait`, which this crate does not use yet; 13 may need some of them).
**Not delivered, by ownership:** `crates/fs/src/mutate.rs` and `FsMutator` belong to 13 (architecture 15.2). Nothing in
this crate can mutate (no `linkat`, `renameat`, `unlinkat`, `fchmodat`, no write opens); the mutation port is therefore
unreachable in dry-run by construction. 13 must add `mod mutate;` to `lib.rs` and build on `handle::*` (below).

| Module | Contents |
|---|---|
| `lib` | `ConfinedFs` (implements `FsInventory` and `ContentReader`), `FsLimits { content_read_concurrency }`, `shared()`, `DirHook`, `QBIT_INTERNAL_DIR`, `MIN_CHUNK` |
| `handle` | confinement primitives, public so 13 reuses them: `open_abs_dir`, `open_rel_dir`, `open_dir_at`, `lstat_at`, `signature(&Stat)`, `allocated_bytes`, `handle_signature`, `port_error`, `confinement_error`, `io_kind` |
| `scan` | iterative DFS walk (private), `list_dir` helper |
| `inventory` | pure accounting: `group_physical`, `PhysicalGroup`, `unique_allocated_bytes`, `logical_bytes`, `shared_between` |
| `perm` | `findings(inv, owner)`, `can_modify_dir(sig, uid, gid)` |
| `content` | `samples_match`, `full_compare` (`ContentReader`) |
| `testkit::fixture` | `ParityFixture` (FX-01..37, V2, V5, V6, V7, V9, `add_qbit_internals`), `bytes`, `synthetic_tree`, `TreeShape` |

## Public contract

```rust
let fs = removarr_fs::ConfinedFs::new(cfg.roots(), FsLimits { content_read_concurrency: cfg.limits.content_read_concurrency });
// Arc<dyn FsInventory> and Arc<dyn ContentReader> from the same Arc<ConfinedFs>; 13 adds FsMutator on the same type.
fs.scan_root(&root_id, &ctl) -> Result<RootInventory, PortError>   // run in spawn_blocking
```
- `scan_root`: sequential DFS, one directory handle per level (held through `Rc`, so fds are bounded by depth, not by width),
  every component `openat(O_NOFOLLOW|O_DIRECTORY)`, every entry `fstatat(AT_SYMLINK_NOFOLLOW)`, entries stat'ed in inode order.
  Metadata only: no file is ever opened (test `inventory_never_opens_file_contents` uses mode-000 files). Symlinks are recorded
  (`symlink_target` via `readlinkat`, display only) and never traversed. Entries exclude the root itself (`root_sig` holds it).
  Output is sorted by `rel`. `allocated` = `st_blocks*512` for regular files only, 0 otherwise; `sig.size` is kept only for regular
  files and symlinks (0 for dirs), identically in scan, `stat`, `snapshot_dir`, so signatures are comparable across calls.
  `sig.mode` is `st_mode & 0o7777`; `mtime_ns` = sec*1e9+nsec.
- Errors per path (`ScanErrorRecord`): `ENOENT` race on stat or open, `EACCES` on open or list, directory replaced between `lstat`
  and open (dev/ino mismatch), mid-listing readdir failure. The walk continues. `RootInventory::has_issue_under(dir)` (domain)
  is the incomplete-subtree propagation (ancestors, equal and descendants). A root that cannot be opened is `Err`, not an inventory.
- Protected zones: `BT_backup` directory and any directory holding a `*.fastresume` file -> `QbitInternal` (names listed only to
  detect it; children are not stat'ed, not inventoried, nothing opened); different `st_dev` below the root -> `MountBoundary`
  (entry recorded, not entered); non-UTF-8 name -> `NonUtf8Name` zone at the **parent directory**, entry skipped.
- Cancellation: `ctl.cancel` checked before each directory; cancelled -> `Ok(inventory)` with `finished = false` (never a mutation
  input, `is_clean()` false). Progress: `ctl.progress` gets `+examined entries` after each directory (memory only).
- `stat(path)`: `Ok(None)` = ENOENT (also below a missing directory); symlink as final component -> `Symlink` signature;
  symlink as an intermediate component -> `Err(Confinement)`; file used as directory -> `Err(Io(NotADirectory))`; unconfigured root ->
  `Err(Confinement)`.
- `snapshot_dir(dir)`: recursive subtree snapshot, keys relative to `dir` (`""` = the directory), as folder removal needs (DR-07).
  Any error, non-UTF-8 name, mount boundary or replaced directory fails the whole call.
- `free_space(root)`: `f_bavail * f_frsize` via `fstatvfs` on the root handle.
- `permission_findings(inv, owner)`: pure over signatures. `OwnerMismatch{uid,gid}` for every regular file, directory and the root
  whose (uid, gid) differs from `expected_owner`; `ParentNotWritable` for every directory (and the root, path `""`) that has at least
  one inventoried child and in which the expected identity cannot create/rename/unlink (first matching permission class, needs `w`
  and `x`; uid 0 always can); `NotWritableByOwner` for a regular file owned by the expected uid without the owner write bit.
  Limits: supplementary groups, ACLs, immutable flags and read-only mounts are invisible to `stat` (documented in `perm.rs`).
  Symlinks are not owner-checked. Nothing is ever chmod'ed or chown'ed.
- Accounting (`inventory`): hardlink proof is `(dev, ino)` only; `nlink` is carried as data. `group_physical(inventories)` groups
  regular files across any number of roots, `unique_allocated_bytes` counts each inode's blocks once, `shared_between(a, b)` returns
  inodes present in both trees.
- `ContentReader`: only ever called on retained candidates, never by the walk. Each file is opened with `O_NOFOLLOW|O_NONBLOCK` after
  an `lstat`, signature checked against `expect` (ignoring `nlink`) before and after (fstat on the open handle).
  `samples_match`: unequal sizes -> false, size 0 -> true, window `min(size, spec.size)`, offsets `round(i*span/(count-1))`
  (legacy DR-12); a signature mismatch -> `Err(StateChanged)`; cancellation -> `Err(Cancelled)`. `full_compare`: chunk
  `max(64 KiB, chunk_bytes)`, `pread`, returns `Identical`, `Differs{offset}` (first differing byte) or `Changed` (pre/post signature
  mismatch, short read); sequential `posix_fadvise` and `DONTNEED` on Linux. Content readers are bounded by `content_read_concurrency`
  (clamped to 1..=4).
- Concurrency: a process-wide gate allows **one walk at a time** (`peak_concurrent_scans()` proves it) and
  `content_read_concurrency` readers (`peak_concurrent_content_reads()`). No timers, no loops, no watchers: scheduling belongs to `jobs`.
- Roots are opened per call by walking `/` -> root with no-follow at every component (so a symlinked ancestor or a root replaced by a
  symlink after startup is refused with `Confinement`). Only ancestors of a configured root and the root are ever opened, never siblings.
- Test seam: `ConfinedFs::with_dir_hook(Arc<dyn Fn(&RelPath)>)` (`doc(hidden)`) runs after a directory is listed and before its
  entries are examined; used to create ENOENT and symlink-swap races deterministically.

## Persistence batching

`scan_root` returns the whole `RootInventory` for one root (about 20k entries is a few MB). Batching is done by storage:
`ScanStore::save_inventory` diffs and writes in 1000-row transactions (04 handoff). The walk itself holds no DB handle. No per-file
progress write exists. If 11 needs bounded memory for far larger trees, a streaming callback API can be added without changing the
port; not needed for the 20k reference fixture.

## Performance measurements (host: macOS arm64 APFS, synthetic tree, 19,787 files + 420 directories, 64-byte files, page cache warm)

| Measure | Debug | Release |
|---|---|---|
| Full walk (`scan_root`) | 74 ms | 59 ms |
| Unchanged rescan | 71 ms | 57 ms |
| 8 concurrent callers (serialized by the gate, 8 full walks) | 635 ms | 518 ms |
| Peak concurrent walks | 1 | 1 |

Cold-cache HDD numbers are not measurable here; the design choices that matter for HDDs are: single walker, inode-ordered stats,
no content opens, fd count bounded by depth. Budget B4 (fs walk <= 15 s for 20k files) has large headroom. `23-e2e-perf` should
re-measure on Linux (ext4/btrfs) with a cold cache. `media-match` note: content reads are the only expensive operation and are
gated by `content_read_concurrency`; use `samples_match` first, `full_compare` only on the retained pair.

## Tests (all in `crates/fs/tests`, temp dirs only)

`scan_parity.rs` (7): §7.5 inventory numbers, T-01 real hardlinks and unique blocks (independent `st_blocks` check), T-02 nlink 2
with external link, unconfigured sibling `outside/` (mode 000, never opened) and unconfigured root refused, `BT_backup` and
`.fastresume` zones, non-UTF-8 (skipped where the filesystem refuses such names, as on APFS), free space.
`confinement.rs` (7): symlinks as entries only, symlinked components, stat and snapshot refusals, root replaced by symlink,
symlinked ancestor, directory swapped for a symlink mid-scan, stat equals scan signature.
`races_and_errors.rs` (4): ENOENT races (file, directory, nested), V2 permission failure (skipped as root), cancellation, progress.
`permissions.rs` (5): writability classes, base fixture (only FX-33's parent flagged), V5 wrong owner, root-owned tree, `0444`.
`content_reads.rs` (7): identical, V7, different sizes, V6 changed, symlink/escape refused, cancellation, concurrency bound.
`large_tree.rs` (3): ~20k-file tree with 8 racing callers (peak 1), never opens contents, rescan identical.
Legacy tests routed to fs-engine: T-01 and T-02 covered here (`t01_real_hardlinks_are_one_physical_identity_and_blocks_count_once`,
`t02_nlink_two_with_a_link_outside_the_trees_is_not_a_shared_inode`). T-14 and T-17 are mutation-side (13); INV-41/FH-13 (ENOENT) and
INV-18 (no symlink) are covered by the race and confinement tests.

## Deviations and notes

1. `snapshot_dir` is recursive (the domain doc says "direct entries"); required by DR-07 and `FsMutator::remove_tree`.
2. `scan_root` returns `Ok` with `finished = false` on cancellation (as the domain type implies), `Err` only when the root cannot be
   opened or is not configured.
3. `ParentNotWritable` is reported only for directories with at least one inventoried child; `NotWritableByOwner` has a narrow
   definition (owner lacks `w` on its own regular file), because the architecture does not define it.
4. `FsEntry`/signature `size` is 0 for directories (single rule everywhere) to avoid diff churn on filesystems where directory size varies.
5. Mount boundary detection compares `st_dev` with the root's; the boundary directory entry is recorded but never entered.
6. `ConfinedFs::new` takes `&removarr_config::Roots`, which is only re-exported at the crate root (`removarr_config::Roots`).

## Open TODOs

- 13: `mutate.rs` (`FsMutator`) reusing `handle::*`; `with_dir_hook`-style seams for race tests are available.
- 11: map `ProtectedZone`/errors to session status per architecture 7.1 step 8; consume `permission_findings` per root with `cfg.owner`.
- Linux-only validation: `fadvise`, `fstatvfs` units, real mount boundary (`MountBoundary` has no automated test; needs a second filesystem).
- Parity fixture variants V3, V4, V8, V10 are not filesystem variants (connector side). V9 is built by `ParityFixture::apply_v9`.

## Commands run and results (host target)

- `cargo fmt -p removarr-fs -p removarr-testkit --check`: pass.
- `cargo clippy -p removarr-fs -p removarr-testkit --all-targets -- -D warnings`: pass.
- `cargo test -p removarr-fs -p removarr-testkit`: pass (fs: 33 integration tests across 6 files, 0 unit tests; testkit 5).
- `cargo test -p removarr-fs --release --test large_tree -- --nocapture`: pass, timings above.
- Workspace: `cargo fmt --all --check` fails in `crates/qbit/src/control.rs` (agent 08 in progress) and `cargo test --workspace`
  fails to compile `removarr-qbit` tests (`FakeQbit::add_extra`, `TorrentHash: FromStr`; agent 08 files). Not in fs or testkit fixture.
