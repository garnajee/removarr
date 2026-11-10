# CP1 review — architecture checkpoint (iteration 2)

Reviewer: independent `general-purpose` agent. I wrote none of the deliverables under review.
Scope of this iteration:
- the revised `docs/architecture.md` (commit `a184135`; I read §4-§20 in full and checked §1-§3 for changes);
- ADR-0003, ADR-0006 and ADR-0010 (amended);
- `docs/handoffs/01-system-arch.md` ("CP1 iteration 1 changes");
- the CP1 criteria again, using the inputs of iteration 1: `MASTER-PROMPT.md`, CONTEXTE, `legacy-parity.md` and the agent files.

Verdict: PASS

All 19 iteration-1 findings are fixed in the body of the documents, not only listed in §20. One residue
remains, in F19 (minor). No blocking finding remains. The changes introduce **one new major finding (CP1-F20)**
and a few minor ones. One major is within the pass rule ("more than a handful" fails).

Condition attached to this PASS: CP1-F20 needs one ARR read in a domain port. The domain freezes after
wave 3, so F20 must be patched in `docs/architecture.md` (or through the §15.1 rule-4 targeted 03 patch)
**before wave 3 starts**. CP2-tier4 must verify it.

---

## Open findings

| ID | Severity | Crit. | Evidence | Problem | Required change |
|---|---|---|---|---|---|
| CP1-F20 | major | 1, 3, 5 | `architecture.md` §7.4 ReplaceRelease steps 2-4 ("Old media files that ARR did not remove"); ADR-0010; §17 C20; CONTEXTE §3.6 ("retirer l'ancienne version seulement après cette preuve"), §4.11 Handmaid's Tale; INV-35 | Delegating the import to ARR changes the order of operations. When ARR imports a file over an existing one, its upgrade path removes the **old** media file (recycle bin, or permanent deletion) and only then copies or hardlinks the new one. This happens inside `manual_import`, so **before** the app's inode proof of step 3. A failed or partial import (one episode of a season) can therefore lose the old coverage first. That is the opposite of CONTEXTE §3.6 and INV-35 ("global transaction; each episode proven before removing the old coverage"). C20 says "No CONTEXTE invariant is weakened", which is not accurate. | Register the conflict in §17 with a resolution where CONTEXTE wins. Minimal fix: precondition `OldCoverageRecoverable` on `ReplaceRelease`. For every old media file, either (a) its inode has another retained link in the latest scan (for example the old completed release, still seeding), or (b) the ARR recycle bin is configured. For (b), add an ARR read port, for example `ArrRead::media_management() -> { recycle_bin: Option<MappedPath>, hardlinks_enabled: bool }`, plus a snapshot field. Otherwise the plan gets a new blocker `OldCoverageUnrecoverable`. Verification (step 3) also reports where each removed old file went. Use `hardlinks_enabled` to warn before planning when step 3(c) can never hold. Add the case to V10 and to the tier-4 tests. |
| CP1-F19 | minor (residue) | 4 | §16 OQ-05 "**Auto**: any scan error **or protected zone** in a root blocks every automatic action" vs §7.1 step 8 and §13.3 A2 (`QbitInternal` does not count) | The single rule is stated in §7.1 and A2, but OQ-05 still contradicts it. | Edit OQ-05 so it excludes `QbitInternal` (units containing a zone are still blocked). |
| CP1-F21 | minor | 4, 5 | §7.1 step 3 ("every entry under its content directory (from both `save_path` and `download_path`)") | For a **single-file** torrent, `content_path` is the file itself, so "content directory" is undefined. If it is read as `save_path`, every entry of `<completed_root>` becomes torrent-owned while such a torrent downloads. That fails safe, but it hides every orphan or junk issue. | Define it. Content directory = `content_path` for multi-file torrents. For single-file torrents only the file candidates (with the `.!qB` variant) and `.parts` files in that directory count as owned. |
| CP1-F22 | minor | 4 | §6.5/§6.8 (guard tests cover dry-run processes only); `POST /plans/{id}/simulate` | The new "simulate any plan in a live process" path (ADR-0003 amendment) has no guard test. A wiring bug could hand the live set to a simulation. | Add guard test (e): in a live process with counting fakes, `ExecutePlan { simulate: true }` performs zero fs mutations and zero qBittorrent, ARR or Jellyfin mutator calls, and leaves `PlanStatus` unchanged. Owners: 13 and 22. |
| CP1-F23 | minor | 1 (MASTER §3 low I/O) | §7.6 step 2; §8.1 `AutoCycle` "plus one inline rescan" | The auto cycle always runs a second full filesystem scan after phase 1, including in dry-run and when phase 1 mutated nothing. That doubles the nightly HDD metadata walk for no information. | Run the rescan only on the roots touched by at least one **live** mutation in phase 1. Otherwise phase 2 reuses the session. |
| CP1-F24 | minor | 4 | §11: the `download_path` paragraph is inserted between table rows (after `services.qbittorrent.external_prefixes`) | It breaks the Markdown table. The rows after it (ARR and Jellyfin URLs, `mode.dry_run`, `mode.auto_mode`, `auth.*`) no longer render as part of the configuration contract. | Move the paragraph below the table. |
| CP1-F25 | minor | 4 | §15.2 row 19 ("`crates/web/static/**` (including … `web::assets`)"; "`crates/web/templates/**` (login page markup and styling only, …)"); §12.1 `/plans/{id}/approve` | Small contract inconsistencies. (1) `web::assets` is `crates/web/src/assets.rs`, which is not under `static/**`, so no row owns that path. (2) Row 19's parenthesis can be read as limiting 19 to the login template. (3) The approve route omits `confirmed_torrent_removals`, which §7.3 requires. | List `crates/web/src/assets.rs` under 19. Reword row 19 to "all templates; for `pages/login.html`, markup and styling only". Add `confirmed_torrent_removals` to the approve route. |

