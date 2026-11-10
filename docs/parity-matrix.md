# Legacy parity matrix (T-01..T-32)

Owned by `unit-tests` (21). Source of truth for the legacy mapping: `docs/legacy-parity.md` §2. Status values:
`covered` (Rust test exists and passes), `pending subsystem` (owner not yet delivered), `superseded`, `obsolete`.
A superseded or obsolete row still needs a replacement test once its owner ships; the replacement is named in the last column.

Last update: pass 1 (after wave 3, domain only).

| ID | Legacy test | Planned status | Parity status | Rust test(s) | Owning subsystem(s) |
|---|---|---|---|---|---|
| T-01 | `test_scans_real_hardlinks_and_deduplicates_allocated_size` | keep | pending subsystem | -- | fs-engine, issue-engine |
| T-02 | `test_nlink_outside_media_does_not_count_as_imported` | keep | pending subsystem | -- | fs-engine, issue-engine, reporting |
| T-03 | `test_gain_is_counted_only_when_every_hardlink_is_selected` | keep | covered | `removarr_domain::gain::tests::gain_counted_only_when_every_hardlink_is_selected`; also `inode_held_elsewhere_is_never_counted`, `zero_selected_links_and_overflow_are_safe`, `same_inode_recorded_by_two_actions_is_not_double_counted`; property `reclaimable_counts_each_inode_once_and_only_when_fully_selected` (`crates/domain/tests/parity_props.rs`) | domain-model, plan-engine, reporting |
| T-04 | `test_similarity_uses_title_year_episode_and_size` | keep | pending subsystem | -- | media-match |
| T-05 | `test_scene_title_stops_before_identity_marker_and_ignores_release_group` | keep | pending subsystem | -- | media-match |
| T-06 | `test_auto_candidate_refuses_different_editions` | keep | pending subsystem | -- | media-match, plan-engine |
| T-07 | `test_auto_candidate_requires_exact_normalized_title` | keep | pending subsystem | -- | media-match, plan-engine |
| T-08 | `test_writes_bidirectional_unlinked_inventories` | keep | pending subsystem | -- | issue-engine, reporting |
| T-09 | `test_duration_verification_blocks_a_different_cut` | keep | pending subsystem | -- | media-match, plan-engine |
| T-10 | `test_identical_copy_is_proposed_for_hardlink_repair` | keep | pending subsystem | -- | media-match |
| T-11 | `test_same_name_and_size_is_only_a_potential_duplicate` | keep | pending subsystem | -- | media-match, action-runner |
| T-12 | `test_quick_match_can_never_bypass_final_exact_comparison` | keep | pending subsystem | -- | media-match, action-runner |
| T-13 | `test_hardlink_repair_links_completed_toward_media` | keep | pending subsystem | -- | action-runner |
| T-14 | `test_hardlink_permission_is_checked_before_full_comparison` | keep (message must not recommend `sudo`, CON-30) | pending subsystem | -- | action-runner, fs-engine |
| T-15 | `test_smallest_orphan_folder_is_proposed_with_sidecars` | adapt | pending subsystem | -- | issue-engine, plan-engine |
| T-16 | `test_changed_folder_is_rejected` | keep | pending subsystem | -- | plan-engine, action-runner |
| T-17 | `test_deletion_never_follows_a_symlink` | keep | pending subsystem | -- | action-runner, fs-engine |
| T-18 | `test_report_directory_cannot_be_under_media` | adapt | partial (config side) | `crates/config/tests/roots.rs::data_dir_must_be_outside_roots_and_contain_none`; report directory under data_dir is pending in reporting | config-secrets, reporting |
| T-19 | `test_auto_candidate_requires_age_identity_quality_and_language` | superseded (CON-02) | pending subsystem (superseded by CON-02) | -- replacement: ARR-snapshot gate test in plan-engine | plan-engine, arr-api, media-match |
| T-20 | `test_auto_candidate_refuses_recent_and_failed_repair_paths` | keep | pending subsystem | -- | plan-engine |
| T-21 | `test_auto_folder_is_all_or_nothing` | keep | pending subsystem | -- | plan-engine |
| T-22 | `test_hardlink_interrupt_resumes_unmodified_torrent` | keep | pending subsystem | -- | action-runner, job-runner |
| T-23 | `test_partial_torrent_is_left_stopped_and_reported` | keep | pending subsystem | -- | action-runner |
| T-24 | `test_id_ranges` | obsolete | pending subsystem (obsolete, principle kept) | -- replacement: explicit-ID selection and unknown-ID rejection in backend-api | backend-api, web-ui |
| T-25 | `test_api_mapping_stop_recheck_and_safe_delete` | keep | pending subsystem | -- | qbit-api |
| T-26 | `test_mapping_rejects_path_outside_container_root` | keep (+ `..` case, CON-07) | covered (config side) | `crates/config/tests/mapping.rs`: `classification`, `traversal_and_garbage_are_invalid`, `ancestor_rule_still_confines_to_roots`, property `confinement_and_round_trip`; `crates/config/tests/parity_redaction.rs`: `dot_dot_never_escapes_even_when_it_would_land_in_another_root`, `prefix_matching_is_case_sensitive_and_component_bounded`; qbit-side wiring pending | config-secrets, qbit-api |
| T-27 | `test_qbittorrent_v4_uses_pause` | keep | pending subsystem | -- | qbit-api |
| T-28 | `test_qbittorrent_v5_2_accepts_204_login` | keep | pending subsystem | -- | qbit-api |
| T-29 | `test_api_failure_is_wrapped` | keep | pending subsystem | -- | qbit-api |
| T-30 | `test_dry_run_does_not_mutate_files_or_qbittorrent` | keep (stricter: also `start`/`resume`) | pending subsystem | -- | action-runner, job-runner, qbit-api, arr-api, jellyfin-api |
| T-31 | `test_auto_mode_repairs_proven_duplicate_without_prompt` | adapt (CON-21, CON-05) | pending subsystem | -- | job-runner, plan-engine, action-runner |
| T-32 | `test_automatic_cleanup_removes_only_empty_torrent_without_files` | adapt (CON-04, CON-05) | pending subsystem | -- | action-runner, qbit-api, plan-engine |

Count: 32 rows, 2 covered (T-03; T-26 config side), 1 partial (T-18 config side), 29 pending (T-19 superseded, T-24 obsolete, both with replacement owners). No row omitted.

Domain invariants with domain-level tests but no T-xx row: INV-17 (`outcome_from_verification`), INV-21 (gain ids, `aggregate_unique`), INV-13 (plan hash, tamper, approval), INV-15 fail-closed approval (`incomplete_evidence_cannot_be_automatically_approved`), INV-45/INV-12 type separation (`evidence` tests).
