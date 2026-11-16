# Handoff — 16-backend-api (wave 12)

## Delivered scope

Owned paths only (architecture 15.2 row 16): `crates/web/Cargo.toml` (added `async-trait`), `crates/web/src/{lib.rs,router.rs,
state.rs,error.rs,forms.rs,health.rs,i18n.rs}`, `crates/web/src/handlers/**`, `crates/web/templates/**` (minimal functional
templates, including `pages/login.html`), `locales/en.json`, `locales/fr.json` (211 keys each, same key set), `crates/app/**`
(manifest, `src/{lib,main,cli,serve,wiring,services,logging,system}.rs`, `tests/wiring.rs`). `Cargo.lock` gained the new edges.
No other crate, no root `Cargo.toml`, no architecture/ADR file was edited. `docs/parity-matrix.md`: row T-24 only.

| Module | Contents |
|---|---|
| `web::state` | The interfaces handlers depend on: `ReadModel` (reporting), `JobControl` (job runner), `PlanControl` (planner), `Readiness`, the `Guard` extension point (`Access`, `Principal`, `Denied`, `FailClosedGuard`), `UseCaseError`/`ExcludedItem`, `WebSettings`, `AppState` |
| `web::router` | Route table, guard middleware (page/command), `CommandGrant`, layers: 1 MiB body (`DefaultBodyLimit` + `RequestBodyLimitLayer`), 8 KiB request target (414), 30 s timeout (408), panic catcher (generic 500) |
| `web::error` | `WebError` (status from `ErrorClass`, i18n key, safe details, correlation id, `Cache-Control: no-store`), `class_status`, `plan_failure`, `job_failure`, `port_failure` |
| `web::forms` | Validation of ids (issue = 32 lowercase hex, plan/job/session = UUID), categories, services, plan hash (64 hex), pagination, sort, filters, search, reason, subjects, rooted paths, locale, log level; bounds `MAX_SELECTION = 2000`, `MAX_PER_PAGE = 200`, `MAX_BODY_BYTES = 1 MiB`, `MAX_URI_BYTES = 8 KiB` |
| `web::health` | `/healthz` (touches nothing), `/readyz` (one DB read, 2 s timeout, no detail) |
| `web::i18n` | `Locale` (`removarr_locale` cookie, then `Accept-Language`), `Tr::t` with English then key fallback, embedded JSON |
| `web::handlers` | `Ctx`/`Cmd` extractors, `render` (page or HTMX fragment), `see_other` (303 or `HX-Redirect`), `parse_form`, formatting helpers; `dashboard`, `issues`, `plans`, `jobs`, `ops`, `reports`, `login`; route tests in `handlers/tests.rs` |
| `app::wiring` | `build(cfg, logs, guard) -> App { state, runtime, db, report, probe }`, `WiringReport`, `default_guard()`, `DB_FILE`, `StartupError` |
| `app::services` | `ReporterModel`, `RuntimeJobs`, `PlannerControl`, `DbReadiness` (web interfaces); `StoreHistory` (`ActionHistory`), `PendingAuto` (`PendingAutoPlans`), `Executing` (`ExecutingPlans`) |
| `app::serve` / `app::cli` | `removarr [serve|healthcheck|--help|--version]`, graceful shutdown, exit codes |
| `app::logging` | `buffer(cfg)` (ring + every secret registered with the redactor), `install` (EnvFilter + `LogLayer::stdout`), `default_filter` |
| `app::system` | `SystemClock`, `UuidV7` |

## Route contracts (for `auth-guard` 17 and `web-ui` 19)

Access: **public** = no guard; **page** = guard with `Access::Page` (session); **command** = guard with `Access::Command`
(session + CSRF + origin) and the `CommandGrant` the handler requires. Every display route is `GET` and read-only; every command is
`POST`. `HX-Request: true` gets the fragment template, otherwise the full page. Commands answer `303 See Other` (or `200` +
`HX-Redirect` for HTMX). Forms are `application/x-www-form-urlencoded`; repeated keys carry lists (`issue_ids=a&issue_ids=b`, no `[]`).