Count: 0 blocking, 1 major (F20), 6 minor (F19 residue, F21-F25).

---

## Status of the iteration-1 findings

| ID | Sev. (it. 1) | Status | Evidence in the revised documents |
|---|---|---|---|
| F01 | blocking | **Fixed** | §5 `ManualImportRequest { files }` has no mode field, and the adapter sends the `importMode = "copy"` literal. §6.8(c) gate grep (`importMode` once, `"copy"`; no `move`/`auto`). §6.8(d) `fake_arr` rejects any other mode, and 22 has a move test. §7.4 ReplaceRelease 1-3: the completed source is `EntryUnchanged` and re-`stat`ed. `PathStillHasInode` is required. A move gives `FailedNeedsReview` + `ArrImportMoved`. A copy gives `ExecutedUnverified` + `AddedCost` and leaves `DeleteMediaFile` blocked. Step 5 is now consistent. Also covered by ADR-0010 and V10. |
| F02 | blocking | **Fixed** | §7.3 `DeleteReplaced` planning rule: mapped, scanned regular file, equal size, distinct `PhysicalId`, `EntryUnchanged` precondition. A mismatch gives `ArrAmbiguous`/`KeepManual`, and only a plain deletion with reinforced confirmation remains possible. §7.4 delete step 1 re-`stat`s the replacements and re-reads the ARR path. §13.3 A7 repeats the rule for auto. |
| F03 | major | **Fixed** (clarification F21) | §4.1 `TorrentInfo.download_path`, candidates from all three bases plus `.!qB` variants, `is_working_file`. §7.1 step 3 ownership index used by `orphan_completed`, `incomplete_orphan`, `junk` and `NoTorrentReferences`. §7.4 DeleteIncompleteOrphan. §11 `download_path` mapping note. §15.3 V9. |
| F04 | major | **Fixed** | §5 `ArrRead::episodes` (all expected episodes, with or without files). §4.1 `ArrEpisode` (absolute and scene numbers), `episode_ids` on preview decisions, `CoverageProof`. §7.2 coverage definition (monitored and aired, specials opt-in, exactly-one mapping both ways). 07 is explanatory only, and disagreement gives `NumberingMismatch`. §18.1 INV-36 has the single owner 12. V10 covers it. |
| F05 | major | **Fixed** | §4.2 `arr_queue` category with INV-39 stale proof and `QueueItemHidesError`. §7.3 `PlanRequest::Media`. §12.1 `/media/...` routes, §12.2 `MediaDetailVm`, `DashboardVm.arr_queues`. §9.1 `arr_queue_items`. §18.4 inventory note. |
| F06 | major | **Fixed** | §13.2 `arr_grab` default **false**. §5 `ArrCommands::grab` (live adapter, capability-checked). §7.4 `ArrGrabRelease`: tier 4, manual only, never auto, live only, journaled `ArrGrab`, verification `ArrQueueHasRelease`. §13.3 A9 lists it. §4.1 `ReleaseSearchResult` (indexer, seeders, size, guid/indexer id, CF, rejections) and `sort_search_results`. C24, M9.3, ADR-0010. `ArrRead` has no grab method. |
| F07 | major | **Fixed** | §4.1 `MatchCandidate` (signature-bound validity), §5 `MatchStore`, §9.1 `match_candidates`. §7.1: `RefreshService` reclassifies only, with **zero fs calls and zero content reads**. §8.1 lists disk I/O "none". §8.5 item 3. B6 extended to `RefreshService` and UI requests. `scan.qbit_refresh_minutes` exists. |
| F08 | minor | **Fixed** | §7.2 `workflows::evaluate::verify_content` (agent 12): bounded, shared locks, results through `MatchStore::update_proof` and `attach_evidence`. |
| F09 | minor | **Fixed** | §7.4 common rules: affected hashes = source **or target** paths (library seeders included). Inode-only sharing is informational. |
| F10 | minor | **Fixed** | §7.1 (`NotConfigured` gives `ServiceNotConfigured(Qbittorrent)` on every ownership-dependent action and classification). OQ-05. |
| F11 | minor | **Fixed** (follow-up F23) | §7.6 two-phase cycle: consolidations, then rescan, then abort on errors, then deletion planning. Failed consolidations become blockers. |
| F12 | minor | **Fixed** (route detail F25) | §7.3 companion actions. `Approval.confirmed_torrent_removals`, `ConfirmVm.torrent_removals`. |
| F13 | minor | **Fixed** | `ArrCommands::rescan_all`, `PlanRequest::ServiceRescan` (one-action plans, so every intent has a plan), `POST /recovery/retry`, `JellyfinRead::library_refresh_running`, C25. |
| F14 | minor | **Fixed** (C20 statement contested by F20) | §17 C19-C25 and the complete accepted-cuts list. (e) resolved by `POST /plans/{id}/simulate` (ADR-0003). |
| F15 | minor | **Fixed** | §12.4 `htmx.org@2.0.4`, SHA-256 constant + unit test + SRI `integrity`, cross-checked against two published digests, release gate, no ship-without option. ADR-0006, handoff TODO 1. |
| F16 | minor | **Fixed** (detail F25) | §15.1 rules 7 (parity tests excluded from crate globs; 21 may add workspace dev-deps between waves) and 8 (no double "Creates / owns"). Rows 16, 17 and 19 rewritten. |
| F17 | minor | **Fixed** | §8.4 streamed counting limit, `limits.max_response_bytes` (32 MiB), ffprobe stdout cap. |
| F18 | minor | **Fixed** | `CandidateState` is derived only (`derive_candidate_state`, used by reporting). `PlanStatus::can_transition_to` is enforced. `torrent_observations` was removed (grep confirms). |
| F19 | minor | **Partially fixed** | §7.1 step 8, A2 and §4.3 agree. OQ-05 still says "any … protected zone" (open as residue). |

