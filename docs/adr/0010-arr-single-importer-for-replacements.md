# ADR-0010: ARR is the single importer for release replacements and approved grabs

Status: accepted (wave 1), amended after CP1 iteration 1 (CP1-F01, CP1-F06).
Details: `docs/architecture.md` sections 5, 7.2, 7.4, 13.

## Context
CONTEXTE §3.6 and §3.9 and INV-11, INV-16, INV-35 require replacing a media version only on ARR
quality evidence, never with two importers, and removing the old version only after inode proof
of the new one. With `importMode` `move` (or `auto` without a tracked download), Sonarr/Radarr
**move** the completed file into the library. That takes seeding data out of `<completed_root>`
without the application's deletion controls.

## Decision
- ARR's decision on local files comes from `GET manualimport` (preview, read-only). Its
  accept/reject reasons and per-file `episode_ids`, together with the profile, Custom Format
  scores, current files and the expected episode list (`ArrRead::episodes`), form the
  `ArrQualitySnapshot` and the `CoverageProof`.
- The import is performed by ARR through the separately authorized `arr_manual_import` capability
  (default off). `ManualImportRequest` has **no mode field**. The adapter always sends
  `importMode = "copy"` as a literal (ARR hardlinks when "Use Hardlinks instead of Copy" is on).
  A gate grep and the ARR fake enforce it.
- The application never creates hardlinks in media for a *different* version. It creates them only
  for exact-copy consolidation.
- Verification requires all three of the following:
  - ARR's current file is the new path;
  - the **completed source path still exists with its recorded `(dev, ino)`**;
  - the media path shares that `(dev, ino)`.

  Outcomes:
  - A vanished or changed source means `FailedNeedsReview` (`ArrImportMoved`), reported Critical.
  - A different media inode (ARR copied) means `ExecutedUnverified`, the new bytes are recorded as
    `AddedCost`, and old-file deletion stays blocked.
- Old media files that ARR did not remove are deleted only by an explicit, manually approved step
  after that proof. Never automatic.
- Grab: `ArrGrabRelease` from a listed interactive search result. It needs the `arr_grab`
  capability (default off), manual approval only, never auto, live only and journaled. ARR then
  downloads and imports, so it remains the single importer. A search never grabs.

## Consequences
- The ARR database stays consistent, and the single-importer rule holds by construction.
- A move by ARR cannot pass as success.
- Replacement and grab require ARR to be configured. Without hardlinks enabled in ARR, a
  replacement can never be `Verified`.