| Method | Path | Access | Input | Result |
|---|---|---|---|---|
| GET | `/healthz` | public | — | `200 ok`, touches nothing |
| GET | `/readyz` | public | — | `200 ready` / `503 not ready` (one read on the DB reader pool, 2 s timeout) |
| GET | `/login` | public | — | minimal page "sign-in unavailable" (placeholder for 17) |
| GET | `/` | page | — | dashboard (`DashboardView`) |
| GET | `/issues/{category}` | page | `q`, `sort=name\|size\|age`, `dir=asc\|desc`, `kind=all\|folder\|file`, `hide_recent=0\|1`, `page>=1`, `per_page<=200` | tab (`ListView`); ignored items excluded; nothing preselected |
| GET | `/issues/{category}/{issue_id}` | page | — | detail (`DetailView`: paths, inodes, blockers, evidence, available actions) |
| GET | `/ignored` | page | same query | ignored items with a restore form |
| GET | `/plans/{id}` | page | — | stored plan (`PlanView`), read-only: no status refresh, no transition |
| GET | `/plans/{id}/confirm` | page | — | confirmation: target count, space impact, irreversible flag, typed counts, per-file sample and unrecoverable acknowledgements (unchecked) |
| GET | `/jobs`, `/jobs/{id}`, `/jobs/{id}/progress` | page | — | memory only (`JobView`; progress fragment polls `every 2s` only while active) |
| GET | `/auto` | page | — | automatic mode status and pause reason (read once; no toggle) |
| GET | `/history` | page | `page`, `per_page` | `HistoryView` |
| GET | `/stats` | page | — | `StatsView` (series totals and per-day points; no SVG yet) |
| GET | `/logs` | page | `level=debug\|info\|warn\|error`, `q` | sanitized ring (200 newest) |
| GET | `/export/{session\|latest}/{json\|csv\|csv-history\|csv-series\|md\|html}` | page | — | attachment rendered in memory |
| POST | `/scan` | command | — | `JobKind::Scan` -> `/jobs/{id}` |
| POST | `/services/{qbittorrent\|sonarr\|radarr\|jellyfin}/refresh` | command | — | `RefreshService` -> `/jobs/{id}`; unconfigured service 409 |
| POST | `/services/{sonarr\|radarr\|jellyfin}/rescan` | command | — | one-action `ServiceRescan` **draft** plan -> `/plans/{id}` (no job, no approval) |
| POST | `/issues/selection-summary` | command | `issue_ids` | read-only fragment: count, size, unknown count |
| POST | `/issues/ignore` | command | `issue_ids` (resolved to subjects) or `subjects`, `reason<=500` | stored in DB -> `/ignored`; one unknown id rejects all |
| POST | `/issues/restore` | command | `issue_ids` or `subjects` | -> `/ignored` |
| POST | `/issues/evaluate`, `/issues/verify-content` | command | `issue_ids` | job -> `/jobs/{id}` |
| POST | `/plans` | command | `category`, `action=consolidate\|delete\|delete_replaced\|cleanup_stalled\|fix_mode\|replace_release\|remove_queue_item`, `issue_ids`, `remove_torrent_tasks`, `include_specials`, `delete_old_media` | `PlanRequest::Issues` draft -> `/plans/{id}` |
| POST | `/plans/preview` | command | same | the same plan through the same path, not stored (page/fragment) |
| POST | `/plans/{id}/approve` | command | `plan_hash`, `confirmed_count`, `confirmed_torrent_removals`, `acknowledged_samples`, `acknowledged_unrecoverable` (`root:rel`) | `Planner::approve` -> `/plans/{id}`; never executes |
| POST | `/plans/{id}/execute` | command | — | `Planner::ticket(id, false)` must pass (approved, unexpired, live plan, live process), then `ExecutePlan { simulate: false }` -> `/jobs/{id}` |
| POST | `/plans/{id}/simulate` | command | — | `ticket(id, true)` then `ExecutePlan { simulate: true }` |
| POST | `/plans/{id}/discard` | command | — | -> `/plans/{id}` |
| POST | `/jobs/{id}/cancel` | command | — | cooperative cancel -> `/jobs/{id}` |
| POST | `/recovery/retry` | command | — | `retry_recovery()` -> `/` |
| POST | `/auto/resume` | command | — | clears `auto_paused` -> `/auto` |
| POST | `/locale` | command | `locale=en\|fr` | sets `removarr_locale` cookie (1 year, SameSite=Strict) -> `/` |