---

## Re-check of the six criteria (regressions and new gaps)

1. **Coverage.** No regression. The new elements (grab, queue category, coverage, simulation) are traced in
   §18.2 (M6.21, M9.3) and §18.4. CONTEXTE §3.6's ordering rule is the one new gap (F20).
2. **One owner per requirement.** It still holds. INV-36 now has a single owner (12). `verify_content` is owned by 12.
   No requirement is dropped. The accepted-cuts list (C6, C9, C14, C18, C19, C22) is complete and consistent
   with MASTER and CONTEXTE. ARR grab is no longer a cut.
3. **Contradictions.** C19-C25 were added with CONTEXTE-first resolutions. C20 needs the F20 amendment.
4. **Implementability.** The new ports and DTOs are fully specified for 03 (handoff 01 lists them for the domain
   freeze). Module ownership is still disjoint for the parallel waves:
   - wave 4: {`crates/storage`} vs {`crates/config`, `examples/`, `.env.example`};
   - wave 5: {`crates/fs` minus `mutate`, `testkit/fixture.rs` (V9 tree)} vs {`crates/qbit`,
     `testkit/fake_qbit.rs` (T_R)} vs {`crates/arr`, `testkit/fake_arr.rs` (V10)} vs {`crates/jellyfin`,
     `testkit/fake_jellyfin.rs`};
   - wave 13: {`web/src/auth`, `web/tests/auth_*`, *edits* of login template, web
     Cargo/router/lib/state, `crates/app/src`} vs {`docs/ux`} vs {Docker, compose, `.github`, `README.md`,
     `.env.example`, `deny.toml`, `docs/operations.md`}.
   - The root `Cargo.toml` rules are unchanged. The new configuration keys belong to 05 (wave 4).

   Remaining gaps are minor (F21, F24, F25).
