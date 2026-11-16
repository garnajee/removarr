# Handoff 26 - sec-fix (wave 19)

## Outcome

Inputs: wave-18 audit (0 confirmed, 1 needs-validation lead, 1 rejected and ignored). Audit artifacts under `docs/security/` were not edited. No commit. All tests use in-process routers, fake clocks, fake ports and dummy secrets; no network, Docker or live service.

## Remediation map

| Item | Verified real? | Change | Test |
|---|---|---|---|
| Lead `web-login-limiter-shared-bucket-without-trusted-proxies` | Behavior confirmed locally (existing `forwarded_for_is_ignored_from_untrusted_peers` already showed the per-(IP,user) lockout; new test adds per-IP 20 window, expiry, and trusted-subnet separation). It is the intended fail-closed design; the exposure is a deployment-dependent misconfiguration (empty `trusted_proxies` behind a proxy). Limits and XFF trust unchanged. | `crates/web/src/auth/login.rs` + `auth/mod.rs`: one-time `warn` (security target, event `forwarded_header_untrusted_peer`) when `X-Forwarded-For` arrives from a peer not in `trusted_proxies`. Docs: `README.md` (hardening item 4), `.env.example`, `examples/removarr.toml`. | `crates/web/tests/auth_flow.rs`: `empty_trusted_proxies_behind_a_proxy_share_one_bucket_and_trusting_the_proxy_fixes_it`; log test extended with the new event |
| B1 `observe/services.rs` history failure as "no history" | Real | `read_arr`: any failed/panicked `history_for_download` marks the whole ARR service failed (same path as a library failure), empty observation, so dependent proposals are blocked | `crates/workflows/tests/observe_history_failclosed.rs` |
| B2 `actions/auto.rs` execution-time ARR check weaker than plan time | Real (execution accepted any rejection, or `!approved`) | Extracted pure rule `plans::arrplans::not_upgrade_verdict` (approved / `removarr:` reasons / non-"not an upgrade" reasons), used by plan time `replaced_member` and by `arr_validated_blockers` (re-exported in `plans/mod.rs`) | `crates/workflows/tests/actions_tier5.rs` (`t5_05`, new case: unrelated rejection yields `ArrRejected`) |
| B3a dry-run recovery never restarts app-stopped torrents | Not a defect: a dry-run process has no live controller by design (LiveMode unreachable); restarting would be a mutation. It leaves them stopped and reports them (`NoLiveController`). No change. | none | existing recovery tests |
| B3b leftover `.removarr-link-*` temp links never swept | Real but low (conservative: extra nlink only blocks deletions). NOT fixed: a safe sweep needs a new unlink method on the `FsMutate` port (domain trait, fs impl, dry-run and test fakes) and a CP2-style review of a new destructive path. The recovery note already tells the operator to remove it. Recorded as open. | none | none |
| B4 ffprobe environment | Real (child inherited REMOVARR secrets). `-protocol_whitelist file` was already present; no demuxer allowlist added (no safe `-f` allowlist for probing arbitrary containers). | `crates/matching/src/ffprobe.rs`: `env_clear()` + `PATH=/usr/local/bin:/usr/bin:/bin` | `ffprobe_fake.rs::the_child_does_not_inherit_the_process_environment` |
| B5 weaker scrubbers | Real | Moved `sanitize`, `sanitize_json`, `is_sensitive_key`, `REDACTED*` to `removarr_domain::redact` (lowest crate; storage cannot depend on reporting). `reporting::redact` re-exports them (Redactor/truncate stay). `storage::codec::scrub` and `jobs::events::redact` now call `sanitize`. `scrub_value` still only rewrites string leaves (not key-based, to avoid altering journal fields). | `domain/src/redact.rs` test, `storage/src/codec.rs::scrub_is_as_strict_as_the_shared_sanitizer`, `jobs/src/events.rs` test. Existing codec test input moved `done` to a new line: the stricter rule redacts the rest of a line after `Authorization:`. |
| B6 devops | Real | `Dockerfile`: `/config` now `chown 65532:65532` + `0700` (was 1777). `release.yml`: `latest` only when the tag has no `-` (no prerelease). `.gitignore`: `/config/`. | none (CI/Docker; not runnable here) |

## Public contracts

- New module `removarr_domain::redact` (moved code, same behavior). `removarr_reporting::redact` keeps its public names.
- New `removarr_workflows::plans::not_upgrade_verdict`.
- New security log event `forwarded_header_untrusted_peer` (once per process).

## Deviations / notes

- Dockerfile: with a custom `user: UID:GID` the operator must mount a directory owned by that uid (already required by README line 79). An anonymous volume for a different uid will no longer be writable (fail-closed). Docker was not run.
- `docs/handoffs/20-devops.md` still says 1777 (historical).

## Unresolved / deployment-dependent

- Lead: whether a reverse proxy exists, what `REMOVARR_TRUSTED_PROXIES` is set to, and which peer address Docker presents are deployment facts. Remains "unresolved, deployment-dependent"; the warning and docs make it visible.
- B3b leftover temp-link sweep (open).
- Other audit hardening notes not in scope: `Secure` on locale/flash cookies, connection caps/header-read timeout, ctime in `EntrySignature`, keyed plan hash, mem/pids limits, scan/push image split, `$$` guidance.

## Next

Request a fresh targeted `sec-audit` (not this agent) on: `crates/web/src/auth/login.rs`, `auth/mod.rs`, `crates/workflows/src/observe/services.rs`, `plans/arrplans.rs`, `actions/auto.rs`, `crates/matching/src/ffprobe.rs`, `crates/domain/src/redact.rs`, `crates/storage/src/codec.rs`, `crates/jobs/src/events.rs`, `Dockerfile`, `.github/workflows/release.yml`. Then `release-gate`.

## Commands run

See gates below (appended after completion).

| Command | Result |
|---|---|
| focused: `cargo test -p removarr-web --test auth_flow`, `-p removarr-workflows --test observe_history_failclosed`, `--test actions_tier5`, `-p removarr-matching --test ffprobe_fake`, domain/storage/jobs/reporting | pass |
| `cargo fmt --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo test --workspace` | pass (132 result lines, 0 failed) |
| Locale files | not touched, parity check not needed |
