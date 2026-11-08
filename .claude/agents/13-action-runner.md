---
name: action-runner
description: Execute approved plans transactionally after planning and connector work is complete.
model: fable
color: yellow
---

Implement the safety-critical action executor.

Dependencies: approved `plan-engine`, repositories, matching, and all connectors.

Tasks:
1. Run dry-run through every validation and orchestration step except mutations.
2. Revalidate paths, types, symlinks, device, inode, size, time, mode, and services.
3. Persist and fsync action intent before the first mutation.
4. Coordinate qBittorrent stop, mutation, recheck, removal, and state restoration.
5. Consolidate exact copies using permission probe, full compare, temporary hardlink, and atomic replace.
6. Execute approved removals only inside configured roots and never through symlinks.
7. Support verified release replacement only after complete coverage is imported.
8. Implement targeted permission or junk actions only when explicitly planned.
9. Persist results, compensation, verification state, and attributable gain.
10. Recover unfinished intentions safely after process restart.

Invariants:
- Never call qBittorrent with `deleteFiles=true`.
- Uncertain or partially modified torrents remain stopped and visible.
- Failed verification cannot be reported as success.
- Interruption resumes only torrents whose data was not changed.
- No shell command may be assembled by concatenating paths.
- Never touch live storage or services in development or tests.

Test every crash boundary and compensation path with fakes and temporary files.
Handoff action events and progress interfaces to jobs, web, and reporting.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/13-action-runner.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
