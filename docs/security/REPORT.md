# Security audit report: removarr-v2

**Run:** removarr-v2 / run-1. **Skill:** Cloudflare `security-audit` (project install, commit c1c8a8c). **Mode:** full audit, `standard` profile, full-repository scope, no budget set. **Source ref:** `cb62ef1df38b0a0a1b3042b2ff907dec77414b7c` (branch `autonomous-build`, clean worktree). **Prior runs:** none (no compatible ledger; no carried same-source confirmations or changed-source revalidations).

**Execution policy:** source-only. No target-controlled code was executed: no cargo build or test, no Docker, no network, no live service. The skill requires a full OS sandbox (no network, empty env, read-only target, scratch-only writes, resource limits) plus trusted descriptor-based promotion for any execution. These controls were not established on this host, and the orchestrator forbade parallel builds. Consequence: every check is `method: source`, and no finding could reach `confirmed`, which requires a bounded local observed result.

**This is a complete standard-profile pass over source, not a proof of absence.** Agents spent: 33 (4 reconnaissance, 18 hunters over 5 waves, 7 coverage critics, 2 candidate verifiers, 2 final-record verifiers). `run_status: complete`.

## Security posture summary

The code base is unusually defensive for its class. No confirmed vulnerability was found. One needs-validation lead (login rate-limit lock-out behind a reverse proxy with default settings) depends on deployment facts. Another candidate (ffprobe demuxer and environment) was rejected as having no attacker-visible result. The destructive paths have layered, mutually redundant guards: dirfd-confined `openat(O_NOFOLLOW)` filesystem access, full-signature recheck immediately before each unlink/link/rename, an immutable hashed plan recomputed on load and bound to the approval, a journal intent written before each mutation, fail-closed revalidation, and a `LiveMode` token that only exists when `dry_run=false`. The `deleteFiles=false` and `importMode=copy` invariants are enforced by literals at the single send site with no parallel path.

## Confirmed findings

None. (0 critical, 0 high, 0 medium, 0 low, 0 informational.)

## Needs validation

| Fingerprint | Title | Blocker (summary) |
|---|---|---|
| `web-login-limiter-shared-bucket-without-trusted-proxies` | Login limiter collapses to one shared bucket behind a reverse proxy when trusted_proxies is unset | Reverse proxy use, `REMOVARR_TRUSTED_PROXIES` value, Docker peer address. See `NEEDS-VALIDATION.md`. |

No severity is assigned.

## Rejected during validation (not findings)

`matching-ffprobe-unrestricted-demuxer-local-file-read`: ffprobe is run with `-protocol_whitelist file` but no `-f`/`-formats` restriction and with the full environment; the file writer has no channel to read probe output. Retained in `findings.json` so a future run does not repeat the claim without changed evidence.

## Invariants requested by the orchestrator (all held in source)

- **qBittorrent `deleteFiles=false`:** the only reference to `/torrents/delete` is `Cmd::RemoveTask` in `crates/qbit/src/control.rs`, which always sends the literal `deleteFiles=false`. The `QbitControl` trait has no flag parameter. Hashes are validated 40/64 hex, so `all` cannot be smuggled. A 401/403 relogin retry cannot double-send (qBittorrent rejects before executing); transport errors are not retried and removal is judged by a fresh listing.
- **ARR `importMode` copy:** the only ManualImport body is built in `ArrCommander::manual_import` with literal `copy`; the request type has no mode field. Queue removal sends literal `removeFromClient=false`, `blocklist=false`.
- **Dry-run / LiveMode:** `LiveMode(())` has a private field; the only non-test constructor is `Config::live_mode()` returning `Some` only when `!dry_run`; test constructors are behind a `test-support` feature enabled only through `[dev-dependencies]`; the Dockerfile builds `-p removarr --release --locked` with no feature flags. `wiring.rs` builds recording mutators in dry-run.
- **Filesystem races:** `replace_with_hardlink` links to a fresh temp name (EEXIST on collision, no follow), proves the inode with lstat, rechecks the target and renames atomically; residual windows require a writer who could already replace the target. `remove_tree` re-opens each directory with `O_NOFOLLOW` and rechecks dev/ino per entry. No escape found.
- **Path traversal/root escape:** `PathMapper`/`lexpath` reject `.`, `..`, NUL, relative paths; prefix match on component boundaries; roots are canonical, distinct, non-nested; hostile qBit file names become `Invalid` and refuse mutation.
- **Auth/session/CSRF/rate limits:** private AES-GCM cookie, HttpOnly, SameSite=Strict, Secure by default, fresh session id at login, limiter reserved before Argon2, default-deny guard, session+Origin+constant-time CSRF token on all 20 mutating POST routes, GET pages read-only.
- **Templates:** Askama default escaping, no `safe`/raw output anywhere, no inline script, CSP `script-src 'self'`, HTMX `allowEval:false`, SRI, `/static` exact-match on two embedded files. Exports escape CSV formulas, Markdown and HTML.
- **SSRF:** service URLs are operator config (scheme, host, no userinfo); redirects and env proxy disabled, no `danger_accept_*`; no remote-supplied URL is ever fetched.
- **SQLite:** all values bound; `format!` only over constants; stored plans re-verified by hash on load; migration triggers make plans/approvals/journal immutable.
- **Containers/CI:** non-root, read-only rootfs, `cap_drop ALL`, `no-new-privileges`, loopback publish; CI has no `pull_request_target`, `contents: read`, SHA-pinned actions, no expression injection.