Error responses (`error::class_status`): Validation 422, NotFound 404, Conflict/StateChanged/Cancelled 409, Unauthorized 401,
Forbidden/CapabilityDisabled 403, RateLimited 429 (+`Retry-After`), Unavailable/Storage 503, Rejected 502, Config/Io/Confinement/
Integrity/Internal 500. Body: standalone `pages/error.html` with the English message of the key, safe details (field name and a static
problem, or excluded subject keys with blocker codes) and the correlation id. The raw detail is logged (sanitized) with the
correlation id: INFO for 4xx, WARN for 5xx and confinement. Bad path segments (category, ids, export format) answer 404.

### Contract for `auth-guard` (17)

```rust
#[async_trait] pub trait Guard: Send + Sync {
    async fn check(&self, access: Access, req: axum::extract::Request) -> Result<(Request, Principal), Denied>;
}
pub enum Access { Page, Command }
pub struct Principal { pub user: String, pub csrf_token: String }   // Debug redacts the token
pub enum Denied { Unauthenticated, Forbidden, RateLimited { retry_after_secs: u64 } }
```
* Plug point: `AppState.guard` (`Arc<dyn Guard>`), filled by `removarr::wiring::build(cfg, logs, guard)`; `serve` passes
  `wiring::default_guard()` = `FailClosedGuard` (refuses everything). Replace it in `crates/app/src/serve.rs` (or change
  `default_guard`) with the real guard; `wiring::build` already takes it as a parameter.
* The router calls the guard for every non-public route **before** the handler; on `Ok` it inserts the `Principal` and, for
  `Access::Command` only, the private `CommandGrant`. Command handlers refuse without the grant (403) and display handlers refuse
  without a principal (401), so a route wired without the guard fails closed. A display route never carries a grant.
* `Denied` mapping: `Unauthenticated` -> page: `303 /login` (HTMX: `401` + `HX-Redirect: /login`); command: `401`. `Forbidden` ->
  `403`. `RateLimited` -> `429` + `Retry-After`.
* CSRF token: rendered as the hidden field `csrf_token` in every form (`ctx.chrome.csrf` from `Principal.csrf_token`). The guard
  checks the `X-CSRF-Token` header or buffers the urlencoded body (bounded by the 1 MiB layer already in place), reads
  `csrf_token`, and rebuilds the request with the same bytes. The `Origin`/`Referer` check is the guard's.
* Login: `GET /login` is `handlers::login::page` (placeholder view `LoginView { error_key, retry_after }`, template
  `pages/login.html`); `POST /login`, `POST /logout` are not registered. Add them in the `public` group of `router.rs`.
* `ConnectInfo<SocketAddr>` is available (served with `into_make_service_with_connect_info`), for
  `cfg.server.trusted_proxies.client_ip(peer, xff)`.
* Not done here (12.3, owned by 17): security headers (CSP, nosniff, Referrer-Policy, X-Frame-Options), sessions, rate limit,
  Argon2, `hash-password` subcommand.

### Contract for `web-ui` (19)

