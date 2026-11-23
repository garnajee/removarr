# Legacy parity matrix — removarr

Produced by `00-legacy-audit` (wave 0). Read-only audit of `old/`. This document is the traceable
bridge between the legacy Python tool and the Rust rewrite. Later agents reference its stable IDs.

## 0. Conventions

### 0.1 Sources and precedence

| Source | Role | Precedence |
|---|---|---|
| `old/doublon_checker.py` (3214 lines) | Behavioral source of truth (software behavior) | 1 for behavior |
| `old/test_doublon_checker.py` (32 tests) | Asserted behavior | 2 for behavior |
| `old/CONTEXTE_RACINE_APP_NAS.md` ("CONTEXTE") | Business rules, invariants, rejected choices, history | Wins over MASTER-PROMPT on business rules |
| `old/generate_final_audit.py` | Last read-only correlation audit (files / qBittorrent / ARR) | Reference for reporting and classification |
| `old/rapport-final.md` | Last human report (historical state) | Historical evidence only |
| `MASTER-PROMPT.md` ("MASTER") | Product requirements for the Rust app | Wins over agent files; loses to CONTEXTE on business rules |

Where MASTER deliberately changes legacy behavior (configured roots instead of personal paths,
configurable expected owner, qBittorrent state allowlist, web UI instead of terminal), the change is
recorded in section 5 as a *deliberate change*, not as a defect.

### 0.2 Symbolic roots (no live paths)

| Symbol | Meaning | Legacy equivalent |
|---|---|---|
| `<completed_root>` | `download_roots.completed` | `Config.completed_path` |
| `<incomplete_root>` | `download_roots.incomplete` | none (not handled by the legacy engine) |
| `<media_root>` | `media_roots` (library read by Jellyfin) | `Config.media_path` |
| `<common_parent>` | Single volume mounted in the container that contains all roots | `Config.qbittorrent_host_path` (mapping host side) |
| `<app_data>` | Config volume (SQLite, exports) | `Config.report_path` (legacy reports dir) |
| `<undeclared_sibling>` | Any directory under `<common_parent>` not declared as a root | historical personal excluded directory (CONTEXTE §1.3.8) |

Historical absolute NAS paths, service URLs, user names, UID/GID values and tracker data are
intentionally not reproduced here.

### 0.3 ID scheme

| Prefix | Section | Meaning |
|---|---|---|
| `CFG-xx` | 1.1 | Legacy configuration parameter |
| `DR-xx` | 1.2 | Domain rule extracted from legacy code |
| `WF-xx` | 1.3 | Legacy workflow |
| `RPT-xx`, `EVT` | 1.4 | Legacy report / journal event |
| `API-xx` | 1.5 | External API usage |
| `FH-xx` | 1.6 | Failure handling / exit code |
| `T-xx` | 2 | Legacy test (1..32) |
| `INV-xx` | 3.1 | Permanent invariant |
| `HIS-xx` | 3.2 | Historical state (never current) |
| `ONE-xx` | 3.3 | One-off decision (lesson extracted) |
| `REJ-xx` | 4 | Rejected approach |
| `CON-xx` | 5 | Contradiction / legacy defect / deliberate change |
| `NEW-xx` | 6.1 | MASTER requirement without legacy equivalent |
| `CTX-xx` | 6.2 | CONTEXTE requirement not explicit in MASTER |
| `FX-xx` | 7 | Parity fixture entry |
| `OQ-xx` | 8 | Open question for `system-arch` |

### 0.4 Destination subsystems

`fs-engine`, `media-match`, `qbit-api`, `arr-api`, `jellyfin-api`, `issue-engine`, `plan-engine`,
`action-runner`, `job-runner`, `reporting`, `config-secrets`, `db-layer`, `domain-model`,
`backend-api`, `auth-guard`, `web-ui`, `devops`. Test-owning agents (`unit-tests`,
`integration-tests`, `e2e-perf`) are named where relevant.

---

## 1. Legacy inventory

### 1.1 Configuration (`Config` dataclass, `validate_configuration`, `__main__` block)

| ID | Legacy parameter | Historical value | Rust destination | Notes |
|---|---|---|---|---|
| CFG-01 | `completed_path` | personal path | config-secrets: `download_roots.completed` | Must exist, be a dir, resolved strictly |
| CFG-02 | `media_path` | personal path | config-secrets: `media_roots` | Must not contain / be contained by completed |
| CFG-03 | `report_path` | personal path | config-secrets: data dir (`<app_data>`) | Must be outside completed and media (T-18) |
| CFG-04 | `qbittorrent_url/username/password` | secrets | config-secrets (env vars) | Password required in dry-run/auto (legacy) |
| CFG-05 | `qbittorrent_container_path` / `qbittorrent_host_path` | `/data` / volume root | config-secrets: `path_mappings.qbittorrent` | completed must be under host path |
| CFG-06 | `video_extensions` | `{.mkv}` | config-secrets | Normalized to lower-case with leading dot |
| CFG-07 | `similarity_threshold` | 0.60 | media-match | Identification only, never quality |
| CFG-08 | `recheck_timeout_seconds` | 600 | qbit-api / action-runner | |
| CFG-09 | `quick_sample_size` / `quick_sample_count` | 1 MiB / 5 | media-match | Must be > 0 |
| CFG-10 | `full_compare_chunk_size` | 16 MiB | fs-engine | Must be >= 64 KiB |
| CFG-11 | `search_similar_names_for_hardlink` | False in `__main__`, True as function default | media-match | See CON-16 |
| CFG-12 | `run_mode` | `interactive` (`dry-run`, `auto`) | config-secrets / job-runner | See CON-03 |
| CFG-13 | `auto_min_age_hours` | 48 | plan-engine | >= 0 |
| CFG-14 | `auto_delete_similarity_threshold` | 0.90 | plan-engine | In [0,1]; identification only |
| CFG-15 | `auto_remove_empty_torrents` | True | plan-engine | See CON-04 |
| CFG-16 | `ffprobe_command`, `ffprobe_host_path`, `ffprobe_container_path` | empty / volume / `/data` | media-match | Required when duration match is on |
| CFG-17 | `auto_require_duration_match`, tolerance seconds / ratio | False, 5.0 s, 0.002 | media-match / plan-engine | Tolerance >= 0 |
| CFG-18 | `ffprobe_workers` | 4 | media-match | > 0; bounded concurrency |
| CFG-19 | qBittorrent HTTP timeout | 20 s (`QbitClient.__init__`) | qbit-api | |
| CFG-20 | ffprobe timeout | 120 s (`probe_media_duration`) | media-match | |

### 1.2 Domain rules (legacy code)

