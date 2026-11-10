# Handoff — 00-legacy-audit (wave 0)

## Delivered scope

- `docs/legacy-parity.md`: the traceable legacy parity matrix (English). It contains:
  - section 1: legacy inventory, i.e. configuration (CFG-01..20), domain rules (DR-01..36),
    workflows (WF-01..13), reports and journal events (RPT-01..09, EVT list), external APIs
    (API-01..12), failure handling and exit codes (FH-01..14), and a legacy symbol -> Rust
    subsystem map;
  - section 2: all 32 legacy tests (T-01..T-32), each with the Rust behavior expected, a destination
    subsystem and a status (keep 26, adapt 4, superseded 1, obsolete 1);
  - section 3: 51 permanent invariants (INV-01..51), each with evidence and a destination,
    10 historical-state items (HIS) and 13 one-off decisions (ONE), each with the lesson kept;
  - section 4: 30 rejected approaches (REJ-01..30). REJ-01..13 map one-to-one to CONTEXTE §2.12;
    MASTER §2 anti-patterns are included;
  - section 5: 32 contradictions, legacy defects or deliberate changes (CON-01..32), each with a
    recommended resolution and an owner;
  - section 6: 26 MASTER requirements with no legacy equivalent (NEW-01..26) and 12 CONTEXTE-only
    requirements (CTX-01..12);
  - section 7: the synthetic parity fixture (37 entries FX-01..37, 13 simulated torrents,
    8 variants V1..V8), the exact inventory and outputs the legacy code would produce on it, and the
    proposed Rust expectations where behavior deliberately differs;
  - section 8: 15 open questions (OQ-01..15), each with a recommendation;
  - Appendix A: CP1 checklists mapping every MASTER §2 item, CONTEXTE §1.3, §2.12, §3.1-3.15 and §9,
    and MASTER §3-§10, to IDs.
- This handoff.

Nothing else was created or modified. `old/` and the application sources were not touched.

## Key contracts and insights for `01-system-arch`

1. **Every ID has an owner.** Each INV/T/CON/NEW/CTX row names one or more of the 17 subsystems.
   Ownership is a proposal: `docs/architecture.md` may reassign rows, but CP1 checks that each one
   still has an owner.
2. **The legacy engine is narrower than the product.** It handles only `.mkv` files in completed
   and media: orphan videos, exact-copy repair, deletions, and qBittorrent stop/recheck/remove.
   It has **no** incomplete tracking, junk detection, permission/owner detection, sample
   protection, state allowlist, ARR REST client or Jellyfin client (NEW-06, NEW-09..15). Those
   requirements come from CONTEXTE and MASTER only, so there is no legacy behavior to preserve
   for them.
3. **Legacy behaviors to drop, not port** (the parity fixture demonstrates each one):
   - samples are proposed for deletion, and auto mode may delete one (CON-01);
   - automatic deletion uses a local resolution/language hierarchy (CON-02, T-19 superseded);
   - unmappable torrents are skipped silently (CON-07);
   - torrents are stopped before any intent is journaled (CON-11);
   - the recheck check is weak (CON-12);
   - the cumulative gain counter double counts (CON-13);
   - a release folder with a `Sample/` subfolder is split into two candidates (CON-23);
   - path confinement uses resolve-then-act, which is open to TOCTOU races (CON-27).
4. **Decisions only the architect can make** (blocking for later waves):
   - OQ-06 / CON-06: stagnant-incomplete removal conflicts with the MASTER §5.7 allowlist.
     MASTER is inconsistent with itself here.
   - OQ-05: how large the fail-closed perimeter is.
   - OQ-07: policy for torrents outside every path mapping.
   - OQ-01: the sample detection rule.
   - OQ-11 / CON-22: folder plans that contain samples.
   - OQ-12 / CON-24: deletions in media roots during ARR replacement.
   - OQ-09: the default for `expected_owner`.
5. **The quality rule is strict.** The only local checks the Rust plan engine may keep are
   blockers: age, identity, edition, language, ffprobe duration and audio. Promoting a replacement
   requires an ARR snapshot (INV-11, INV-34).
6. **The qBittorrent contract to keep exactly:** login accepts 200 `Ok.` and 204 empty, then is
   confirmed through `app/version`; endpoints are `stop`/`start` on 5.x and `pause`/`resume` on 4.x;
   hashes are joined with `|`, sorted and unique; the `Referer`/`Origin` headers are sent; the remove
   call has no delete-files parameter. Torrent files are attached by content path (DR-23), never by
   torrent name.
7. **Fixture portability.** Allocated bytes depend on the filesystem, so expectations are written
   as inode sets. The fixture needs a single filesystem, a non-root user, no-follow mtime stamping
   (symlinks included) and permission restoration before cleanup (§7.1).

## Deviations from the architecture / MASTER

- No architecture exists yet, so there were no deviations to record. The recommendations that
  change legacy behavior are listed as CON entries of type D (defect) or Δ (deliberate MASTER change).
- On business rules where CONTEXTE and MASTER disagree, CONTEXTE was applied (for example CON-02,
  CON-08). No CONTEXTE invariant had to be weakened.
- Historical identities, absolute paths, URLs and user names were left out on purpose and replaced
  by symbols (`<completed_root>`, `<media_root>`, `<incomplete_root>`, `<common_parent>`,
  `<app_data>`, `<undeclared_sibling>`).

## Open TODOs

- `01-system-arch`: resolve OQ-01..15 in `docs/architecture.md`, confirm or reassign owners, and
  decide the final issue-category names used in §7.7.
- `21-unit-tests` (from wave 3): keep the T-xx statuses up to date and add Rust test names next to
  each row.
- `22-integration-tests`: build the §7 fixture and variants V1..V8, and assert §7.5 and §7.6
  parity plus the §7.7 expectations as finalized by the architect.
- The §7.6 values were derived by reading the code, not by running Python (no shell is available
  to this agent, and the legacy script must not be run against real services). It is worth
  re-deriving them once with the legacy module against the built fixture on a local, offline
  loopback fake. This is optional and must be read-only.

## Files read

- `MASTER-PROMPT.md` (all sections, including §10 orchestration)
- `CLAUDE.md`
- `.claude/agents/00-legacy-audit.md`
- `docs/PROGRESS.md`; `docs/handoffs/` contains no dependency handoffs, as expected for wave 0
- `old/CONTEXTE_RACINE_APP_NAS.md` (all 1830 lines)
- `old/doublon_checker.py` (all 3214 lines)
- `old/test_doublon_checker.py` (all 885 lines; 32 `test_*` methods plus the helper
  `scans_real_hardlinks_and_deduplicates_allocated_size`)
- `old/generate_final_audit.py` (all 631 lines)
- `old/rapport-final.md`

## Commands run and results

- No shell commands were run. Only file tools were used: Read, Glob and Grep.
- `Grep "def (test_|scans_)"` on the test file returned 33 matches: 32 tests plus 1 helper.
- `Grep "(?i)sample|vrac|junk|owner|uid|incomplete"` on `doublon_checker.py` confirmed that the
  legacy engine has no sample, junk, owner or incomplete logic. The only matches were content
  sampling, the EPERM message and the scan-incomplete events.
- No network access, no NAS, no live services. No secrets were copied.