* Every page is `templates/pages/<name>.html` = `{% extends "base.html" %}` + `{% include "fragments/<name>.html" %}`; the
  template structs (`<Name>Page`, `<Name>Fragment`) take `ctx: &Ctx` and `v: &<View>`. Views: `DashboardView`, `ListView`
  (`RowView` rows), `DetailView`, `SummaryView`, `PlanView` (`ActionRow`, `ExcludedRow`, optional `PlanReportVm`), `JobsView`,
  `JobView`, `AutoView`, `HistoryView`, `StatsView` (`SeriesRow`), `LogsView`, `LoginView`. `Ctx { tr, htmx, chrome: Chrome { csrf,
  user, dry_run, lang } }`, `ctx.tabs()` lists the issue tabs. Sizes, dates and labels are preformatted in the views.
* Templates are deliberately minimal (no CSS, no HTMX script tag, no SVG). `/static/{file}`, the pinned HTMX file, `assets.rs` and
  the SVG chart markup (`reporting::svg::chart_geometry` over `StatsView.vm.series`) are 19's.
* All UI strings are keys in `locales/*.json`; dynamic reporting keys are mapped to static keys (`service.*`, `reason.*`,
  `recovery.*`, `category.*`, `action.*`). `i18n::tests` asserts both files have the same key set.

## App wiring (`crates/app`)

* Config: `Config::load_from_process()`; invalid -> diagnostics (key names only) on stderr, exit 2. Bad CLI usage exits 64,
  startup failure (DB, adapter, bind, runtime) exits 1.
* Logging: `LogBuffer` (2000 events, level from `server.log_level`), every secret (`REMOVARR_PASSWORD_HASH`, session key,
  qBittorrent password, ARR/Jellyfin API keys) registered with the redactor, `EnvFilter` (`RUST_LOG` overrides; default keeps
  `hyper`, `reqwest`, `h2`, `rustls`, `tower_http` at INFO and `sqlx` at WARN) + `LogLayer::stdout`.
* Storage: `Db::open(<data_dir>/removarr.db)`, `Repos`, `Reporter::open` (own read pool).
* Readers always: `ConfinedFs` (inventory + content), `QbitReader::from_config`, `ArrReader` per configured service,
  `JellyfinReader::from_config`, `FfprobeProbe` (`check_available` at startup: WARN only), `ContentVerifier`.
* Mutators: `cfg.live_mode()` is called once, in `wiring::build`. `Some(token)` -> `Mutators::live(LiveFsMutator::new(roots,
  token), QbitController::new(&reader, token), ArrCommander::new(reader.clone(), caps, token), JellyfinCommander::from_config(cfg,
  token))`; `None` -> `Mutators::recording(..)` only. `LiveMode::` appears nowhere outside `config` and tests (grep).
* `PlanSettings { mode, config_fingerprint, plan_ttl_hours, capabilities (all nine flags incl. AutoRemoveEmptyTorrents),
  implemented_tier: actions::IMPLEMENTED_TIER, process_uid: geteuid, auto_min_age_hours, duration_tolerance, files }`.
* `ActionPorts { .., executing: Executing(PlanRepo), ignores: Some(IgnoreRepo) }`, `ActionSettings { config_fingerprint,
  capabilities, auto_mode_active: false (the runner sets it per cycle call), auto_min_age_hours, recheck_timeout, recheck_interval
  2 s, verifier }`.
* `WorkflowDeps { observe, observe_cfg (roots in order completed, incomplete, media), planner, actions, mutators,
  action_settings, verify, history: StoreHistory, pending: PendingAuto }`; `JobsConfig { schedule (interval, anchor, qbit
  refresh), limits, retry default, auto_mode: cfg.mode.auto_mode }` (read once); `JobRuntime::new(.., RandomJitter)`.
* Serve: `runtime.start()`, bind `server.bind`, `axum::serve(..).with_graceful_shutdown(SIGINT|SIGTERM)`, then
  `runtime.shutdown(shutdown_grace_seconds)` and `db.close()`.
* `healthcheck` does not load the configuration (that would `stat` the roots): it reads `REMOVARR_BIND`, connects to the loopback
  port and expects `HTTP/1.1 200` from `/healthz` within 3 s.