5. **Safety.** The specific checks requested:
   - `arr_grab` defaults to `false`, is tier 4, manual approval only, excluded from auto by A9, live only
     (simulated in dry-run) and journaled. `ArrRead` cannot grab.
   - `importMode = "copy"` is a literal with no parameter, enforced by a gate grep and a fake that rejects
     anything else. A move is detected through the source inode check.
   - Refresh never reads file content: `RefreshService` makes zero fs calls (§7.1, §8.1, §8.5, B6).
   - Simulation in a live process always uses the recording set, never changes the plan status, and is ignored by
     recovery. A guard test is still missing (F22).
   - Still no `deleteFiles=true`, no `st_nlink` proof, no whole-NAS hashing, no unconfigured paths, no shell
     strings, no automatic deletion in media roots, no automatic action on incomplete scans or ambiguous mappings,
     and samples are still protected.
   - The new data-loss ordering risk inside approved manual replacements is F20.
6. **Simplicity.** No material new speculative abstraction. `MatchStore`/`match_candidates`, `simulation_runs`,
   `arr_queue_items`, the `PlanRequest` enum, `CoverageProof` and `ReleaseSearchResult` each have a named
   consumer. `CandidateState` became a single derived function, and `torrent_observations` was removed.

## Still compliant (unchanged from iteration 1)

The legacy-parity coverage (Appendix A, 32 tests, INV/REJ/CON sets) is unchanged and still covers all of
these:
- MASTER §2 invariants and anti-patterns;
- CONTEXTE §1.3, §2.12, §3 and §9.

These architecture elements are unchanged and still compliant:
- crate boundaries and dependency gates;
- structural dry-run (`LiveMode`);
- the SQLite `synchronous=FULL` intent journal;
- recovery;
- the single execution lane with root locks;
- the configuration contract;
- route security (session, CSRF, rate limit, CSP);
- the tier 1-5 mapping with capability flags and `IMPLEMENTED_TIER`;
- budgets B1-B11.