## Hardening notes (not findings; no reachable boundary violation demonstrated)

Highest value first. Owners per `docs/architecture.md` section 15.2.

1. `crates/matching/src/ffprobe.rs` (agent 07): add `.env_clear()` and a `-f`/`-formats` allowlist; the child inherits all `REMOVARR_*` secrets; the symlink check is path-based (acknowledged TOCTOU).
2. `crates/workflows/src/actions/auto.rs` `arr_validated_blockers` (agent 13) vs `plans/arrplans.rs` `is_not_upgrade_reason` (agent 12): execution-time check accepts any rejection reason while planning requires "not an upgrade"; `is_not_upgrade_reason` uses broad `contains("upgrade")`; no episode-coverage recheck of the replacement at execution.
3. `crates/workflows/src/observe/services.rs` `read_arr` (agent 11): `history_for_download(...).unwrap_or_default()` turns a failed history call into "no history" while the service stays Ok; mark the observation partial.
4. `crates/workflows/src/actions/recovery.rs` (agent 13): in the dry-run/no-controller branch, torrents stopped by an interrupted run are only reported in memory and are never restarted by a later live start; `QbitRemoveKeepData` recovery trusts a single (possibly empty) listing; leftover `.removarr-link-*`/`.removarr-probe-*` hardlinks are not swept and are inventoried as ordinary files.
5. `crates/storage/src/codec.rs` `scrub` and `crates/jobs/src/events.rs` `redact` (agents 04, 14): weaker than `reporting::sanitize` (miss JSON keys, spaced `=`, `?passkey=` in a larger token, scheme-less announce paths). No path currently feeds real secrets into them.
6. `crates/domain/src/fs.rs` `EntrySignature` (agent 03): no ctime, so an in-place rewrite with restored mtime is undetected; needs a non-operator writer on the NAS. Plan hash is unkeyed SHA-256 (a DB-file writer who drops the triggers can forge a consistent plan and approval); consider an HMAC and re-running `validate::check_plan` on load.
7. Deployment: `Dockerfile` `/config` mode 1777 and DB under default umask; no mem/pids limits; `latest` tag is moved by prerelease tags in `release.yml`; Trivy scans a separate amd64 build; base images tag-pinned only; `./config` missing from `.gitignore`; `.env.example` `$$` guidance conflicts with README quoting.
8. Web: locale and flash cookies lack `Secure`; no global connection cap or header-read timeout; `qbit` snapshot keys by hash so a hostile duplicate-hash response drops earlier candidates (compromised qBittorrent only); `qbit/safety.rs` `decide_removal` has no production caller (duplicate logic in `delete.rs`).
9. Resource use (operator-triggered scans only): quadratic classification in `issues/orphan.rs` and `issues/index.rs`, unbounded similarity candidate pool, `classify()` runs synchronously inside async scan.

## Positive source patterns

Typed capability tokens for live mutation; immutable hashed plans with DB triggers; intent-before-mutation journal with recovery that only observes; per-step revalidation including a torrent ownership index covering every candidate path and `.!qB` variants; fail-closed defaults (dry-run on, auto off, cookie Secure on, trusted proxies none); response-size caps and no redirects on every outbound client; centralised strict form validation; `unsafe_code = forbid`.

## Coverage summary (from `coverage-ledger.json`)

38 units over 5 waves: 36 `covered`, 2 `candidate` (both carry the fingerprints above), 0 `blocked`, 0 `deferred`, 0 `out_of_scope`. Wave 1 seeded 20 units from reconnaissance; critics added 8 (wave 2), 5 (wave 3), 2 (wave 4) and 3 (wave 5) units, each hunted by a fresh agent. The last two critics (post-wave-5 and distinct final-clean) each returned `stop: true` with no missing units and no reassignments.

**Not covered / limits:** no target execution, so no behaviour was observed; deployment facts (TLS, proxy, host firewall, Docker network, GitHub tag protection, GHCR visibility) are outside the repository; dependency CVE status was not checked (`cargo deny` needs the network); test code under `crates/*/tests` was read only as context. Excluded companion classes: data-isolation (single tenant), AI/LLM, memory safety (`unsafe` forbidden), RPC/messaging, desktop/local IPC. The final two critics returned very quickly (about 20 to 26 seconds); their clean result is recorded but carries the weaker assurance that implies.

## Process notes and deviations from the skill text

- Hunter prompts referenced a parent-written `hunter-brief.md` (architecture verbatim, method, promotion block, candidate gate, result contract) and pointed to selected companion blocks by file and section instead of pasting every block verbatim.
- Hunters returned structured JSON; the parent transcribed each unit's checks into the ledger in condensed form.
- Reconnaissance and hunter roles used the generic delegated agent (sonnet); finder and verifier of each record were different agents.
- Verification of the one retained lead: Phase 3 verifier (decision needs_validation), Phase 5 verifier (replaced: corrected line numbers, added the per-(IP,user) window of 5), then a third fresh verifier (verified unchanged). The ffprobe candidate was rejected in Phase 3.
- Ledger bookkeeping fields (`_key`, `_hunter`, `_hardening`, `_added_by`) were added; the validator accepts them.