## Deviations from the architecture

1. **Issue search, sort, kind and "hide recent" filters and paging are done in `web`** over at most 1000 rows per category:
   `reporting::IssueQuery` has none of them (only category, severity, offset, limit). `ListView.truncated` says when a category has
   more. Request for 15: add search/sort/kind/min-age to `IssueQuery`.
2. **Issue detail** is the matching row of the category listing (no per-issue query in reporting); an issue beyond the first 1000
   rows of its category answers 404.
3. **User plan requests re-read ARR.** The planner needs `ArrObservation`s; the job runner keeps its `ServiceCache` private. For
   `PlanRequest::Issues` the web plan service calls `observe::read_arr` (status, library, queue, and history of the torrents of the
   selected issues only). Approval passes no observation (staleness does not read them). Without this, consolidations would lose
   the `NoArrImport` guard and ARR-dependent actions would always be excluded. A shared cache accessor in 14 would remove the reads.
4. Not implemented (no use case behind them yet, or owned by later agents): `GET /plans` (no listing query), `/media/{service}/{id}`
   and its evaluate, `POST /plans` `Media`/`Release` variants, `/explorer`, `/settings`, `/stats/chart/{kind}.svg`, `/static/{file}`.
5. Ignore and restore accept `issue_ids` (resolved through the read model) besides `subjects`; list fields have no `[]` suffix.
6. Error pages are English (the error type does not carry the request locale).
7. `DashboardInputs.torrents_left_stopped = 0`: no component persists or exposes the left-stopped set.
8. `/jobs/{id}` reads memory only (no `JobRepo::get` fallback through the interface); old jobs answer 404.
9. `/readyz` uses `StateStore::get` on the reader pool rather than a literal `SELECT 1` (storage exposes no ping).
10. Export: extra formats `csv-history`, `csv-series`, `html` and the session alias `latest`.
11. A `GET` on a command path is answered by the command guard (401/403), not 405.
12. Route tests live in `crates/web/src/handlers/tests.rs` (inside 16's paths) rather than `crates/web/tests/`.

## Open TODOs

* 17: real `Guard`, `POST /login`/`/logout`, security headers, `hash-password`; swap `default_guard()`.
* 19: final templates, CSS, HTMX asset route, SVG charts, settings/explorer/media pages, locale-aware error pages.
* 15: `IssueQuery` search/sort/filters; a per-issue query; a plan listing query.
* 14: an accessor for the in-memory ARR observations (drops deviation 3); left-stopped count in `RuntimeStatus`.
* 22: route tests with the real `JobRuntime` and repositories beyond the empty-database case; 6.8(e) over HTTP.

## Tests

* `crates/web` (30): forms (5), error mapping (2), i18n (3), issue list selection (1), size formatting (1) and 18 route tests with
  fakes only: health probes cheap and detail-free; without auth-guard every protected route redirects/401s and nothing runs;
  commands need the grant and CSRF; every display route renders with zero mutations; HTMX fragment vs page; lists preselect
  nothing and hide ignored items; validation (404/422/409 shapes); request size limits (413, 414, selection cap); one-item and bulk
  selections take the same path (same request shape, same refusals, no implicit selection); preview stores nothing; approval is
  explicit and execution is a checked job; a refused execution queues nothing and leaks nothing; rescans and refreshes imply no
  action; ignore/restore with reason; read-only selection summary; storage and plan failures render no path or secret; job
  commands; exports.
* `crates/app` (5 unit + 5 integration): CLI parsing, loopback health target, healthcheck without a server, filter defaults,
  clock/ids; dry-run wiring builds no mutating adapter (6.8(a)); a live configuration builds the live set and writes nothing in a
  root; the production wiring fails closed and answers `/healthz`/`/readyz`; real routes over an empty SQLite database are safe
  (no host path in any page, planning without a scan is a 409, unknown plan 404); invalid configuration exits 2 without values.

## Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 878 passed, 0 failed, 1 ignored (opt-in real ffprobe).
* `cargo build --workspace --release`: pass (`target/release/removarr` 14 MB, host target).
* Smoke run of the release binary (temp roots, dry-run): `/readyz` 200 after 1.1 s, `removarr healthcheck` exit 0, `GET /` 303
  `/login`, `POST /scan` 401, SIGTERM -> graceful stop, exit 0; an invalid environment exits 2 without echoing the secret value.
* Gates: `cargo tree -p removarr-web -e normal --depth 1` shows no storage/fs/connector crate; `LiveMode::` appears only in
  `crates/config` and tests; `cargo tree -p removarr -e normal,features | grep tier5` prints nothing.
* No network beyond crates.io; nothing committed.

## Follow-up after wave 14

Scope: items 1 to 5 and 8 of `docs/handoffs/19-web-ui.md` "Backend follow-ups needed". Edited: `crates/web/src/**`
(`error.rs`, `forms.rs`, `router.rs`, `state.rs`, handlers `issues/plans/reports/ops/mod`), `crates/reporting/src/**`
(`reporter.rs`, `model.rs`, `svg.rs`, `lib.rs`), `crates/app/src/services.rs` (one delegating method), templates
(`base`, `error`, `plans` page and fragment, `issue_detail`, `issues`, `plan_confirm`, `stats`) and both locales (63 new keys each, 1 key
removed from each, same key sets). No migration, no write path, no new dependency.

### Per-item status

1. **Done, localized errors and confirmation re-render.** `WebError` still renders English in `IntoResponse` and stores itself in the
   response extensions; the new `error::localize` layer (outermost of the route layers, inside `security_headers`) re-renders any
   `WebError` in the request language (cookie, then `Accept-Language`), whichever layer produced it (handler, guard refusal, 414,
   panic catcher). The error page now has `lang`, a navigation bar (dashboard, jobs, history, plans, statistics) and translated
   details: the static `invalid(field, problem)` texts map to `problem.*` keys (`PROBLEMS` table, tested against both locales) and
   blocker codes in excluded-item details go through `blocker.*`; field names and subject keys stay machine words. 429 keeps
   `Retry-After`; the stale `Content-Length` is dropped. `POST /plans/{id}/approve`: a refusal with status 422 or 409 (bad input, hash or
   count mismatch, missing acknowledgement, state change) re-renders the confirmation page with an `id="confirm-error"` summary (`role="alert"`),
   the same status (200 for HTMX, which does not swap error statuses), a fresh CSRF field and the ticked samples and unrecoverable
   files still ticked; the typed counts are deliberately not kept. Other failures (5xx, 401/403, unknown plan) stay plain error pages.
   `ErrorSummary` (`error::WebError::summary(&Tr)`) is the shared, translated shape.
2. **Done, plan listing.** `Reporter::plans(&PlanListQuery) -> PlanListVm` (newest first; id, status, mode `dry_run|live`, origin,
   created, action count, done and failed counts, planned and recorded attributable and added bytes), `ReadModel::plans`, `GET /plans`
   (`page`, `per_page` validated like history; read-only) and a "Plans" entry in the More panel of the navigation. The "auto" label
   is the `auto_policy` origin shown next to the mode.
3. **Done, statistics selector.** `StatsQuery.range` (`StatsRange`: all, 7, 30, 90, 365 days; the later of `since` and the window
   applies) and `GET /stats?view=cumulative|per_day&range=all|7d|30d|90d|365d` (422 otherwise). `view` is a presentation choice:
   `svg::chart_geometry_for(.., per_day)` plots each day's bytes instead of the running total (the existing `chart_geometry` is
   unchanged). The selector is a plain GET form. The gain totals above the chart stay all-time (the page says so when a window is
   selected); the series and tables follow the window.
