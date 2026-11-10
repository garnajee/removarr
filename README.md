# removarr-v2

Self-hosted, low-footprint web app (Rust, Axum, Askama, HTMX, SQLite) that audits the storage shared by
qBittorrent, Radarr, Sonarr and Jellyfin: stagnant incomplete downloads, missing hardlinks, exact duplicates,
orphans, permission problems and junk files. It proposes fixes as immutable plans and applies them only after
explicit approval, with a dry-run mode enabled by default.

**Status:** pre-implementation. The repository currently holds the specification and the agent definitions
used to build the application.

## Repository layout

| Path | Content |
|---|---|
| `MASTER-PROMPT.md` | Master specification and the ordered, autonomous build plan for the agents |
| `.claude/agents/` | The 28 agent definitions (`00` to `27`) |
| `old/` | Legacy Python reference: business rules, tests and the context document |
| `docs/PROGRESS.md` | Build progress tracker, used to resume an interrupted build |
| `docs/handoffs/` | One handoff per agent, written during the build |
| `docs/reviews/` | Independent checkpoint reviews, written during the build |

## Building it

Open the repository in Claude Code and ask: "Read `MASTER-PROMPT.md` and execute it". The orchestrator runs
the agents wave by wave, commits locally on an `autonomous-build` branch, and never pushes or publishes.

## Safety principles

- Hardlinks are proven by identical `(device, inode)`, never by link count or file name.
- qBittorrent tasks are removed with `deleteFiles=false` only.
- Sonarr and Radarr are the only authority on release quality.
- Only explicitly configured paths are ever scanned; every ambiguity fails closed.

## License

To be defined.
