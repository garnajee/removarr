# ADR-0010: ARR is the single importer for release replacements

Status: accepted (wave 1). Details: `docs/architecture.md` sections 7.2, 7.4, 13.

## Context
CONTEXTE §3.6 and §3.9 and INV-11, INV-16, INV-35 require replacing a media version only on ARR
quality evidence, never with two importers, and removing the old version only after inode proof
of the new one.

## Decision
- ARR's decision on local files comes from `GET manualimport` (preview, read-only). Its
  accept/reject reasons, together with the profile, Custom Format scores and current files, form
  the `ArrQualitySnapshot` (fingerprinted).
- The import itself is performed by ARR through the separately authorized `arr_manual_import`
  capability (default off). The application never creates hardlinks in media for a
  *different* version. It creates them only for exact-copy consolidation.
- Verification = ARR's current file is the new path **and** the media path shares `(dev, ino)`
  with the completed candidate. Old media files that ARR did not remove are deleted only by an
  explicit, manually approved step after that proof. Never automatic.
- ARR grab is not implemented in V1. Search results are display-only.

## Consequences
- The ARR database stays consistent, and the single-importer rule holds by construction.
- Replacement requires ARR to be configured with hardlinks enabled. If ARR copies instead, the
  added space is reported as `AddedCost` in the net gain.