4. **Done, SQL issue queries.** `IssueQuery` gained `ids`, `ignored`, `unit_kind`, `search`, `sort` (`Severity` default, `Name`,
   `Size`, `Age`), `descending`, `min_age_days` and `include_detail`. Selection, ordering and totals (`total_count`, `total_logical_bytes`,
   `total_potential_bytes`) are computed over every match in SQL (`select_issues`); the page is a slice of that ordered list, so the
   1000-row read and the truncation notice are gone (`issues.truncated` key and template block removed). The detail page, the selection
   summary and ignore/restore resolve any id (chunks of 500). "Hidden recent" is the exact difference with a second query (limit 1)
   without the age filter. Search is a case-insensitive (ASCII) substring match on subject, name, primary path and, for folders, member
   and annex paths, never on JSON keys; `%` and `_` are plain characters (`instr`, no `LIKE`).
5. **Done, detail data.** With `include_detail`: `IssueRowVm.torrents` (hash, name, state from the `torrents` snapshot, 50 max),
   `annexes` (folder units, 200 max, `root:relative`) rendered as a nested list tree, and `planned` (`PlannedActionVm`: plan id and
   status, mode, action index, kind, action state, expected logical, attributable and added bytes, torrent count) for actions of stored
   plans (not discarded, expired or invalidated; 5000 newest actions scanned, 20 kept) whose target path is the issue's unit path.
   List rows never run these queries. The page links each planned action to its plan; nothing there changes anything.