| ID | Rule | Evidence (`doublon_checker.py` symbol) | Destination |
|---|---|---|---|
| DR-01 | Physical identity is `(st_dev, st_ino)`. | `EntryRecord.identity` | domain-model, fs-engine |
| DR-02 | Scan: iterative DFS with `scandir`, `lstat` semantics (`follow_symlinks=False`); kinds `file`/`dir`/`symlink`/`other`; size and allocated only for regular files; allocated = `st_blocks*512` (fallback `ceil(size/4096)*4096`); `is_video` = regular file with case-folded suffix in extensions; root resolved strictly and must be a dir; symlinks recorded, never traversed; every `scandir`/`stat` failure appended as `ScanError(path, message)` and the walk continues. | `scan_tree`, `make_entry`, `allocated_bytes`, `entry_kind` | fs-engine |
| DR-03 | Statistics per tree: files, videos, logical bytes, unique allocated bytes (one per identity), unique inodes, errors. Cross-tree: shared inodes (completed ∩ media identities), shared allocated, combined unique allocated, completed-exclusive reclaimable, orphan-video reclaimable. | `tree_statistics`, `compute_scan_statistics` | issue-engine, reporting |
| DR-04 | Reclaimable accounting: an identity counts its allocated bytes once iff it is not present in media AND the number of selected paths for that identity is >= `st_nlink`. `nlink` is used for accounting only, never as hardlink proof. | `estimate_reclaimable`, `compute_scan_statistics` | domain-model, reporting, plan-engine |
| DR-05 | Orphan video = completed video whose identity is absent from the set of media *file* identities (symlinks never count). | `build_deletion_candidates`, `write_unlinked_inventory_reports` | issue-engine |
| DR-06 | Deletion candidates: a directory under completed (never the root) qualifies if every video below it is orphan and no scan error lies below it; keep only leaf-most qualifying directories; folder candidate = directory record + all descendants; remaining orphan videos become file candidates; candidates sorted by case-folded path then kind, IDs 1..n. | `build_deletion_candidates`, `error_below` | issue-engine, plan-engine |
| DR-07 | Folder snapshot: map relative path -> `(kind, dev, ino, size, nlink, mtime_ns, mode)` including `"."`; any snapshot error drops the folder candidate. | `snapshot_subtree`, `EntryRecord.signature` | fs-engine, plan-engine |
| DR-08 | Scene/P2P parsing: NFKD + casefold + ASCII; `SxxEyy` regex; year 19xx/20xx; tokens `[a-z0-9]+`; first resolution/codec/source token; `web`+`dl` -> `web-dl`; language set; editions (incl. director's cut, open matte); title = tokens before the earliest episode/year match, else before the first strong technical marker; fallback filters scene markers. | `parse_scene_name`, marker sets | media-match |
| DR-09 | Similarity score: 0 if both have different episodes or no common title token; else `0.55*SequenceMatcher(title) + 0.25*Jaccard(title) + 0.10*min/max(size)`, `+0.10` same year / `-0.20` different year, `+0.15` same episode, clamped to [0,1]. Identification only. | `similarity_score` | media-match |
| DR-10 | Candidate pool: title-token index; with episode -> token pool ∩ episode pool (episode pool alone if no token hit); with year -> same-year subset of token pool if non-empty else token pool; keep score >= threshold; sort `(-score, abs(size diff), path)`; limit 3 (10 for repair search). | `SimilarityEngine.find` | media-match |
| DR-11 | Hardlink-repair discovery runs only when the media scan has no errors and both roots share `st_dev`. For each completed video not already shared and on the same device: exact `(casefold(filename), size)` matches among media videos -> one repair per match, **no content read** (`exact_name_and_size`); otherwise, if enabled, top-10 similar names with equal size -> sample check -> first passing pair (`similar_name_and_samples`). | `discover_hardlink_repairs` | media-match |
| DR-12 | Sample check: regular files, equal size; size 0 -> true; width `min(sample_size,size)`; offsets evenly spread `round(i*span/(count-1))` incl. first and last; any mismatch or `OSError` -> false. A positive result is a proposal only. | `files_match_samples` | media-match, fs-engine |
| DR-13 | Full comparison: both regular, equal size; buffers `max(64 KiB, chunk)`; sequential `posix_fadvise`; compare every chunk; any `OSError` -> false. | `files_are_identical` | fs-engine |
| DR-14 | Automatic assessment: age reason if the newest mtime among candidate entries plus `lstat(candidate)` is newer than `now - min_age`; blocked video paths (planned/unresolved/failed repair); for each video the first similarity without rejection is the replacement; eligible iff no reason and >= 1 video. Reason codes: `score_insuffisant`, `titre_different_ou_ambigu`, `episode_different_ou_absent`, `annee_absente`, `annee_differente`, `resolution_absente`, `resolution_jellyfin_inferieure`, `langue_absente_ou_incompatible`, `edition_differente_ou_ambigue`, `aucun_remplacant`, `reparation_hardlink_planifiee` / `_non_resolue` / `_en_echec`, `cible_inaccessible_pendant_controle_age`, `modification_plus_recente_que_<N>h`, `criteres_prudents_satisfaits`. | `assess_auto_candidate`, `auto_match_rejection`, `candidate_age_reason` | plan-engine, media-match (see CON-02) |
| DR-15 | Language compatibility: classes `fr`, `en`, `fr_sub`, `multi`; missing tags -> incompatible; MULTI/DUAL source requires MULTI/DUAL replacement; MULTI/DUAL replacement accepted; otherwise source classes must be a subset. | `language_profile`, `languages_are_compatible` | media-match |
| DR-16 | Duration check (optional): ffprobe both paths in parallel (bounded workers); tolerance `max(tol_seconds, max(d1,d2)*tol_ratio)`; probe error -> `duree_non_verifiee`, mismatch -> `duree_differente`; decision downgraded to keep. ffprobe invoked as an argv list (no shell). | `verify_auto_decision_durations`, `probe_media_duration` | media-match, plan-engine |
| DR-17 | Revalidation: file signature `(kind, dev, ino, size, nlink, mtime_ns, mode)` via `lstat` must equal scan; folder snapshot must be equal; repair variant ignores `nlink` (changes caused by the same batch). | `record_still_matches`, `repair_record_still_matches`, `revalidate_candidate` | plan-engine, action-runner |
| DR-18 | Confinement: delete target resolved strictly must be strictly under completed root (never the root); in folder deletion a symlink leaf is allowed if its resolved parent is under the root (the link itself is unlinked); media target must resolve under media root and be a regular file. | `ensure_delete_target`, `ensure_media_target` | fs-engine, action-runner |
| DR-19 | Folder deletion: unlink all non-dir snapshot entries deepest first (symlinks unlinked, not followed), then `rmdir` dirs deepest first, then the folder; partial progress is logged on failure. | `delete_candidate` | action-runner |
| DR-20 | Hardlink repair transaction: intent event -> confinement checks -> revalidate both (nlink-tolerant) -> same device -> permission test -> full compare -> revalidate again -> `link(completed, temp in media dir)` -> atomic `replace(temp, media)` -> verify same `(dev, ino)` -> `fsync` media parent dir -> success event; on failure remove temp if not replaced and log failure. | `create_hardlink_repair` | action-runner |
| DR-21 | Permission test: create a temporary hardlink in the target dir, check same inode, unlink; on `EPERM` raise an explanatory error (process UID vs file owner, `fs.protected_hardlinks`, ACL, immutable). | `verify_hardlink_creation_allowed` | action-runner, fs-engine |
| DR-22 | Temporary names live in the same directory (`.<name>.doublon-checker-link-<pid>-<ns>`); reports are written to `.<name>.tmp-<pid>`, flushed, fsynced, then atomically renamed. | `create_hardlink_repair`, `atomic_write_text` | action-runner, reporting |
| DR-23 | Torrent file path reconstruction: candidates `save_path/name`; `content_path` if single-file; `content_path/name` (dropping the first component when it equals the content dir name); first candidate present in the scan wins, else the first candidate. | `choose_qbit_file_path` | qbit-api |
| DR-24 | qBittorrent index scope: only torrents whose save or content path lies under completed (or completed under save path); files outside completed dropped; mapping errors when a file path cannot be rebuilt or when a torrent with content in completed maps no file. Torrents whose paths are outside the container root are silently skipped (CON-07). | `build_qbit_index` | qbit-api |
| DR-25 | Affected hashes = union of hashes of every file path in the selected candidates (path-based; several hashes per path allowed). | `affected_hashes_for_candidates` | plan-engine |
| DR-26 | Unused torrent = has >= 1 wanted file (`priority > 0`) and every wanted file is planned for removal or no longer exists (`lexists`). | `find_unused_torrents` | plan-engine, action-runner |
| DR-27 | "Previously active" = state not starting with `paused`/`stopped` (case-insensitive). | `torrent_was_active` | action-runner |
| DR-28 | qBittorrent major >= 5 uses `stop`/`start`, otherwise `pause`/`resume`. | `QbitClient.stop/start`, `major_version` | qbit-api |
| DR-29 | Recheck wait: poll `torrents/info`; while any hash is `checking*` sleep 2 s; success once checking was seen and ended, or after 2 s if all hashes are listed; timeout -> false (CON-12). | `wait_for_recheck` | qbit-api, action-runner |
| DR-30 | Torrent restoration: unmodified previously-active hashes resumed; repairs: recheck modified hashes, on success resume those previously active; deletions: recheck, then remove unused hashes only if policy (or interactive confirmation), all other affected hashes left stopped and reported; timeout or qBittorrent error -> left stopped and recorded. | `execute_hardlink_repairs`, `execute_deletions`, `record_stopped_torrents` | action-runner |
| DR-31 | Gains: repair gain = `estimate_reclaimable(replaced media records, ∅)`; deletion gain = `estimate_reclaimable(removed records, media identities)`; observed immediate / final = free-space delta on the completed filesystem; summary always notes snapshots/open files may distort observed gain. | `execute_*`, `build_summary_text` | reporting, domain-model |
| DR-32 | Config validation: mode in allowed set; thresholds in range; ffprobe requirements; report dir outside both roots; roots distinct and non-nested; completed under qBittorrent host mapping; sample params > 0; chunk >= 64 KiB; completed and media under ffprobe host mapping when used. | `validate_configuration` | config-secrets |
| DR-33 | Container path mapping: `normpath` both sides, require `relative_to(container_root)`, else error; result joined under host root. | `map_container_path` | config-secrets, qbit-api |
| DR-34 | Automatic run is two-phase: initial plan -> repairs -> full rescan -> abort if rescan has errors -> rebuild candidates -> failed repairs = completed identity still absent from media -> final decisions -> final plan -> deletions with automatic torrent cleanup policy. | `main` (auto branch) | job-runner, plan-engine |
| DR-35 | Interactive confirmations: no preselection; typed phrase with exact count; extra explicit phrase when any selected item has no replacement; separate typed phrase for repairs and for torrent removal. | `interactive_selection`, `interactive_repair_selection`, `select_unused_torrents` | web-ui, backend-api |
| DR-36 | Final readable audit classification of completed-only videos: ARR match through qBittorrent hash -> ARR history `DownloadId` (episode-checked), else by name (title ratio >= 0.72, movie year ±1); classes `autre_release_deja_dans_jellyfin` (potential gain), `identifie_arr_absent_de_jellyfin` (import), `correspondance_jellyfin_a_valider` (best similarity >= 0.72 at threshold 0.35), `non_gere_ou_ambigu` (keep); media-only videos matched to ARR by file path then name; inventory of `stoppedUP` and `missingFiles` torrents. Read-only. | `generate_final_audit.py: main, arr_from_history, arr_from_name` | issue-engine, arr-api, reporting |

### 1.3 Workflows

| ID | Workflow | Evidence | Destination |
|---|---|---|---|
| WF-01 | Startup: validate config, create report dir; exit 2 on invalid config. | `main`, `validate_configuration` | config-secrets, job-runner |
| WF-02 | Observe: scan completed and media, statistics, warn on different devices and on scan errors (zones protected). | `main` | fs-engine, issue-engine |
| WF-03 | Propose: deletion candidates (always), repair proposals (only if media scan clean and same device). | `main` | issue-engine, media-match |
| WF-04 | Assess (non-interactive): automatic decisions with planned repairs blocking their sources; optional ffprobe duration check. | `main`, `assess_auto_candidates` | plan-engine |
| WF-05 | Report: unlinked inventories, scan JSON, candidates CSV, empty actions JSONL, duration report; exit 3 if reports cannot be written (no deletion authorized). | `write_*` | reporting |
| WF-06 | Gate: interactive with nothing to do -> summary, exit 0; non-interactive with any scan error -> `automation_blocked`, exit 7; any mode with media scan errors -> exit 7. | `main` | job-runner |
| WF-07 | Connect: qBittorrent login (prompt for password in interactive), index torrents; unavailable -> exit 5; mapping errors -> exit 8. | `main`, `build_qbit_index` | qbit-api |
| WF-08 | Dry-run: write auto plan (repairs, eligible deletions, predicted unused torrents), journal `auto_plan_written`, summary "no mutation", exit 0. | `main` | plan-engine, job-runner |
| WF-09 | Auto: WF-08 plan, then DR-34 two-phase execution; KeyboardInterrupt -> exit 130; qBittorrent error -> exit 6. | `main` | job-runner, action-runner |
| WF-10 | Interactive: repair selection -> repair batch -> rescan (abort if media errors) -> post-repair report -> deletion selection -> deletion batch. | `main` | web-ui, action-runner |
| WF-11 | Repair batch: affected hashes, stop all, revalidate all (abort + resume on change), per-repair transaction DR-20, interrupt handling, resume unmodified, recheck repaired, resume previously active on success. | `execute_hardlink_repairs` | action-runner, qbit-api |
| WF-12 | Deletion batch: affected hashes, stop all, revalidate all (abort + resume on change), batch intent, per-candidate delete, interrupt handling, gains, resume unmodified, recheck affected, unused torrent removal (`deleteFiles=false`), partial torrents left stopped, summary. | `execute_deletions` | action-runner, qbit-api |
| WF-13 | Final readable audit: scan both roots (abort on any scan error), qBittorrent file index, ARR data (direct SQLite, REJ-15), classification DR-36, CSV + Markdown + manifest. | `generate_final_audit.py: main` | issue-engine, reporting |

### 1.4 Reports and journal events

| ID | Artifact | Content | Rust destination |
|---|---|---|---|
| RPT-01 | `scan-<stamp>.json` | config summary (mode, automation, detection settings), statistics, scan errors per root, repair proposals, candidates with similarities | db-layer (audit session) + reporting (export) |
| RPT-02 | `candidates-<stamp>.csv` | id, type, path, video count, logical, estimated reclaimable, has_unmatched_video, similar paths, hardlink target, auto_eligible, auto_action, auto_reason | reporting |
| RPT-03 | `actions-<stamp>.jsonl` | append-only journal, one fsynced JSON line per event | db-layer (action journal) + action-runner |
| RPT-04 | `summary-<stamp>.txt` | logical deleted, attributable gain (repairs vs deletions), observed immediate/after qBittorrent, counts, torrents left stopped, cumulative gain (CON-13), snapshot caveat | reporting |
| RPT-05 | `unlinked-completed-<stamp>.csv`, `unlinked-medias-<stamp>.csv` | bidirectional inventories: path, logical, allocated, nlink, links seen in tree, possible external links, parsed fields, best match | issue-engine + reporting |
| RPT-06 | `auto-plan-<stamp>[-post-repair].json` | criteria, repairs, deletion decisions, blocked repair paths, predicted unused torrents, `dry_run` flag | plan-engine (immutable plan) |
| RPT-07 | `post-repair-<stamp>.json` | statistics, scan errors, remaining candidates | job-runner, reporting |
| RPT-08 | `duration-check-<stamp>[-phase].json` | per pair durations, difference, tolerance, match, error | media-match, reporting |
| RPT-09 | final audit dir: `completed-a-classer.csv`, `jellyfin-sans-source-completed.csv`, `qbittorrent-arretes-ou-manquants.csv`, `rapport.md`, `manifest.json` | DR-36 classification and counts | reporting |

Journal events (`EVT`, `append_jsonl` callers) to be mapped onto the Rust action journal and event
schema (`action-runner`, `job-runner`, `db-layer`): `hardlink_intent`, `hardlink_success`,
`hardlink_failure`, `hardlink_batch_aborted`, `hardlink_batch_interrupted`,
`hardlink_interrupt_resume_failure`, `hardlink_resume_failure`, `hardlink_recheck_timeout`,
`hardlink_qbittorrent_failure`, `hardlink_batch_blocked`, `delete_intent`, `delete_success`,
`delete_failure`, `deletion_batch_intent`, `deletion_batch_interrupted`,
`deletion_interrupt_resume_failure`, `deletion_unmodified_resume_failure`, `session_aborted`,
`recheck_timeout`, `unused_torrents_found`, `torrents_removed` (always `deleteFiles: false`),
`torrents_left_stopped`, `qbittorrent_post_delete_failure`, `qbittorrent_unavailable`,
`qbittorrent_mapping_errors`, `qbittorrent_failure`, `automation_blocked`, `auto_plan_written`,
`session_summary`.

### 1.5 External APIs

| ID | API | Legacy usage | Evidence | Rust destination |
|---|---|---|---|---|
| API-01 | qBittorrent `POST /api/v2/auth/login` | form `username`, `password`; body `"Ok."` (<= 5.1) or empty with HTTP 204 (>= 5.2) accepted; anything else = refused; login confirmed by `GET app/version` matching `^v?\d+(\.\d+)+` | `QbitClient.login` | qbit-api |
| API-02 | `GET /api/v2/app/version` | version + auth confirmation; major version drives endpoint names | `login`, `major_version` | qbit-api |
| API-03 | `GET /api/v2/torrents/info` | `hash`, `name`, `state`, `save_path`, `content_path` (+ `progress`, `category`, `size` in final audit) | `torrents_info`, `build_qbit_index` | qbit-api |
| API-04 | `GET /api/v2/torrents/files?hash=` | `name`, `size`, `priority` (+ `progress` in final audit) | `torrent_files` | qbit-api |
| API-05 | `POST torrents/stop` or `torrents/pause` | `hashes` joined by `|`, sorted, unique | `stop` | qbit-api |
| API-06 | `POST torrents/start` or `torrents/resume` | same | `start` | qbit-api |
| API-07 | `POST torrents/recheck` | same | `recheck` | qbit-api |
| API-08 | `POST torrents/delete` | `hashes`, `deleteFiles=false` hard-coded (no parameter) | `delete` | qbit-api |
| API-09 | Request conventions | cookie jar session, `Referer` and `Origin` headers set to base URL, booleans encoded `true`/`false`, 20 s timeout, every transport/HTTP/JSON error wrapped in `QbitError` | `_request` | qbit-api |
| API-10 | Sonarr/Radarr | direct read-only SQLite (`Series`, `Episodes`, `EpisodeFiles`, `History.DownloadId`, `Movies`, `MovieMetadata`, `MovieFiles`) — **rejected** (REJ-15); Rust uses REST APIs only | `generate_final_audit.py: load_arr` | arr-api |
| API-11 | Jellyfin | none in legacy code; CONTEXTE §5.4 notes a full library refresh accepted with HTTP 204 | — | jellyfin-api |
| API-12 | ffprobe | argv `ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 <mapped path>`, 120 s timeout, duration must be finite > 0 | `probe_media_duration` | media-match |

### 1.6 Failure handling and exit codes

| ID | Situation | Legacy behavior | Evidence | Rust destination |
|---|---|---|---|---|
| FH-01 | Invalid config | exit 2, nothing scanned | `main` | config-secrets (startup refusal) |
| FH-02 | Reports not writable | exit 3, "no deletion authorized" | `main` | db-layer / job-runner (no plan without durable storage) |
| FH-03 | qBittorrent password prompt cancelled | exit 4, summary saved | `main` | n/a (web auth) |
| FH-04 | qBittorrent unreachable / auth not confirmed | exit 5, `qbittorrent_unavailable`, reports kept | `main`, `login` | qbit-api, job-runner |
| FH-05 | qBittorrent error during auto or batch | exit 6, `qbittorrent_failure`, summary | `main` | action-runner |
| FH-06 | Scan errors (auto/dry-run: any; interactive: media) | exit 7, `automation_blocked` | `main` | job-runner, plan-engine |
| FH-07 | qBittorrent mapping errors | exit 8, `qbittorrent_mapping_errors` | `main` | qbit-api, plan-engine |
| FH-08 | KeyboardInterrupt during auto | exit 130, unmodified torrents resumed, summary | `main`, `execute_*` | job-runner (safe cancel), action-runner |
| FH-09 | Revalidation failure before batch | resume previously-active hashes, `session_aborted` / `hardlink_batch_aborted`, all repairs counted failed | `execute_*` | action-runner |
| FH-10 | Single repair/delete failure | logged with partial counts; batch continues with next item (CON-26) | `create_hardlink_repair`, `delete_candidate` | action-runner |
| FH-11 | Recheck timeout | torrents stay stopped, `*_recheck_timeout` | `execute_*` | action-runner |
| FH-12 | qBittorrent error after local mutation | torrents stay stopped, local summary saved | `execute_*` | action-runner |
| FH-13 | ENOENT / any OSError during scan | recorded as `ScanError`, subtree protected | `scan_tree`, `error_below` | fs-engine |
| FH-14 | `EPERM` on hardlink | explanatory error before any content read | `verify_hardlink_creation_allowed` | action-runner |

### 1.7 Legacy symbol -> Rust subsystem map

| Legacy symbol(s) | Rust subsystem |
|---|---|
| `EntryRecord`, `TreeScan`, `ScanError`, `ParsedName`, `Similarity`, `DeletionCandidate`, `HardlinkRepair`, `AutoCandidateDecision`, `QbitFile/QbitTorrent/QbitIndex`, `DeletionResult` | domain-model |
| `scan_tree`, `make_entry`, `allocated_bytes`, `snapshot_subtree`, `error_below`, `files_are_identical`, `path_is_within`, `lexical_key` | fs-engine |
| `parse_scene_name`, `similarity_score`, `SimilarityEngine`, `language_profile`, `languages_are_compatible`, `auto_match_rejection`, `files_match_samples`, `discover_hardlink_repairs`, `probe_media_duration`, `verify_auto_decision_durations` | media-match |
| `QbitClient`, `map_container_path`, `choose_qbit_file_path`, `build_qbit_index`, `wait_for_recheck` | qbit-api |
| `generate_final_audit.load_arr / arr_from_history / arr_from_name` | arr-api (REST rewrite) + issue-engine |
| `compute_scan_statistics`, `tree_statistics`, `build_deletion_candidates`, `write_unlinked_inventory_reports` (logic) | issue-engine |
| `assess_auto_candidate(s)`, `candidate_age_reason`, `affected_hashes_for_candidates`, `find_unused_torrents`, `file_paths_for_candidates`, `write_auto_plan` | plan-engine |
| `record_still_matches`, `repair_record_still_matches`, `revalidate_candidate`, `ensure_delete_target`, `ensure_media_target`, `verify_hardlink_creation_allowed`, `create_hardlink_repair`, `delete_candidate`, `execute_hardlink_repairs`, `execute_deletions`, `torrent_was_active`, `append_jsonl` | action-runner (+ db-layer for the journal) |
| `main` orchestration, run modes, rescans | job-runner |
| `write_*` reports, `build_summary_text`, `save_session_summary`, `human_size`, `cumulative_gain` (replaced) | reporting |
| `Config`, `validate_configuration`, `__main__` constants | config-secrets |
| `interactive_selection`, `interactive_repair_selection`, `parse_id_expression`, `select_unused_torrents`, `candidate_line`, `show_candidate_details`, `print_statistics` | web-ui + backend-api (terminal UI obsolete) |

---

## 2. Test parity matrix (32 legacy tests)

Status: **keep** = port the assertion as is; **adapt** = port with a documented change;
**superseded** = assertion contradicts a permanent rule, replace by the stated behavior;
**obsolete** = terminal-only, principle preserved.

| ID | Legacy test (`test_doublon_checker.py`) | Legacy symbols | What it proves | Expected Rust behavior | Destination | Status |
|---|---|---|---|---|---|---|
| T-01 | `test_scans_real_hardlinks_and_deduplicates_allocated_size` (body in `scans_real_hardlinks_and_deduplicates_allocated_size`) | `scan_tree`, `compute_scan_statistics`, `build_deletion_candidates` | A real hardlink completed<->media counts as 1 shared inode, 0 orphan reclaimable, no candidate | Scanning a real `link()` pair yields `shared_inodes=1`, zero reclaimable, no hardlink-defect issue | fs-engine, issue-engine | keep |
| T-02 | `test_nlink_outside_media_does_not_count_as_imported` | same | `nlink=2` with the other link outside media is still an orphan; reclaimable 0 | `nlink` never proves import; issue raised with attributable gain 0 and "external link" evidence | fs-engine, issue-engine, reporting | keep |
| T-03 | `test_gain_is_counted_only_when_every_hardlink_is_selected` | `estimate_reclaimable` | Inode blocks counted only when all its links are selected | Expected/attributable gain counts an inode once and only if every link is removed by the plan | domain-model, plan-engine, reporting | keep |
| T-04 | `test_similarity_uses_title_year_episode_and_size` | `SimilarityEngine.find` | Same show + same episode ranks first; other episode excluded | Identification ranks by title/episode/year/size; never used as quality | media-match | keep |
| T-05 | `test_scene_title_stops_before_identity_marker_and_ignores_release_group` | `parse_scene_name` | Title tokens stop before `SxxExx`; release group ignored | Parser yields identical title tokens and episode `(1,1)` | media-match | keep |
| T-06 | `test_auto_candidate_refuses_different_editions` | `assess_auto_candidate` | Different editions -> not eligible, `edition_differente_ou_ambigue` | Edition mismatch is a structural blocker for any automatic replacement | media-match, plan-engine | keep |
| T-07 | `test_auto_candidate_requires_exact_normalized_title` | `assess_auto_candidate` | Different normalized title -> not eligible | Identity mismatch blocks automation (manual decision) | media-match, plan-engine | keep |
| T-08 | `test_writes_bidirectional_unlinked_inventories` | `write_unlinked_inventory_reports` | Completed-only and media-only lists exclude shared inodes | Hardlink-defect list (completed side) and media-only informational list, both excluding shared identities; exportable CSV | issue-engine, reporting | keep |
| T-09 | `test_duration_verification_blocks_a_different_cut` | `verify_auto_decision_durations` | 7200 s vs 6900 s -> not eligible, `duree_differente` | ffprobe duration mismatch beyond `max(5 s, 0.2 %)` blocks a replacement; probe error also blocks | media-match, plan-engine | keep |
| T-10 | `test_identical_copy_is_proposed_for_hardlink_repair` | `discover_hardlink_repairs` | Exact name+size proposes repair without reading samples | Name+size index proposes a consolidation candidate with zero content I/O at discovery | media-match | keep |
| T-11 | `test_same_name_and_size_is_only_a_potential_duplicate` | `discover_hardlink_repairs`, `create_hardlink_repair` | Same name+size, different bytes -> proposal exists but repair fails, inodes unchanged | Consolidation executes only after full byte comparison; mismatch -> action fails, nothing mutated | media-match, action-runner | keep |
| T-12 | `test_quick_match_can_never_bypass_final_exact_comparison` | `files_match_samples`, `create_hardlink_repair` | Samples match, one byte differs -> repair refused, media bytes intact | Positive samples never short-circuit the full compare | media-match, action-runner | keep |
| T-13 | `test_hardlink_repair_links_completed_toward_media` | `create_hardlink_repair` | Media path now has completed inode; completed inode unchanged; success event last | Direction completed -> media via temp link + atomic rename; post-verify `(dev, ino)`; journal success | action-runner | keep |
| T-14 | `test_hardlink_permission_is_checked_before_full_comparison` | `create_hardlink_repair`, `verify_hardlink_creation_allowed` | `EPERM` on `link` fails before any content read; error explains `fs.protected_hardlinks` | Permission probe (temp hardlink in target dir) precedes comparison; explanatory error, no read | action-runner, fs-engine | keep (message must not recommend `sudo`, CON-30) |
| T-15 | `test_smallest_orphan_folder_is_proposed_with_sidecars` | `build_deletion_candidates` | Two leaf season folders proposed (not the show), with `.srt` annexes | Smallest fully-eligible folder unit with annex preview; samples handled per CON-22/CON-23 | issue-engine, plan-engine | adapt |
| T-16 | `test_changed_folder_is_rejected` | `revalidate_candidate` | New file in folder after scan -> invalid | Any change in folder snapshot invalidates the action (no silent adaptation) | plan-engine, action-runner | keep |
| T-17 | `test_deletion_never_follows_a_symlink` | `delete_candidate` | Symlink inside folder is unlinked; outside target intact | Deletion never follows symlinks; only the link entry is removed; all ops confined | action-runner, fs-engine | keep |
| T-18 | `test_report_directory_cannot_be_under_media` | `validate_configuration` | Report dir under media rejected | Data dir / SQLite / exports must be outside every declared root; roots validated at startup | config-secrets | adapt |
| T-19 | `test_auto_candidate_requires_age_identity_quality_and_language` | `assess_auto_candidate` | 1080p H264 vs 2160p H265 eligible; lower 720p / incompatible language refused | Automatic replacement requires ARR snapshot admitting the candidate as an improvement; age, identity, edition, language, duration remain local *blockers*; resolution rank is explanatory only | plan-engine, arr-api, media-match | superseded (CON-02) |
| T-20 | `test_auto_candidate_refuses_recent_and_failed_repair_paths` | `assess_auto_candidate` | Recent mtime -> not eligible; failed repair path -> blocked | Minimum age gate; any path involved in a failed/unresolved consolidation is blocked | plan-engine | keep |
| T-21 | `test_auto_folder_is_all_or_nothing` | `assess_auto_candidate` | Folder with one video lacking replacement -> whole folder refused, reason names that video | Folder unit is all-or-nothing; blocker lists the offending member | plan-engine | keep |
| T-22 | `test_hardlink_interrupt_resumes_unmodified_torrent` | `execute_hardlink_repairs` | Interrupt before any success: stopped once, resumed once, `hardlink_batch_interrupted` last | Safe cancellation / crash: torrents not mutated are resumed, mutated ones stay stopped; journal records it | action-runner, job-runner | keep |
| T-23 | `test_partial_torrent_is_left_stopped_and_reported` | `execute_deletions` | Deleting 1 of 2 files: torrent stopped, not deleted, reported stopped | Partially touched torrent stays stopped and visible; never removed | action-runner | keep |
| T-24 | `test_id_ranges` | `parse_id_expression` | `"1, 3-5,9"` with valid `{1..5}` -> `{1,3,4,5}` | Terminal syntax obsolete; selection is an explicit set of listed item IDs, no preselection; unknown IDs rejected (stricter, CON-32) | backend-api, web-ui | obsolete |
| T-25 | `test_api_mapping_stop_recheck_and_safe_delete` | `QbitClient`, `build_qbit_index` | File path mapped from container; delete sends `deleteFiles=false`; stop called | Path mapping per service; remove-task API has no delete-files parameter and always sends `false` | qbit-api | keep |
| T-26 | `test_mapping_rejects_path_outside_container_root` | `map_container_path` | Path outside mapping -> error | Lexical prefix validation, confinement after resolution, reject `..` escapes | config-secrets, qbit-api | keep (+ `..` case, CON-07) |
| T-27 | `test_qbittorrent_v4_uses_pause` | `QbitClient.stop` | v4 uses `/pause` | v4 `pause`/`resume`, v5 `stop`/`start` | qbit-api | keep |
| T-28 | `test_qbittorrent_v5_2_accepts_204_login` | `QbitClient.login` | 204 empty login accepted; `QBT_SID_*` cookie kept; version endpoint called | Accept 200 `Ok.` and 204; confirm with `app/version`; keep session cookie | qbit-api | keep |
| T-29 | `test_api_failure_is_wrapped` | `QbitClient._request` | Transport error -> `QbitError` | Typed error; caller fails closed (no mutation) | qbit-api | keep |
| T-30 | `test_dry_run_does_not_mutate_files_or_qbittorrent` | `main` | Dry-run: inodes unchanged, no stop/pause/recheck/delete, 1 plan, CSV has auto columns | Dry-run runs the full pipeline (scan, plan, validations) with mutation adapters replaced: no fs mutation in roots, no stop/pause/start/resume/recheck/delete, no ARR/Jellyfin mutators; plan persisted | action-runner, job-runner, qbit-api, arr-api, jellyfin-api | keep (stricter: also `start`/`resume`) |
| T-31 | `test_auto_mode_repairs_proven_duplicate_without_prompt` | `main` | Auto mode consolidates an exact copy, stops + rechecks, writes 2 plans | Auto mode (opt-in, dry-run off) consolidates a proven exact copy only if all auto rules hold (age, allowlisted state with progress 1, clean scan/mapping); two plan phases or equivalent post-repair re-plan | job-runner, plan-engine, action-runner | adapt (CON-21, CON-05) |
| T-32 | `test_automatic_cleanup_removes_only_empty_torrent_without_files` | `execute_deletions`, `find_unused_torrents` | After deleting the only file, torrent removed with `deleteFiles=false` | Task removal only when every wanted file is gone and removal is planned; policy opt-in; allowlisted state | action-runner, qbit-api, plan-engine | adapt (CON-04, CON-05) |

Coverage: 32/32 mapped (keep 26, adapt 4, superseded 1, obsolete 1; T-14 and T-30 are kept with
stricter wording). `unit-tests` owns the matrix updates from wave 3.

---

## 3. Invariants, historical state, one-off decisions

### 3.1 Permanent invariants

Sources: CONTEXTE §0 qualifier "Règle permanente", §1.3, §2.12, §3.x, §6.5, §9; MASTER §2-§8.

| ID | Invariant | Evidence | Legacy tests | Destination |
|---|---|---|---|---|
| INV-01 | Hardlink proof = same `(st_dev, st_ino)` present in both trees; never `st_nlink` alone, never the name. | CONTEXTE §1.2, §1.3.3; MASTER §2; DR-01, DR-05 | T-01, T-02 | fs-engine, issue-engine, domain-model |
| INV-02 | Name + size (and similarity) find candidates; they never prove content equality. | CONTEXTE §1.3.2, §3.3 L2; DR-11 | T-10, T-11 | media-match |
| INV-03 | Content equality = full byte comparison of the single retained pair, reached through the funnel inode -> name+size -> scene parsing -> samples (eliminate only) -> full compare. No whole-NAS SHA-256, no all-pairs comparison. | CONTEXTE §3.3, §4.3; MASTER §2; DR-12, DR-13 | T-11, T-12 | media-match, fs-engine |
| INV-04 | Repair direction: canonical source is the completed file; the physical media copy is replaced by a hardlink to that inode (temp link then atomic rename), verified by `(dev, ino)`. | CONTEXTE §1.3.4, §3.5; MASTER §2; DR-20 | T-13 | action-runner |
| INV-05 | Hardlink permission probe in the target dir precedes any long content read. | CONTEXTE §3.5, §4.5; DR-21 | T-14 | action-runner, fs-engine |
| INV-06 | qBittorrent never deletes data: task removal always `deleteFiles=false`; no internal API can express `true`. | CONTEXTE §1.3.5, §9.8; MASTER §2; API-08 | T-25, T-32 | qbit-api |
| INV-07 | Never read or modify qBittorrent internals (`BT_backup/`, `.torrent`, `.fastresume`). | CONTEXTE §1.3.6; MASTER §2 | — | fs-engine (scope), qbit-api, config-secrets (root validation must refuse a root containing them) |
| INV-08 | Intent journal entry before every mutation (incl. service mutations such as stop), flushed + `fsync` (+ parent dir sync when needed), then result or error. Recovery reads intents without results. | CONTEXTE §1.3.7, §3.13; MASTER §2; `append_jsonl` | T-13, T-22 | action-runner, db-layer |
| INV-09 | Scope = declared roots only (`download_roots`, `media_roots`); no default path; undeclared paths never walked, read or modified; roots validated at startup (exist, distinct, no ambiguous overlap, no escaping symlink); every access confined. | MASTER §2, §4; CONTEXTE §1.3.8 (generalized), §2.3; DR-32 | T-18, T-26 | config-secrets, fs-engine |
| INV-10 | Samples are protected by default (never junk, never orphan deletion); only an explicit instruction targeting the sample may remove it. | CONTEXTE §3.7 "Samples", §5.4; MASTER §2, §5.6 | — (legacy gap, CON-01) | media-match (detection), issue-engine, plan-engine |
| INV-11 | Sonarr/Radarr are the only quality authority (profiles, Custom Formats, scores, cutoff, rejection reasons). No hard-coded codec/group hierarchy. ARR absent or ambiguous = manual decision. | CONTEXTE §1.3.9-11, §1.4, §2.11, §4.12, §9.9; MASTER §2 | T-19 (superseded) | arr-api, plan-engine, issue-engine |
| INV-12 | Technical metadata (codec, resolution, language, group, source, size, ffprobe) is explanatory and may block, never promote a release. | CONTEXTE §1.3.10, §3.3; DR-15, DR-16 | T-06, T-09 | media-match, web-ui |
| INV-13 | Plans are immutable (hash), revalidated just before execution: existence, type, no symlink, confinement, dev, inode, size, `mtime_ns`, mode, folder + annex state, no scan error under target, external service state/config. Any difference invalidates the action; never adapt silently. | CONTEXTE §3.12, §6.2 Planificateur; MASTER §2; DR-17 | T-16 | plan-engine, action-runner |
| INV-14 | qBittorrent transaction: identify every touched hash (several torrents per inode/content), record initial state, stop, mutate, recheck with timeout, resume intact torrents, leave uncertain ones stopped and visible. | CONTEXTE §3.8; MASTER §2; DR-25, DR-30 | T-22, T-23, T-25 | action-runner, qbit-api |
| INV-15 | Fail-closed: incomplete scan, ambiguous mapping, unavailable API, permission denied or `ENOENT` within a perimeter = no automatic mutation in that perimeter. | CONTEXTE §2.12.8, §3.11, §3.15, §4.9; MASTER §2; FH-06, FH-07 | T-29 | job-runner, plan-engine, action-runner, fs-engine |
| INV-16 | Single importer per media: never create hardlinks while Sonarr/Radarr imports the same media; per-media lock. | CONTEXTE §3.9 Concurrence, §4.11 (two-importer race), §6.2 Exécuteur; MASTER §2 | — | action-runner, arr-api, job-runner |
| INV-17 | An action succeeds only after verification (rescan, inode, recheck); otherwise "executed, not verified". | CONTEXTE §6.4; MASTER §2 | T-13 | action-runner, domain-model, reporting |
| INV-18 | Never follow a symlink during scan or deletion. | CONTEXTE §3.2, §6.7; DR-02, DR-19 | T-17 | fs-engine, action-runner |
| INV-19 | A torrent task is removed only if every planned/wanted file has really disappeared and removal is part of the plan. | CONTEXTE §3.8, §6.7; DR-26 | T-23, T-32 | plan-engine, action-runner |
| INV-20 | Recheck timeout does not mean bad data: torrents stay stopped, state becomes manual / re-observe. | CONTEXTE §3.15; FH-11 | — | action-runner |
| INV-21 | Gains: distinguish logical size, unique allocated space, attributable gain, observed free-space delta; count each inode once; consolidations free media blocks only if no other link holds them; unique gain ID per action, aggregates reference IDs (no double counting); "potentially recoverable" is never presented as guaranteed. | CONTEXTE §1.2, §3.2, §3.14, §5.1-5.2; MASTER §3, §5, §6; DR-04, DR-31 | T-02, T-03 | domain-model, reporting, db-layer |
| INV-22 | Strict separation observe / propose / plan / approve / execute / verify / report: a scan never fixes, an ARR search never grabs, a display never deletes. | CONTEXTE §3.1; MASTER intro | T-30 | job-runner, backend-api, arr-api, plan-engine |
| INV-23 | Dry-run is a real execution path using all logic except mutation adapters: no file mutation, no pause/resume/recheck/remove, no ARR grab or mutator. Default on. | CONTEXTE §3.11, §9.5; MASTER §4, §6 | T-30 | action-runner, job-runner, qbit-api, arr-api, jellyfin-api, config-secrets |
| INV-24 | No preselection; bulk actions apply exactly the unit validations; blocked items excluded and listed. Explicit confirmation (target, count, space impact, irreversible effects) before destructive actions. | CONTEXTE §2.7, §3.11; MASTER §6; DR-35 | T-24 | web-ui, backend-api, plan-engine |
| INV-25 | Folder actions are all-or-nothing on the smallest fully-eligible folder, with a preview of every annex (subtitles, NFO, images, samples, subfolders). | CONTEXTE §3.7; MASTER §5, §6; DR-06 | T-15, T-21 | issue-engine, plan-engine, web-ui |
| INV-26 | Never automatic deletion in `media_roots`; media-only files are informational (often normal). | MASTER §5.4; CONTEXTE §2.2, §5.4 | T-08 | issue-engine, plan-engine |
| INV-27 | Junk is junk only if it belongs to no qBittorrent torrent; samples excluded from junk. | MASTER §5.6 | — | issue-engine |
| INV-28 | qBittorrent allowlist (fail-closed): destructive eligibility requires `progress == 1` and state in {`uploading`, `stalledUP`, `queuedUP`, `forcedUP`, `pausedUP`/`stoppedUP`}; every other state protected; handle 4.x and 5.x names. | MASTER §5.7 (deliberate addition, CON-05) | — | qbit-api, plan-engine |
| INV-29 | A completed item is attached to its torrents by content path (`save_path` + `content_path` / file names after mapping), never by torrent name; one row per content with torrent count; actions cover all hashes. | MASTER §5; DR-23 | T-25 | qbit-api, issue-engine, web-ui |
| INV-30 | Path mapping: lexical prefix validation, result confined under its root after resolution, no unbounded string replacement; out-of-root results rejected. | CONTEXTE §2.3; MASTER §4; DR-33 | T-26 | config-secrets, qbit-api, arr-api, jellyfin-api |
| INV-31 | No secret (password, API key, passkey, tracker URL) in logs, reports, HTML, database or exports; tracker URLs redacted. | CONTEXTE §0.2, §6.5; MASTER §6, §8 | — | config-secrets, reporting, web-ui, db-layer |
| INV-32 | No implicit recursive `chmod`/`chown`; permission issues are detected during scan and fixed only through a targeted approved plan. | CONTEXTE §2.4, §3.1; MASTER §5.5 | — | issue-engine, plan-engine, action-runner |
| INV-33 | Application data (SQLite, exports, journal) lives outside all managed roots (config volume). | DR-32; MASTER §3, §8 | T-18 | config-secrets, devops |
| INV-34 | Each ARR-based plan stores an ARR quality snapshot (media id, profile, cutoff, CFs, scores, totals, decision, reasons, capture date); any change invalidates the plan. ARR-rejected candidates are never auto-selected. | CONTEXTE §1.4, §2.11, §3.4; MASTER §5 | — | arr-api, plan-engine, db-layer |
| INV-35 | Season replacement is a global transaction: every episode proven inode by inode before removing the old coverage; net gain = freed minus added. | CONTEXTE §3.6, §4.11 (season H265 replacement); MASTER §5 | — | plan-engine, action-runner, media-match |
| INV-36 | Absolute / aired / TVDB numbering and specials handled by explicit, verifiable mapping. | CONTEXTE §3.6, §4.11 (absolute numbering, specials); MASTER §5 | — | media-match, arr-api |
| INV-37 | Real audio language (ffprobe) may block a replacement; `und` tags require caution. | CONTEXTE §3.3, §4.11 (real audio); MASTER §5 | T-09 (analogous) | media-match, plan-engine |
| INV-38 | ARR read (search) never implies grab; grab, manual import, monitoring change, metadata change, queue removal, profile/CF change are separately authorized capabilities. | CONTEXTE §3.9, §6.6; MASTER §6, §9 | — | arr-api, plan-engine, backend-api |
| INV-39 | An ARR queue alert does not prove a missing hardlink: check inodes first, then parent permissions; never hide an error by removing a queue entry. | CONTEXTE §3.9, §3.15, §4.11 (permissions case); MASTER §6 | — | issue-engine, arr-api |
| INV-40 | Interruption / crash: resume torrents that were not mutated, keep mutated ones stopped until a successful recheck; recovery from the journal. | CONTEXTE §3.13, §3.15; DR-30 | T-22 | action-runner, job-runner |
| INV-41 | `ENOENT` during a scan indicates concurrency: protect the zone, rescan later, never delete on that basis. | CONTEXTE §3.15, §4.9; FH-13 | — | fs-engine, job-runner |
| INV-42 | Every observation carries date, provenance and proof level; historical data is never treated as current without a new scan. | CONTEXTE §0.1; MASTER §9 | — | domain-model, db-layer |
| INV-43 | Hardlinks require the same `st_dev` and the same mount; different devices or `EXDEV` = no consolidation; the container mounts the common parent as one volume. | DR-11, DR-20; MASTER §8 volumes | — | fs-engine, action-runner, devops |
| INV-44 | Filesystem scans are scheduled or manual, never looping; UI refresh never wakes disks; content reads are bounded and limited to retained candidates. | MASTER §3 | — | job-runner, web-ui, fs-engine |
| INV-45 | Similarity score is an identification mechanism only, never a quality score. | CONTEXTE §2.5, §3.11; DR-09 | T-04 | media-match |
| INV-46 | Ignore/exclusion list persisted in the database with reason and date, respected by bulk and automatic modes, restorable. | MASTER §2 anti-pattern, §6 | — | db-layer, backend-api, plan-engine, web-ui |
| INV-47 | Automatic mode is off by default, runs only plans meeting every automatic rule (age, ARR validation, full compare, reliable scan/API/mapping/permissions), stops at the first uncertainty. | CONTEXTE §3.11; MASTER §4, §6 | T-20, T-31 | job-runner, plan-engine, config-secrets |
| INV-48 | A different version cannot be "consolidated"; it is a media replacement governed by ARR. | CONTEXTE §3.6 | — | media-match, plan-engine |
| INV-49 | After deletion, a parent left empty (or containing only annexes listed in the plan) is removed in the same transaction, only under `<completed_root>`. | MASTER §5 | — | action-runner, plan-engine |
| INV-50 | No shell command built by path concatenation (ffprobe and any helper via argv). | CONTEXTE §6.5; API-12 | — | media-match, action-runner |
| INV-51 | Web security and deployment: single-user form auth with Argon2 hash, encrypted HttpOnly SameSite session, CSRF on every mutating request, login rate limit; rootless container, no debug server; CI actions pinned by SHA; images only on `v*.*.*` tags. | MASTER §2 anti-patterns, §8; CONTEXTE §6.5 | — | auth-guard, devops, backend-api |

### 3.2 Historical state (never import as current — MASTER §9)

| ID | Historical fact | Source | Handling |
|---|---|---|---|
| HIS-01 | Library around 20 TiB; historical absolute NAS paths. | CONTEXTE §1.1, §2.2 | Replaced by configured roots (CON-08). |
| HIS-02 | Historical service identity UID:GID (CONTEXTE §2.4) and a second legacy owner that broke renames. | CONTEXTE §2.4 | Replaced by `expected_owner` (CON-09). |
| HIS-03 | Gains per batch (Oct–Nov 2026) and conversational totals. | CONTEXTE §5.2; rapport-final | Not seeded into the DB; gain ledger starts empty. |
| HIS-04 | Inventory evolution: completed-only MKV down to 10 samples (811 MiB); 243 media-only MKV (409 GiB), 240 linked to ARR, 3 unmanaged. | CONTEXTE §5.3-5.4; rapport-final | Must be re-observed by a scan. |
| HIS-05 | qBittorrent 5.3.0, `stoppedUP` 0, `missingFiles` 0, 62 seeds resumed; incomplete empty. | CONTEXTE §5.4 | Only proves 5.x support is needed. |
| HIS-06 | ARR queues empty; Sonarr 4.0.20 error with `skipRedownload=true` on unknown series. | rapport-final | arr-api should surface queue-removal errors and never retry blindly (CTX-08). |
| HIS-07 | cross-seed indexing ~3072 torrents, quarantined hybrid torrents, 518 MiB diagnostic backup. | CONTEXTE §5.4 | Out of scope (MASTER §9). |
| HIS-08 | Docker on another volume; Btrfs snapshot usage unknown. | CONTEXTE §5.4 | Explains why observed gain may differ (INV-21). |
| HIS-09 | Secrets once visible in diagnostics; rotation recommended. | CONTEXTE §0.2, §5.5 | INV-31; nothing to import. |
| HIS-10 | Remaining actions §5.5 (ARR analysis of media-only files, unmanaged media, snapshot check, secret rotation, log consolidation). | CONTEXTE §5.5 | User tasks; the app provides the tools (issue-engine, arr-api), not the outcomes. |

### 3.3 One-off decisions and the lesson kept

| ID | Decision (CONTEXTE §4.11 / rapport-final) | Lesson kept as | Destination |
|---|---|---|---|
| ONE-01 | Absolute-numbered anime pack remapped to TVDB numbering. | INV-36 | media-match, arr-api |
| ONE-02 | Correctly hardlinked season blocked by a parent dir owned by another UID. | INV-39, permission issue category (MASTER §5.5) | issue-engine |
| ONE-03 | Two importers raced, creating redundant names; specials remapped. | INV-16, INV-36 | action-runner, media-match |
| ONE-04 | Netflix "parts" vs ARR seasons reorganized manually. | Canonical model from ARR ids; protect manual organization (targeted monitoring disable, CTX-06) | arr-api, jellyfin-api |
| ONE-05 | Missing torrent source restored from the library by hardlink, then recheck. | Reverse restore Jellyfin -> qBittorrent source (CTX-04) | plan-engine, action-runner |
| ONE-06 | French 720p kept over English-only 1080p after ffprobe. | INV-37 | media-match |
| ONE-07 | Season H264 -> H265 replacement proven per episode before removal. | INV-35 | plan-engine |
| ONE-08 | Specific exact-copy pairs consolidated; a targeted single-movie removal. | Exact copies are the most automatable case (tier 2) | action-runner |
| ONE-09 | Hybrid torrents refused by the client; quarantine instead of retry loop. | REJ-21 (no retry loops); out of scope (cross-seed) | — |
| ONE-10 | Specific releases deleted / kept by user choice (rapport-final "Suppressions validées"). | Manual decisions only; never a rule | — |
| ONE-11 | 10 samples kept on explicit request. | Generalized into INV-10 by the user | media-match, issue-engine |
| ONE-12 | Historical codec preference and group penalties. | Expressed in ARR Custom Formats, never in code (INV-11, REJ-09, REJ-10) | arr-api |
| ONE-13 | Sonarr queue removal with `skipRedownload=false` workaround. | Version-specific; not a rule (HIS-06) | — |

---

## 4. Rejected approaches (never reintroduce)

| ID | Rejected approach | Source | Guarded by |
|---|---|---|---|
| REJ-01 | `st_nlink == 2` (or `== 1`) as proof of import / non-import. | CONTEXTE §2.12.1, §4.1; MASTER §2 anti-pattern | INV-01; fs-engine, issue-engine |
| REJ-02 | Full SHA-256 of every NAS file. | CONTEXTE §2.12.2; MASTER §2 | INV-03; media-match |
| REJ-03 | Full comparison of every file against every file. | CONTEXTE §2.12.3, §4.3 | INV-03; media-match |
| REJ-04 | Same name + same size treated as certain equality. | CONTEXTE §2.12.4 | INV-02; media-match |
| REJ-05 | Manually removing `BT_backup` files. | CONTEXTE §2.12.5 | INV-07 |
| REJ-06 | qBittorrent `deleteFiles=true` / torrent client `delete_files` / `delete_data`. | CONTEXTE §2.12.6; MASTER §2 anti-pattern | INV-06; qbit-api |
| REJ-07 | Automatically deleting an orphan without a reliable library replacement. | CONTEXTE §2.12.7 | INV-11, INV-26; plan-engine |
| REJ-08 | Continuing automatic mode when scan, mapping or an API is incomplete. | CONTEXTE §2.12.8 | INV-15, INV-47; job-runner |
| REJ-09 | Hard-coding `AV1 > H265 > H264` (any codec hierarchy). | CONTEXTE §2.12.9; MASTER §2 | INV-11; plan-engine, media-match |
| REJ-10 | Release-group penalties outside ARR Custom Formats. | CONTEXTE §2.12.10 | INV-11 |
| REJ-11 | Imposing a dedicated cross-seed links directory. | CONTEXTE §2.12.11 | Out of scope (MASTER §9) |
| REJ-12 | Abandoning strict cross-seed matching without need. | CONTEXTE §2.12.12 | Out of scope (MASTER §9) |
| REJ-13 | Modifying cross-seed configuration without consent. | CONTEXTE §2.12.13 | Out of scope; the app has no cross-seed integration |
| REJ-14 | Destructive HTTP route without authentication, CSRF and approved plan. | MASTER §2 anti-pattern | INV-51; auth-guard, backend-api |
| REJ-15 | Reading Sonarr/Radarr SQLite databases directly. | MASTER §9; CONTEXTE §2.10; `generate_final_audit.load_arr` | arr-api (REST only) |
| REJ-16 | Exclusion list stored only in the browser (`localStorage`). | MASTER §2 anti-pattern | INV-46; db-layer |
| REJ-17 | Hard-coded root paths, debug server, container running as root, PUID/PGID privilege-dropping entrypoint. | MASTER §2, §8; legacy `__main__` | INV-09, INV-51; config-secrets, devops |
| REJ-18 | Publishing an image on every main-branch push; unpinned CI actions. | MASTER §2, §8 | devops |
| REJ-19 | Comparing content before testing hardlink permission. | CONTEXTE §4.5 | INV-05 |
| REJ-20 | Using `sudo` / running privileged as a fix for ownership/ACL problems. | CONTEXTE §4.5, §6.5; MASTER §8 | CON-30; devops, action-runner |
| REJ-21 | Retry loops on torrent injection failures (quarantine instead). | CONTEXTE §4.11 | qbit-api (no blind retries of mutating calls) |
| REJ-22 | Masking an error by removing an ARR queue entry. | CONTEXTE §3.15 | INV-39; arr-api |
| REJ-23 | Recursive `chmod`/`chown` without a precise approved plan. | CONTEXTE §2.4; MASTER §5.5 | INV-32 |
| REJ-24 | Local quality criteria (resolution rank, language preference) deciding automatic deletion. | CONTEXTE §4.8 | INV-11; CON-02 |
| REJ-25 | Two concurrent importers for the same media. | CONTEXTE §3.9, §4.11 | INV-16 |
| REJ-26 | Interpreting an empty qBittorrent login body as refusal. | CONTEXTE §4.6; API-01 | T-28; qbit-api |
| REJ-27 | Summing session gains across report files (cumulative counter). | CONTEXTE §5.1; `cumulative_gain` | INV-21; CON-13 |
| REJ-28 | Unbounded path prefix replacement, or passing an unmapped path through unchanged. | CONTEXTE §2.3; `generate_final_audit.host_path` | INV-30; CON-18 |
| REJ-29 | Silently skipping torrents whose paths cannot be mapped. | `build_qbit_index` | CON-07 |
| REJ-30 | Importing old JSON/CSV reports as current state. | MASTER §9; CONTEXTE §0.1 | INV-42 |

---

## 5. Contradictions, legacy defects and deliberate changes

Type: **C** = contradiction between sources; **D** = legacy defect (do not port);
**Δ** = deliberate MASTER change of legacy behavior. Resolution rule: CONTEXTE wins over MASTER
on business rules; MASTER wins over agent files; legacy code wins only for behavior not ruled by
CONTEXTE/MASTER.

| ID | Type | Topic | Sources in tension | Recommended resolution | Owner |
|---|---|---|---|---|---|
| CON-01 | D | Samples not protected by the legacy engine | CONTEXTE §3.7 permanent rule, MASTER §2 vs `build_deletion_candidates` (no sample logic; on the fixture, the legacy auto mode marks a sample folder eligible for deletion, see §7.6) | Implement sample detection and protection (INV-10). Define the detection rule (OQ-01). | media-match, issue-engine |
| CON-02 | C | Local quality policy in auto mode | `auto_match_rejection` (`RESOLUTION_RANK`, language compatibility) and T-19 vs CONTEXTE §1.3.9-10, §4.8, §2.12.9; MASTER §2 | CONTEXTE wins: automatic replacement requires an ARR snapshot admitting the candidate as an improvement. Resolution/language/edition/duration stay as *blockers* only. T-19 superseded. | plan-engine, arr-api |
| CON-03 | C | Default mode | Legacy and CONTEXTE §3.11: `interactive` default; MASTER §4: `dry_run=true`, `auto_mode=false` | Compatible: interactive approval flow is the product; `dry_run` is a global safety switch on by default; auto off. | config-secrets, job-runner |
| CON-04 | Δ | Empty-torrent removal default | `auto_remove_empty_torrents=True` vs CONTEXTE §3.11 "only if the policy is enabled", MASTER auto off | Separate opt-in policy, default off; always `deleteFiles=false`. | config-secrets, plan-engine |
| CON-05 | Δ | qBittorrent state allowlist | Legacy only checks paused/stopped for resume; MASTER §5.7 allowlist with `progress == 1` | Adopt MASTER allowlist (INV-28); fake qBittorrent in tests must expose `progress`. | qbit-api, plan-engine |
| CON-06 | C | Stagnant incomplete torrents vs allowlist | MASTER §5.1 (remove task + delete data for stagnant incomplete) vs MASTER §5.7 (destructive only if `progress == 1` and UP state) | Architectural decision required (OQ-06). Recommendation: allowlist governs completed data; stagnant-incomplete cleanup is a separate manual-only action type (age threshold, torrent stopped first, task removed with `deleteFiles=false`, then data under `<incomplete_root>` deleted by the app after revalidation), never in auto mode. | plan-engine, qbit-api |
| CON-07 | D | Unmappable torrents silently skipped | `build_qbit_index` (`except QbitError: continue`), `normpath` lets `..` escape then skip, vs fail-closed (MASTER §2, CONTEXTE §3.8 "signaler toute erreur de mapping") | Torrents whose path matches no configured mapping are recorded as explicit out-of-scope torrents (visible, informational). A path that matches a mapping prefix but escapes it (`..`, symlink) is a mapping error that blocks automation (OQ-07). | qbit-api, plan-engine |
| CON-08 | Δ | Excluded personal directory / hard-coded roots | CONTEXTE §1.3.8, §2.2, §3.2, §6.7, §9.7 vs MASTER §2, §4 | Rule becomes "only declared roots are touched" (INV-09); the fixture's `<undeclared_sibling>` replaces the historical exclusion test. CONTEXTE intent is preserved by construction. | config-secrets, fs-engine |
| CON-09 | Δ | Expected owner | CONTEXTE §2.4 historical UID:GID vs MASTER example `1000:1000` | Configurable `expected_owner`; no hard-coded identity (OQ-09 for the default). | config-secrets, issue-engine |
| CON-10 | D | Interactive mode partially fail-open on scan errors | `main` blocks only media errors in interactive; `error_below` guards folders but file candidates near errors and videos of folders whose snapshot failed are still proposed | CONTEXTE §3.12 lists "no scan error under the target" for every action: block any action whose target, subtree or ancestors carry a scan error; auto blocked per INV-15 (OQ-05 for perimeter size). | issue-engine, plan-engine |
| CON-11 | D | Torrents stopped before any intent is journaled | `execute_*` call `client.stop` before `deletion_batch_intent` / `hardlink_intent` vs INV-08 | Journal an intent for the stop step too; revalidate before stopping (avoid useless service mutations), then again after stopping. | action-runner |
| CON-12 | D | Weak recheck verification | `wait_for_recheck` succeeds 2 s after start without seeing `checking*` and never checks `progress`/state | Verified only when the recheck is observed finished and the torrent reports `progress == 1` in an allowlisted state; otherwise "executed, not verified", torrent stays stopped. | qbit-api, action-runner |
| CON-13 | D | Cumulative gain double counting | `cumulative_gain` sums every `session_summary` vs CONTEXTE §5.1-5.2 | Gain ledger with unique gain IDs; aggregates reference IDs (INV-21). | reporting, db-layer |
| CON-14 | C | `nlink` in revalidation | Legacy deletion signature includes `nlink`; MASTER §2 / CONTEXTE §3.12 lists omit it | Keep `nlink` for deletion plans (it changes attributable gain, stricter); exclude it for links created by the same consolidation batch (legacy `repair_record_still_matches`). | plan-engine, action-runner |
| CON-15 | C | Consolidation step order | CONTEXTE §3.5 (plan fsync -> permission test -> stop -> revalidate -> compare -> intent -> temp link -> atomic replace -> verify -> recheck -> restore -> rescan) vs legacy (stop -> revalidate -> intent -> permission test -> compare -> link) | Follow CONTEXTE §3.5 with INV-08 (intent before *each* mutation, including stop). Permission test before stop and before compare. | action-runner |
| CON-16 | C | Name normalization and similar-name search default | Legacy key = case-folded filename + size, similar-name search off in `__main__`; MASTER funnel: normalized name + size, then parsing + samples | Name+size key keeps legacy case-folding (parity) plus Unicode NFKD normalization; parsing + samples stage enabled by default with bounded I/O. On the fixture this adds one consolidation the legacy default misses (§7.6). | media-match |
| CON-17 | C | ARR data source | `generate_final_audit` reads ARR SQLite vs MASTER §9 | REST APIs only (REJ-15). | arr-api |
| CON-18 | D | Final-audit path mapping and silent errors | `host_path` returns unmatched paths unchanged; `qbit_file_index` swallows `QbitError`; password read from script literals | Explicit per-service mappings, unmatched = error (INV-30); secrets only from env (INV-31). | config-secrets, qbit-api |
| CON-19 | C | Report divergence on ARR import alerts | rapport-final: alerts were not permission problems; CONTEXTE §4.11: one case was a parent-dir ownership problem | Both historical and compatible: always check inodes first, then parent dir permissions (INV-39). | issue-engine |
| CON-20 | C | Dry-run requires qBittorrent | Legacy dry-run exits 5/8 when qBittorrent is down or mapping fails vs CONTEXTE §3.15 (read-only scan allowed, qualified incomplete) | Dry-run and scans still run; plan items depending on qBittorrent are marked blocked "service unavailable"; no mutation possible anyway. | job-runner, plan-engine |
| CON-21 | C | Auto consolidations not age-gated | Legacy auto repairs ignore `auto_min_age_hours` vs MASTER §6 auto rules (minimal age) | Apply the minimal age to every automatic plan, consolidations included (stricter; OQ-10). T-31 fixture must age files. | plan-engine |
| CON-22 | C | Folder deletion containing a sample | MASTER §6 annex preview "incl. samples" vs INV-10 | A folder plan containing a sample requires an explicit per-sample acknowledgement in the approved plan; never in auto mode (OQ-11). | plan-engine, web-ui |
| CON-23 | D | Release folder split | Legacy leaf-most rule splits a release folder with a `Sample/` subfolder into "main file" + "Sample folder" candidates (fixture §7.6) vs MASTER §5 release-folder unit | Release folder (contains `Sample`/`Subs`/`Extras` or several videos) is proposed as one unit; samples inside follow CON-22. | issue-engine |
| CON-24 | C | Deletions inside `media_roots` | CONTEXTE §2.2 allows planned media replacement; MASTER §5.4 "never automatic deletion in media_roots"; CONTEXTE §3.11 auto deletes only under completed | Removal of an old media release only inside a manually approved ARR replacement plan; never automatic (OQ-12). | plan-engine |
| CON-25 | D | Conflicting repair proposals | `discover_hardlink_repairs` emits one repair per media match; one media file can be the target of several completed copies | plan-engine rejects plans with duplicate targets or sources; at most one consolidation per media target per plan. | plan-engine |
| CON-26 | C | Batch continues after an item failure | FH-10 vs MASTER §6 "auto mode stops at the slightest uncertainty" | Auto: stop the batch at the first failure (affected torrents stay stopped). Manual: independent items may continue; every failure leaves its hashes stopped and reported. | action-runner, job-runner |
| CON-27 | D | Confinement via path resolution (TOCTOU) | `ensure_delete_target` resolves then unlinks by path | Directory-handle relative operations without following symlinks (e.g. `openat`/`unlinkat` with no-follow semantics); recheck identity at the handle. | fs-engine, action-runner |
| CON-28 | C | Dry-run writes files | Legacy dry-run writes reports into the report dir | Allowed: dry-run persists plans/reports in `<app_data>` (DB), never inside roots. | job-runner, reporting |
| CON-29 | C | Importing old reports | CONTEXTE §7.2.3 (import as history) vs MASTER §9 (not as current state) | Not in V1; if ever added, history-only with dates (INV-42). | db-layer |
| CON-30 | C | Privilege elevation | CONTEXTE §6.5 "targeted elevation", legacy EPERM message suggests `sudo` vs MASTER §8 (no root process at all) | MASTER stricter: no elevation; error messages explain ownership/ACL alignment, never suggest `sudo`. | action-runner, devops |
| CON-31 | C | Video extensions | Legacy `{.mkv}`; MASTER §4 configurable | Configurable list; default to be decided (OQ-13); parity fixture uses `.mkv`. | config-secrets |
| CON-32 | D | Silent ignore of invalid selection IDs | `parse_id_expression` drops unknown IDs | Backend rejects unknown or stale item IDs (stricter, INV-24). | backend-api |

---

## 6. Scope without legacy equivalent

### 6.1 MASTER requirements with no legacy code (`NEW-xx`)

| ID | Requirement | MASTER section | Owner(s) |
|---|---|---|---|
| NEW-01 | Axum + Tokio backend, Askama + HTMX server-rendered UI, only embedded JS = HTMX | §3 | backend-api, web-ui |
| NEW-02 | Server-side SVG charts (gains potential/expected/attributable/observed over time) | §3, §6 | reporting, web-ui |
| NEW-03 | SQLite via sqlx on the config volume, WAL, batched commits; first-seen dates, status, hardlink state, sessions, evidence, plans, journal, ARR/qBittorrent snapshots, unique gain IDs | §3 | db-layer |
| NEW-04 | Persistent job queue, background workers, safe cancellation, resume after restart | §3 | job-runner |
| NEW-05 | Scheduled scans (interval/cron), manual triggers, never looping; healthcheck that does not wake disks | §3, §8 | job-runner, devops |
| NEW-06 | Incomplete tracking driven by qBittorrent API (`added_on`, state, progress), stagnation threshold, incomplete orphans | §3, §5.1 | qbit-api, issue-engine |
| NEW-07 | Typed roots model, path mappings per service (qBittorrent, Sonarr, Radarr, Jellyfin), thresholds, `expected_owner`, `dry_run`, `auto_mode` | §4 | config-secrets |
| NEW-08 | Issue model: category, severity, evidence + proof level, provenance, observation date, age, blockers | §5 | domain-model, issue-engine |
| NEW-09 | Permissions & owner category (owner mismatch, non-writable parent dir) | §5.5 | fs-engine, issue-engine |
| NEW-10 | Junk category (archives, NFO, TXT, orphan subtitles) outside torrents | §5.6 | issue-engine |
| NEW-11 | qBittorrent allowlist with 4.x/5.x names | §5.7 | qbit-api |
| NEW-12 | One row per content with torrent count; release-folder units; parent cleanup | §5 | issue-engine, plan-engine, web-ui |
| NEW-13 | ARR quality snapshots (profile, CFs, scores, cutoff, reasons) recorded in plans and invalidating them | §5 | arr-api, plan-engine |
| NEW-14 | ffprobe tracks/real audio language as explanation and blocker; numbering mapping; season coverage proof | §5 | media-match |
| NEW-15 | Jellyfin API: health, targeted refresh, recognition check | §4, §6 | jellyfin-api |
| NEW-16 | Responsive mobile-first UI: dashboard (counts per category, potentially recoverable space, hardlink-shared space, service health/version, last scan freshness, running jobs, color + non-color urgency) | §6 | web-ui, ux-design |
| NEW-17 | Tabs per issue type with filters, search, evidence, blockers; list columns name/age/size/torrent count; sort; list and selection totals; select-all without preselection; hide-recent filter; refresh | §6 | web-ui, backend-api |
| NEW-18 | Ignore list ("Ignored" tab, reason, restore), respected by auto mode | §6 | db-layer, backend-api, web-ui |
| NEW-19 | History & statistics tab; logs tab and stdout logging with configurable level, no secrets | §6 | reporting, web-ui |
| NEW-20 | Unit and bulk actions with identical validations; confirmation modal; force-rescan buttons (qBittorrent, Radarr, Sonarr, Jellyfin) | §6 | backend-api, web-ui |
| NEW-21 | i18n: English default, French, `locales/en.json` and `locales/fr.json` embedded, English fallback | §7 | web-ui |
| NEW-22 | Single-user auth (Argon2 hash in env, session key), encrypted session, CSRF, login rate limit | §8 | auth-guard |
| NEW-23 | Rootless multi-stage image (static musl binary + minimal runtime with ffprobe), multi-arch amd64/arm64, `user:` in compose, single common-parent volume, separate config volume, port bound to 127.0.0.1 | §8 | devops |
| NEW-24 | Deliverables: `.env.example`, commented `docker-compose.yml`, README, issue templates | §8 | devops |
| NEW-25 | CI: fmt, clippy `-D warnings`, tests, build on PR/main; GHCR publish only on `v*.*.*`; pinned actions, minimal permissions | §8 | devops |
| NEW-26 | Risk tiers 1-5 with crash-point and dry-run-no-mutation tests per tier | §10 | plan-engine, action-runner, integration-tests |

### 6.2 CONTEXTE requirements not explicit in MASTER (`CTX-xx`) — proposed owners

| ID | Requirement | CONTEXTE | Proposed owner |
|---|---|---|---|
| CTX-01 | Storage explorer: search by path, title, episode, inode or hash; all paths of an inode; physical copy vs hardlink; provenance | §6.2 | backend-api, web-ui |
| CTX-02 | Dashboard extras: capacity/used/free, logical vs allocated per root, ARR queues, problematic qBittorrent states | §6.2 | reporting, web-ui |
| CTX-03 | Observability metrics: scan duration, compare throughput, errors per root/API, recheck time, expired plans, compensated actions, torrents left stopped, unverified imports, ARR config changes | §7.3 | job-runner, reporting |
| CTX-04 | Reverse restore library -> qBittorrent source by hardlink (prove size, content, expected path) then recheck | §4.11 | plan-engine, action-runner (tier 2 or later; OQ-14) |
| CTX-05 | Stale ARR queue entry removal without deleting media (separately authorized) | §3.9 | arr-api |
| CTX-06 | Targeted disabling of ARR monitoring / Jellyfin metadata updates to protect manual organization | §3.10 | arr-api, jellyfin-api (manual action) |
| CTX-07 | Plan expiration date; policies/exceptions with author, reason, date | §6.2, §6.3 | plan-engine, db-layer |
| CTX-08 | Connector tests separated: reachability, auth, version, list, mapping | §4.6 | qbit-api, arr-api, jellyfin-api |
| CTX-09 | Report exports: human Markdown/HTML, JSON/CSV, inode-by-inode proof, plan vs result differences, remaining actions, secret-free | §6.2 | reporting |
| CTX-10 | Backup of a service configuration before modifying it | §6.5 | arr-api (only if config mutation is ever allowed) |
| CTX-11 | Read-only mode usable without mutation rights; minimal permissions per connector; timeouts and size limits | §6.5 | config-secrets, all connectors |
| CTX-12 | Candidate state machine observed -> identified -> (ambiguous/manual | candidate) -> rejected/planned -> expired/approved -> executing -> failed compensated / failed to review / executed -> verified -> closed | §6.4 | domain-model |

---

## 7. Synthetic parity fixture specification

Built later by `integration-tests` (wave 15); `fs-engine`, `media-match`, `issue-engine` and
`unit-tests` may reuse it earlier. Expected values in 7.6 are derived by hand from the legacy
code paths cited; 7.7 lists the proposed Rust expectations where behavior deliberately differs.

### 7.1 Build requirements

- One temporary directory `<fixture>` on a single filesystem (hardlinks need one `st_dev`); run as
  a **non-root** user (permission anomalies must produce `EACCES`).
- Roots: `<completed_root>` = `<fixture>/completed`, `<incomplete_root>` = `<fixture>/incomplete`,
  `<media_root>` = `<fixture>/medias`. Undeclared sibling: `<fixture>/outside`. App data:
  `<fixture>/appdata` (legacy report dir `<fixture>/appdata/reports`).
- qBittorrent path mapping: container `/data/completed` -> `<completed_root>`, `/data/incomplete`
  -> `<incomplete_root>` (legacy equivalent: container `/data` -> host `<fixture>`).
- Content generator: `bytes(label, n)` = `n` deterministic bytes, e.g. byte `i` = `(seed(label) + 31*i) mod 251`
  with distinct seeds per label; "copy" means byte-identical. Sizes are multiples of 4096 except
  annexes, but **allocated bytes are filesystem-dependent**: expectations are expressed as inode sets
  (`alloc(I)` = `st_blocks*512` of inode `I`).
- Timestamps: after creating everything, set every file, directory **and symlink** (no-follow
  `utimensat`) mtime to `now - 72 h`, deepest first, except FX-12 (`mtime = now`). Creating children
  updates directory mtimes, so directories must be stamped last.
- Permissions: FX-33's parent directory is `chmod 0555` last; tests must restore `0755` before
  cleanup.
- Matching configuration for parity runs: video extensions `{.mkv}`, similarity 0.60, auto
  threshold 0.90, min age 48 h, `quick_sample_size = 4096`, `quick_sample_count = 5`, chunk 64 KiB,
  `expected_owner = <creator uid>:<creator gid>`, incomplete stagnation threshold 14 days.

### 7.2 Tree

`I_x` names an inode. `->` = symlink target (relative). `==` = hardlink (same inode).

| ID | Path (under `<fixture>`) | Kind | Size | Content / inode | Purpose |
|---|---|---|---|---|---|
| FX-01 | `completed/Alpha.Movie.2020.1080p.WEB.x264-GRP/Alpha.Movie.2020.1080p.WEB.x264-GRP.mkv` | file | 65536 | `A`, I_A | Real hardlink (healthy) |
| FX-02 | `completed/Alpha.Movie.2020.1080p.WEB.x264-GRP/Alpha.Movie.2020.1080p.WEB.x264-GRP.nfo` | file | 512 | I_nfoA | Junk-like file **inside** a torrent |
| FX-03 | `medias/movies/Alpha Movie (2020)/Alpha Movie (2020).mkv` | file | 65536 | == FX-01 (I_A) | Hardlink in library |
| FX-04 | `completed/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv` | file | 81920 | `B`, I_B1 | Exact copy, same name; shared by 2 torrents |
| FX-05 | `medias/movies/Bravo Movie (2021)/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv` | file | 81920 | copy of `B`, I_B2 | Exact copy target |
| FX-06 | `completed/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv` | file | 98304 | `C`, I_C1 | Exact copy, different name |
| FX-07 | `completed/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv` | file | 8192 | `Cs`, I_Cs | Sample in `Sample/` subfolder |
| FX-08 | `completed/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.nfo` | file | 512 | I_nfoC | Annex inside torrent |
| FX-09 | `medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv` | file | 98304 | copy of `C`, I_C2 | Renamed exact copy |
| FX-10 | `completed/Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv` | file | 40960 | `D1`, I_D1 | Near-duplicate (other release) |
| FX-11 | `medias/movies/Delta Movie (2018)/Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv` | file | 81920 | `D2`, I_D2 | Library version of the same movie |
| FX-12 | `completed/Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv` | file | 32768 | `E`, I_E; **mtime = now** | Orphan, no torrent, no match, recent |
| FX-13 | `completed/Foxtrot.Show.S01.1080p.WEB.x264-GRP/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv` | file | 49152 | `F1`, I_F1 | Hardlinked episode |
| FX-14 | `completed/Foxtrot.Show.S01.1080p.WEB.x264-GRP/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv` | file | 49152 | `F2`, I_F2 | Unlinked episode in partially linked pack |
| FX-15 | `medias/tv/Foxtrot Show/Season 01/Foxtrot Show - S01E01 - Pilot.mkv` | file | 49152 | == FX-13 (I_F1) | |
| FX-16 | `completed/Golf.Movie.2016.1080p.WEB.x264-GRP.mkv` | file | 16384 | `G`, I_G (nlink 2) | `nlink=2` with external link |
| FX-17 | `outside/golf-external-link.mkv` | file | 16384 | == FX-16 (I_G) | External link in undeclared sibling |
| FX-18 | `outside/Hotel.Movie.2015.1080p.WEB.x264-GRP.mkv` | file | 16384 | `H`, I_H (nlink 2) | Never scanned |
| FX-19 | `outside/notes.txt` | file | 64 | I_notes | Symlink target outside roots |
| FX-20 | `medias/movies/Hotel Movie (2015)/Hotel Movie (2015).mkv` | file | 16384 | == FX-18 (I_H) | Media-only file with external link |
| FX-21 | `completed/India.Movie.2014.1080p.WEB.x264-GRP/India.Movie.2014.1080p.WEB.x264-GRP.mkv` | file | 16384 | `I`, I_I | Orphan folder unit |
| FX-22 | `completed/India.Movie.2014.1080p.WEB.x264-GRP/link-to-outside.txt` | symlink | — | `-> ../../outside/notes.txt` | File symlink escaping roots |
| FX-23 | `completed/escape` | symlink | — | `-> ../outside` | Directory symlink escaping roots |
| FX-24 | `medias/movies/Juliet Movie (2013)/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv` | symlink | — | `-> ../../../completed/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv` | Symlink is not a hardlink |
| FX-25 | `completed/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv` | file | 16384 | `J`, I_J | Source of FX-24 |
| FX-26 | `completed/Kilo.Movie.2012.1080p.BluRay.x264-GRP/Kilo.Movie.2012.1080p.BluRay.x264-GRP.mkv` | file | 65536 | `K`, I_K | Hardlinked main file |
| FX-27 | `completed/Kilo.Movie.2012.1080p.BluRay.x264-GRP/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv` | file | 8192 | `Ks`, I_Ks | File-level sample |
| FX-28 | `medias/movies/Kilo Movie (2012)/Kilo Movie (2012) Bluray-1080p.mkv` | file | 65536 | == FX-26 (I_K) | |
| FX-29 | `completed/stray/readme.txt` | file | 128 | I_txt | Junk outside torrents |
| FX-30 | `completed/stray/archive.rar` | file | 4096 | I_rar | Junk outside torrents |
| FX-31 | `completed/stray/Unrelated.Film.2010.srt` | file | 256 | I_srt | Orphan subtitle (no video with that base name) |
| FX-32 | `completed/November.Movie.2010.1080p.WEB.x264-GRP.mkv` | file | 32768 | `N`, I_N1 | Exact copy whose target dir is not writable |
| FX-33 | `medias/movies/November Movie (2010)/November.Movie.2010.1080p.WEB.x264-GRP.mkv` | file | 32768 | copy of `N`, I_N2; parent dir mode `0555` | Permission anomaly |
| FX-34 | `completed/Tango.Movie.2007.1080p.WEB.x264-GRP.mkv` | file | 24576 | `T`, I_T (nlink 2) | Intra-completed hardlink |
| FX-35 | `completed/tango-dupe/Tango.Movie.2007.1080p.WEB.x264-GRP.mkv` | file | 24576 | == FX-34 (I_T) | Second path of the same inode, no torrent |
| FX-36 | `incomplete/Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv` | file | 8192 | I_O | Stagnant incomplete download |
| FX-37 | `incomplete/abandoned.partial.mkv` | file | 4096 | I_ab | Incomplete orphan |

Directories (all `0755` except FX-33's parent): completed: `Alpha…-GRP/`, `Charlie…-GRP/`,
`Charlie…-GRP/Sample/`, `Foxtrot…-GRP/`, `India…-GRP/`, `Kilo…-GRP/`, `stray/`, `tango-dupe/` (8);
medias: `movies/`, `movies/{Alpha,Bravo,Charlie,Delta,Hotel,Juliet,Kilo,November} …/` (8),
`tv/`, `tv/Foxtrot Show/`, `tv/Foxtrot Show/Season 01/` (12 total).

### 7.3 Simulated qBittorrent (loopback fake, version `v5.0.4` unless a variant says otherwise)

Hashes are 40 repeated hex digits (`H1` = `1111…1`, etc.). Every torrent exposes `hash`, `name`,
`state`, `progress`, `save_path`, `content_path`, `added_on` (now - 10 days unless stated),
`category`; files expose `name`, `size`, `priority` (1), `progress`.

| Torrent | Hash | State | Progress | `save_path` | `content_path` | Files (`name`) |
|---|---|---|---|---|---|---|
| T_A | H1 | `uploading` | 1 | `/data/completed` | `/data/completed/Alpha.Movie.2020.1080p.WEB.x264-GRP` | `Alpha…-GRP/Alpha…-GRP.mkv`, `Alpha…-GRP/Alpha…-GRP.nfo` |
| T_B1 | H2 | `stalledUP` | 1 | `/data/completed` | `/data/completed/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv` | `Bravo…-GRP.mkv` |
| T_B2 | H3 | `pausedUP` (4.x name) | 1 | `/data/completed` | same as T_B1 | `Bravo…-GRP.mkv` |
| T_C | H4 | `uploading` | 1 | `/data/completed` | `/data/completed/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP` | main `.mkv`, `Sample/…sample.mkv`, `.nfo` |
| T_D | H5 | `stoppedUP` (5.x name) | 1 | `/data/completed` | `/data/completed/Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv` | `Delta…-OLD.mkv` |
| T_F | H6 | `queuedUP` | 1 | `/data/completed` | `/data/completed/Foxtrot.Show.S01.1080p.WEB.x264-GRP` | `…/…S01E01….mkv`, `…/…S01E02….mkv` |
| T_J | H7 | `checkingUP` | 1 | `/data/completed` | `/data/completed/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv` | `Juliet….mkv` |
| T_K | H8 | `forcedUP` | 1 | `/data/completed` | `/data/completed/Kilo.Movie.2012.1080p.BluRay.x264-GRP` | main `.mkv`, `…-sample.mkv` |
| T_N | H9 | `uploading` | 1 | `/data/completed` | `/data/completed/November.Movie.2010.1080p.WEB.x264-GRP.mkv` | `November….mkv` |
| T_T | Ha | `uploading` | 1 | `/data/completed` | `/data/completed/Tango.Movie.2007.1080p.WEB.x264-GRP.mkv` | `Tango….mkv` (root copy FX-34 only) |
| T_P | Hb | `missingFiles` | 1 | `/data/completed` | `/data/completed/Papa.Movie.2008.1080p.WEB.x264-GRP.mkv` (absent on disk) | `Papa….mkv` |
| T_Q | Hc | `uploading` | 1 | `/srv/other` | `/srv/other/Quebec.Movie.2006.mkv` | `Quebec.Movie.2006.mkv` |
| T_O | Hd | `stalledDL` | 0.4 | `/data/incomplete` | `/data/incomplete/Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv` | `Oscar….mkv`; `added_on` = now - 30 days |

No torrent references FX-12, FX-16, FX-21, FX-22, FX-29..31, FX-35, FX-37.

### 7.4 Variants

| ID | Change from base | Legacy expectation | Rust expectation |
|---|---|---|---|
| V1 | Base fixture | §7.6 | §7.7 |
| V2 | Add `completed/Lima.Movie.2011.1080p.WEB.x264-GRP/Lima….mkv`, then `chmod 0000` the directory | 1 completed scan error; reports still written; dry-run/auto exit 7 with `automation_blocked` (`scan_incomplete`); no auto plan | Scan session incomplete for that subtree; no automatic mutation in the `<completed_root>` perimeter; read-only issues still listed, qualified incomplete |
| V3 | qBittorrent fake down | dry-run exit 5, `qbittorrent_unavailable` | Scan and issues produced; qBittorrent-dependent issues "unknown"; plans blocked "service unavailable"; no mutation (CON-20) |
| V4 | Add torrent T_M with `save_path` `/data/completed/../../etc` | Silently skipped (`normpath` escapes, then `continue`); run proceeds | Mapping error recorded; automation blocked (CON-07) |
| V5 | `expected_owner` = `<creator uid + 1>:<creator gid>` | n/a | Every scanned entry flagged owner mismatch; nothing mutated |
| V6 | After planning a consolidation of FX-04, rewrite FX-04 with same size, different bytes | Revalidation fails (`Le fichier a changé depuis le scan`), previously active hashes resumed | Plan invalidated before any stop (revalidate first), journal records the refusal; T_B1/T_B2 untouched |
| V7 | Flip byte 10000 of FX-09 (outside the 5×4096 sample windows at offsets 0, 23552, 47104, 70656, 94208) | With similar-name search on: samples match, repair proposed, full compare fails, media untouched (T-12 analogue) | Funnel keeps the pair through sampling, full compare rejects it; no consolidation; issue stays "candidate, content differs" |
| V8 | qBittorrent version `v4.6.7` | `pause`/`resume` endpoints | Same (T-27) |

### 7.5 Expected scan inventory (legacy `scan_tree`, `compute_scan_statistics`)

| Measure | `<completed_root>` | `<media_root>` |
|---|---|---|
| Regular files | 21 (FX-01,02,04,06,07,08,10,12,13,14,16,21,25,26,27,29,30,31,32,34,35) | 8 (FX-03,05,09,11,15,20,28,33) |
| Videos (`.mkv` regular files) | 16 | 8 |
| Symlinks (recorded, not traversed) | 2 (FX-22, FX-23) | 1 (FX-24, `is_video=false`) |
| Directories | 8 | 12 |
| Logical bytes | 636288 | 491520 |
| Unique inodes | 20 (FX-34/FX-35 share I_T) | 8 |
| Scan errors | 0 | 0 |

- Nothing under `outside/` appears in any inventory (FX-17, FX-18, FX-19 never visited; FX-23 not
  traversed).
- `shared_inodes` = 3: {I_A, I_F1, I_K}; `shared_allocated_bytes` = alloc(I_A)+alloc(I_F1)+alloc(I_K).
- `orphan_videos_reclaimable_bytes` = alloc over {I_B1, I_C1, I_Cs, I_D1, I_E, I_F2, I_I, I_J, I_Ks,
  I_N1, I_T} (11 inodes; I_G excluded because its second link is outside the tree; I_T included once
  because both of its links are in completed).
- `completed_exclusive_reclaimable_bytes` = the 11 above + {I_nfoA, I_nfoC, I_txt, I_rar, I_srt}
  (16 inodes).
- `combined_unique_allocated_bytes` = alloc over the 25 distinct inodes of both trees.

### 7.6 Expected legacy outputs (V1)

**Deletion candidates** (`build_deletion_candidates`, threshold 0.60). Qualifying folders:
`Charlie…-GRP/`, `Charlie…-GRP/Sample/`, `India…-GRP/`, `tango-dupe/`; leaf-most kept: `Sample/`,
`India…/`, `tango-dupe/` (the Charlie release folder is dropped because `Sample/` qualifies).

| ID | Kind | Path (relative to `<completed_root>`) | Videos | Entries | Estimated reclaimable | Best similarity (score) | `has_unmatched_video` |
|---|---|---|---|---|---|---|---|
| 1 | file | `Bravo…-GRP.mkv` | FX-04 | FX-04 | alloc(I_B1) | FX-05 (1.0000) | no |
| 2 | file | `Charlie…-GRP/Charlie…-GRP.mkv` | FX-06 | FX-06 | alloc(I_C1) | FX-09 (1.0000) | no |
| 3 | folder | `Charlie…-GRP/Sample` | FX-07 | `Sample` dir record, FX-07 | alloc(I_Cs) | FX-09 (0.9083) | no |
| 4 | file | `Delta…-OLD.mkv` | FX-10 | FX-10 | alloc(I_D1) | FX-11 (0.9500) | no |
| 5 | file | `Echo…-GRP.mkv` | FX-12 | FX-12 | alloc(I_E) | none | yes |
| 6 | file | `Foxtrot…-GRP/…S01E02….mkv` | FX-14 | FX-14 | alloc(I_F2) | none (episode pool empty) | yes |
| 7 | file | `Golf…-GRP.mkv` | FX-16 | FX-16 | 0 (nlink 2) | none | yes |
| 8 | folder | `India…-GRP` | FX-21 | dir record, FX-21, FX-22 | alloc(I_I) | none | yes |
| 9 | file | `Juliet…-GRP.mkv` | FX-25 | FX-25 | alloc(I_J) | none (symlink is not a video) | yes |
| 10 | file | `Kilo…-GRP/kilo…-grp-sample.mkv` | FX-27 | FX-27 | alloc(I_Ks) | FX-28 (0.9125) | no |
| 11 | file | `November…-GRP.mkv` | FX-32 | FX-32 | alloc(I_N1) | FX-33 (1.0000) | no |
| 12 | folder | `tango-dupe` | FX-35 | dir record, FX-35 | 0 (1 of 2 links) | none | yes |
| 13 | file | `Tango…-GRP.mkv` | FX-34 | FX-34 | 0 (1 of 2 links) | none | yes |

Score derivation (DR-09): identical titles and years give `0.55 + 0.25 + 0.10*size_ratio + 0.10`;
ratios: Charlie sample 8192/98304, Delta 40960/81920, Kilo sample 8192/65536. Different titles
sharing only "movie" with a different year score at most 0.533 (< 0.60).

**Repair proposals** (`discover_hardlink_repairs`):

| Setting | Repairs |
|---|---|
| `search_similar_names = false` (legacy `__main__` default) | R1 FX-04 -> FX-05 `exact_name_and_size`; R2 FX-32 -> FX-33 `exact_name_and_size` |
| `search_similar_names = true` (function default) | R1, R2, plus R3 FX-06 -> FX-09 `similar_name_and_samples` (score 1.0) |

No proposal for FX-10 (size differs), FX-07/FX-27 (size differs), others (no match).

**Unlinked inventories** (`write_unlinked_inventory_reports`):

- `unlinked-completed`: 13 rows, case-folded path order: FX-04, FX-06, FX-07, FX-10, FX-12, FX-14,
  FX-16, FX-21, FX-25, FX-27, FX-32, FX-35, FX-34. `possible_external_links = True` only for FX-16
  (nlink 2, 1 seen); FX-34/FX-35 show nlink 2, seen 2, `False`.
- `unlinked-medias`: 5 rows: FX-05 (best FX-04, 1.0), FX-09 (best FX-06, 1.0), FX-11 (best FX-10,
  0.95), FX-20 (no best; `possible_external_links = True`), FX-33 (best FX-32, 1.0).

**qBittorrent index** (`build_qbit_index`): `path_to_hashes`: FX-01, FX-02 -> {H1}; FX-04 -> {H2, H3};
FX-06, FX-07, FX-08 -> {H4}; FX-10 -> {H5}; FX-13, FX-14 -> {H6}; FX-25 -> {H7}; FX-26, FX-27 -> {H8};
FX-32 -> {H9}; FX-34 -> {Ha}; `<completed_root>/Papa….mkv` (absent) -> {Hb}. T_Q skipped silently
(outside container root), T_O skipped (not under completed). `mapping_errors = []`.
`torrents/files` is requested for 11 torrents (all except T_Q and T_O).

**Dry-run** (`main`, `run_mode = dry-run`, similar-name search off):

| Candidate | Auto decision | Reason |
|---|---|---|
| 1 Bravo | keep | `reparation_hardlink_planifiee:<path>` |
| 2 Charlie main | **delete** | all criteria pass (MULTi/MULTi, 1080p/1080p) |
| 3 Charlie `Sample/` | **delete** | 0.9083 >= 0.90, all criteria pass (CON-01) |
| 4 Delta | **delete** | 720p -> 1080p, FRENCH -> MULTi compatible (CON-02) |
| 5 Echo | keep | `modification_plus_recente_que_48h`, `video_non_admissible:…:aucun_remplacant` |
| 6, 7, 8, 9, 12, 13 | keep | `video_non_admissible:…:aucun_remplacant` |
| 10 Kilo sample | keep | `langue_absente_ou_incompatible` (no language tag) |
| 11 November | keep | `reparation_hardlink_planifiee:<path>` |

Planned: 2 repairs, 3 deletions, predicted unused torrents = [H5] (T_C keeps its NFO so H4 is not
unused). Exit code 0. Files written: `scan-`, `candidates-` (with `auto_eligible`), `actions-`
(events `auto_plan_written`, `session_summary` with reason "dry-run terminé sans aucune mutation"),
`summary-`, `unlinked-completed-`, `unlinked-medias-`, one `auto-plan-` file. qBittorrent calls:
`POST auth/login`, `GET app/version`, `GET torrents/info`, 11 × `GET torrents/files`; no
stop/pause/start/resume/recheck/delete. Inodes unchanged.

**Repair execution (interactive or auto, R1 + R2)**: affected {H2, H3, H9}; previously active
{H2, H9} (`pausedUP` is not active). R1 succeeds (FX-05 now I_B1); R2 fails at the permission probe
with `EACCES` (not `EPERM`, so the generic error is logged). H9 resumed as unmodified; H2, H3
rechecked; on recheck success only H2 resumed (H3 was paused before). `hardlink_gain` =
alloc(I_B2).

### 7.7 Proposed Rust expectations on V1 (new or deliberately different behavior)

Must match legacy: scan inventory (7.5), shared inodes, reclaimable inode sets, the 13
completed-side and 5 media-side unlinked sets, name+size proposals R1/R2, `path_to_hashes`, dry-run
no-mutation guarantees. Deliberate differences (for `issue-engine`, `plan-engine`,
`integration-tests`; final category names are owned by `system-arch`):

| Topic | Expected Rust result | Rule |
|---|---|---|
| Hardlink defects | Same 13 completed paths; outcomes: consolidate FX-04 (-> FX-05, 2 torrents H2+H3), FX-06 (-> FX-09 via parsing + samples + full compare), FX-32 (-> FX-33, **blocked**: target dir not writable); manual / ARR-required FX-10; keep-sample FX-07, FX-27; keep/manual (no replacement) FX-12, FX-14, FX-16, FX-21, FX-25, FX-34, FX-35 | INV-01, INV-10, INV-11, CON-16 |
| Real duplicates | 3 proven pairs: (FX-04, FX-05), (FX-06, FX-09), (FX-32, FX-33) | INV-03 |
| Auto eligibility (auto on, dry-run off) | Consolidation of FX-04 only if H2/H3 allowlisted with progress 1 (yes: `stalledUP`, `pausedUP`) and age >= 48 h (yes); FX-06 likewise (H4 `uploading`); FX-32 blocked (permission); **no deletion is auto-eligible** (FX-10 lacks an ARR snapshot; samples protected) | INV-11, INV-28, INV-47, CON-02 |
| Content rows | FX-04 shown once with torrent count 2 | INV-29 |
| Orphans | Completed files without torrent: FX-12, FX-16, FX-21, FX-35 (+ junk FX-29..31 per OQ-04); torrent with missing files: T_P | MASTER §5.4 |
| Media-only (informational, never auto-deleted) | FX-05, FX-09, FX-11, FX-20, FX-33 | INV-26 |
| Junk outside torrents | FX-29, FX-30, FX-31; not junk: FX-02, FX-08 (in torrents), FX-07, FX-27 (samples) | INV-27 |
| Permissions | Parent dir of FX-33 not writable by expected owner; 0 owner mismatches (V5 covers mismatches) | NEW-09 |
| Symlinks | FX-22, FX-23, FX-24 reported (never followed); FX-24 never counts as a hardlink for FX-25 | INV-18 |
| qBittorrent protection | H7 `checkingUP`, Hb `missingFiles`, Hd `stalledDL` protected: any plan touching FX-25 is blocked | INV-28 |
| Out-of-scope torrent | T_Q recorded as unmapped / out of scope, visible | CON-07 |
| Incomplete | FX-36 stagnant (30 d > 14 d) with T_O `stalledDL` (action per OQ-06); FX-37 incomplete orphan | NEW-06 |
| Release folder units | Charlie release folder treated as one unit (no split into main file + `Sample/`); India folder unit includes symlink annex FX-22 | CON-23, INV-25 |
| Partial torrent | A plan deleting FX-14 leaves H6 stopped and reported (never removed) | INV-14, T-23 |
| Gain | Deleting only FX-34 or only FX-35 -> attributable 0; deleting both -> alloc(I_T) once | INV-21, T-03 |
| Undeclared sibling | No `outside/` path in DB, issues, logs or exports | INV-09 |

---

## 8. Open questions for `system-arch`

| ID | Question | Recommendation |
|---|---|---|
| OQ-01 | Sample detection rule (legacy has none). | Case-insensitive: filename stem token `sample` delimited by `.`, `-`, `_`, space; or any ancestor dir named `sample`/`samples` below the release folder. Configurable, conservative (false positives only protect). |
| OQ-02 | Default junk extension list. | `.rar`, `.r00`-style parts, `.zip`, `.7z`, `.nfo`, `.txt`, `.sfv`, `.md5`, `.url`, `.jpg`/`.png` outside release folders, subtitles only when orphan; configurable. |
| OQ-03 | Orphan subtitle base-name rule. | Subtitle stem minus trailing language/forced tags (`.en`, `.fr`, `.forced`) must equal a video stem in the same folder. |
| OQ-04 | Can an item appear in several categories (orphan + junk)? | One issue per (item, category); dashboard totals per category; recoverable space deduplicated per inode globally. |
| OQ-05 | Fail-closed perimeter granularity. | Auto mode: whole root on any error in that root (legacy parity). Manual: block targets whose subtree or ancestors have errors. |
| OQ-06 | Stagnant incomplete action vs allowlist (CON-06). | Separate manual-only action type, never auto. |
| OQ-07 | Torrents outside every mapping (CON-07). | Informational "out of scope"; prefix-matching escapes are blocking errors. |
| OQ-08 | Torrent-less path sharing an inode with a torrent file (FX-35 vs FX-34). | Treat the inode as torrent-held for gain and junk/orphan decisions; show both paths. |
| OQ-09 | `expected_owner` default. | Required when permission checks are enabled; otherwise default to the process effective UID:GID with a startup log line. |
| OQ-10 | Minimal age for automatic consolidations (CON-21). | Yes, same threshold. |
| OQ-11 | Folder plan containing a sample (CON-22). | Explicit per-sample acknowledgement; never auto. |
| OQ-12 | Media-root deletions in ARR replacement (CON-24). | Manual-approved replacement plans only. |
| OQ-13 | Default video extensions (CON-31). | `.mkv`, `.mp4`, `.avi`, `.m4v`, `.ts`, `.m2ts`, `.wmv`; fixture uses `.mkv` only. |
| OQ-14 | Reverse restore library -> torrent source (CTX-04) in V1? | Optional in tier 2 (same proof as consolidation, reversed direction); otherwise document as later work. |
| OQ-15 | Symlink anomalies: dedicated category or scan-report note? | Informational category "symlink in managed root"; never actionable automatically. |

---

## Appendix A — Checkpoint CP1 coverage checklists

### A.1 MASTER §2 invariants and anti-patterns

| MASTER §2 item | IDs |
|---|---|
| Hardlink proof `(st_dev, st_ino)` | INV-01, REJ-01 |
| Byte-by-byte equality via funnel, no SHA-256 | INV-02, INV-03, REJ-02, REJ-03, REJ-04 |
| qBittorrent never deletes, `deleteFiles=false`, no `BT_backup` | INV-06, INV-07, REJ-05, REJ-06 |
| Configured roots only, validated, confined | INV-09, INV-30, INV-33, CON-08 |
| Samples protected | INV-10, CON-01 |
| ARR sole quality authority | INV-11, INV-12, INV-34, REJ-09, REJ-10, CON-02 |
| Repair direction | INV-04 |
| Intent journal + immutable plans + revalidation | INV-08, INV-13, CON-11, CON-14 |
| qBittorrent transaction, multi-hash | INV-14, INV-29, INV-40 |
| Fail-closed | INV-15, INV-41, CON-07, CON-10 |
| Single importer | INV-16 |
| Success only after verification | INV-17, CON-12 |
| Anti-pattern `st_nlink == 1` | REJ-01 |
| Anti-pattern client-side data deletion | REJ-06 |
| Anti-pattern unauthenticated destructive route | REJ-14, INV-51 |
| Anti-pattern `localStorage` exclusions | REJ-16, INV-46 |
| Anti-pattern hard-coded roots, debug server, root container | REJ-17 |
| Anti-pattern publish on every push, unpinned actions | REJ-18 |

### A.2 CONTEXTE §1.3 (objectives 1-11)

| §1.3 item | IDs |
|---|---|
| 1 No Jellyfin media loss | INV-11, INV-26, INV-35, REJ-07 |
| 2 Not name only | INV-02, REJ-04 |
| 3 Not `st_nlink` only | INV-01, REJ-01 |
| 4 Repair direction | INV-04 |
| 5 `deleteFiles=false` | INV-06, REJ-06 |
| 6 No `BT_backup` manipulation | INV-07, REJ-05 |
| 7 Intent before mutation | INV-08 |
| 8 Excluded personal directory | INV-09, CON-08 |
| 9 Quality from ARR | INV-11 |
| 10 Technical analysis explanatory | INV-12 |
| 11 No ARR = manual | INV-11 |

### A.3 CONTEXTE §2.12 (rejected choices 1-13)

REJ-01 .. REJ-13 map one-to-one to §2.12.1 .. §2.12.13.

### A.4 CONTEXTE §3 (living specification)

| Section | IDs |
|---|---|
| §3.1 Execution levels | INV-22 |
| §3.2 Inventory (walk, identities, statistics) | DR-02, DR-03, INV-18, INV-21, FX §7.5 |
| §3.3 Matching levels 1-5 + ffprobe | DR-08..DR-13, INV-02, INV-03, INV-37 |
| §3.4 ARR quality evaluation | INV-11, INV-34, NEW-13 |
| §3.5 Exact copy repair | INV-04, INV-05, CON-15, DR-20 |
| §3.6 Different version import | INV-35, INV-36, INV-48 |
| §3.7 Deletion under completed, folders, samples | DR-06, INV-10, INV-25, CON-22, CON-23 |
| §3.8 qBittorrent coordination | INV-14, INV-19, INV-28, DR-23..DR-30 |
| §3.9 Sonarr/Radarr coordination | INV-16, INV-38, INV-39, CTX-05 |
| §3.10 Jellyfin coordination | NEW-15, CTX-06 |
| §3.11 Modes | INV-23, INV-47, CON-03, CON-04, CON-21 |
| §3.12 Revalidation | INV-13, CON-14 |
| §3.13 Journal and durability | INV-08, INV-40 |
| §3.14 Gain measurement | INV-21, DR-31 |
| §3.15 Errors and interruptions | INV-15, INV-20, INV-40, INV-41, CON-20 |

### A.5 CONTEXTE §9 (final directives 1-10)

| §9 item | IDs |
|---|---|
| 1 Model evidence, plans, states first | domain-model, CTX-12, INV-42 |
| 2 Scan = timestamped snapshot that can expire | INV-13, INV-42 |
| 3 Content vs media identity vs quality | INV-03, INV-11, INV-45, INV-48 |
| 4 Inode / full compare / ARR / user | INV-01, INV-03, INV-11, INV-24 |
| 5 Dry-run real path | INV-23 |
| 6 Explicit, journaled, resumable, verifiable mutations | INV-08, INV-17, INV-40 |
| 7 Never walk the excluded directory | INV-09, CON-08 |
| 8 Never `deleteFiles=true` | INV-06 |
| 9 No parallel codec/group preference | INV-11, REJ-09, REJ-10 |
| 10 When in doubt keep data and ask | INV-15, INV-47 |

### A.6 MASTER §3-§9 requirements

| Section | IDs |
|---|---|
| §3 Architecture, performance, I/O | NEW-01..NEW-05, INV-44 |
| §4 Roots and configuration | NEW-07, INV-09, INV-30, CON-09 |
| §5 Detections 1-7, mapping units, ARR replacement | NEW-06, NEW-08..NEW-14, INV-25..INV-29, INV-35..INV-37, INV-49 |
| §6 Web UI, actions, auto mode, APIs | NEW-15..NEW-20, INV-23, INV-24, INV-38, INV-46, INV-47 |
| §7 i18n and code language | NEW-21 |
| §8 Security, Docker, CI/CD | NEW-22..NEW-25, INV-31, INV-51 |
| §9 Out of scope | REJ-11..REJ-13, REJ-15, REJ-30, CON-29 |
| §10 Risk tiers | NEW-26 |
