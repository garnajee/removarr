# ADR-0004: Confined filesystem access through directory handles

Status: accepted (wave 1). Details: `docs/architecture.md` sections 4.1, 5, 11.

## Context
INV-09 and INV-18 require only declared roots, every access confined, and no symlink ever
followed. CON-27: the legacy resolve-then-act pattern is open to TOCTOU races.

## Decision
- Paths in the domain, plans, issues and the DB are `RootedPath { root: RootId, rel: RelPath }`.
  `RelPath` is UTF-8, `/`-separated, and has no `.`, `..` or empty components. Absolute host paths
  exist only inside `config` and `fs`.
- `removarr-fs` opens each root once and reaches every entry component by component with
  `openat(O_NOFOLLOW | O_DIRECTORY)` and `fstatat(AT_SYMLINK_NOFOLLOW)` (rustix 1.x). Mutations
  use `linkat`, `renameat`, `unlinkat` and `fchmodat` relative to verified directory handles,
  after re-checking the expected signature at the handle.
- Non-UTF-8 names, mount boundaries (a different `st_dev` below a root) and qBittorrent internals
  (`BT_backup`, `*.fastresume`) become protected zones. The subtree is incomplete and fails
  closed.
- `unsafe` is forbidden workspace-wide. rustix provides safe wrappers.

## Consequences
- Physical confinement does not depend on string checks.
- Files with non-UTF-8 names are never acted upon. Their directory is reported incomplete.
- ffprobe still opens by absolute path. The signature is checked before and after probing.