8. **Done, locale return.** The More-panel form posts a hidden `return_to` (the current path); `LocaleForm::return_path` accepts only a
   same-origin relative path (one leading `/`, no `//`, no backslash, ASCII, no control or whitespace, at most 512 bytes) and falls
   back to `/` otherwise (no error, no open redirect). Query strings are not carried (the form only knows the path).

### Accepted deviations (out of scope here)

* Item 6: `torrents_left_stopped` stays 0 (no source); a few explanation codes with byte parameters keep their English text.
* Item 7: no acknowledgement field for `POST /auto/resume` (single button).
* Item 9: `/settings`, `/explorer` and `/media/*` are not built; MASTER section 6 does not require them. `/plans` is now built.
* Item 10: log auto-refresh not implemented (optional).
* Search is ASCII case-insensitive (SQLite `lower`); non-ASCII letters match with their exact case only.
* `include_detail` plan lookup is path equality on the unit path; an action that touches the item only through a parent or an annex
  is not listed.
* The language switch returns to the page path, not to its query string.

### Contract changes for other agents

* `ReadModel` gained `plans(&PlanListQuery)`; `IssueRowVm` gained `torrents`, `annexes`, `planned`; `IssueQuery.id` does not exist
  (`ids: Vec<String>`); `StatsQuery.range` is new. `Reporter::issues` with a default `IssueQuery` behaves as before.
* Routes: `GET /plans` (page access, query `page`, `per_page`), `GET /stats?view&range`, `POST /locale` accepts `return_to`.
* Templates: `ConfirmPage`/`ConfirmFragment` take `f: &ConfirmState`; `pages/error.html` takes `lang`, `app`, `nav_label`, `nav`.

### Tests added

`crates/reporting/tests/followup.rs` (6: SQL sort, filters/search/kind/age/ignored with exact totals, issue beyond row 1000 and paging
over 1100 rows, detail data, plan listing, stats range) and `crates/web/src/handlers/tests/followup.rs` (10: localized error pages in
fr and en with navigation, translated blocker details without paths, confirmation re-render with ticks kept, plain errors for
5xx and unknown plans, successful approval unchanged, plan listing and navigation, statistics selector reaching the read model and
the chart, filters sent to the read model, 1500-row list and detail beyond 1000 and bulk ids, detail data and escaping, locale return
path including open-redirect refusals), plus unit tests (`annex_tree`, `return_path`, problem translations). The web fake read model now
applies `IssueQuery` like the SQL model. `/plans` and a `/stats` query joined the GET-never-mutates and both-locale route lists.

### Commands run and results (host target, macOS)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: 961 passed, 0 failed.
* `cargo build --workspace --release`: pass.
* Nothing committed.
