# ADR-0008: Stalled incomplete cleanup is a manual-only action outside the completed allowlist

Status: accepted (wave 1). Resolves OQ-06 / CON-06. Details: `docs/architecture.md` sections 7.4, 16.

## Context
MASTER §5.1 allows removing a stagnant incomplete torrent task and deleting its data. MASTER §5.7
allows destructive actions only for `progress == 1` and UP states. Taken literally, the two rules
contradict each other.

## Decision
- The allowlist (INV-28) governs destructive actions on **completed** data and is unchanged.
- `CleanupStalledIncomplete` is a separate action type, **manual only**, never planned by auto mode.
- Eligibility: progress < 1; `now - added_on >= incomplete_stalled_days`; state not in
  {`checkingDL`, `checkingUP`, `checkingResumeData`, `moving`, `allocating`} and not unknown; every file
  mapped under `<incomplete_root>`.
- Steps: revalidate, stop, remove the task with `deleteFiles=false`, revalidate the data paths
  (exist, confined, unreferenced), then the application deletes the data under
  `<incomplete_root>`, and finally verify.

## Consequences
- qBittorrent still never deletes data (INV-06).
- If the data deletion fails after the task was removed, the files remain as an
  `incomplete_orphan` issue. This is harmless and visible.
