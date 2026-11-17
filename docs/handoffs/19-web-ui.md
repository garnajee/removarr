# Handoff — 19-web-ui (wave 14)

## Delivered scope

Server-rendered UI over the 16 handlers and the 17 guard, per `docs/ux-spec.md`. No custom JavaScript; the only script is the
pinned HTMX file. Everything is HTML forms that work without script; HTMX is used for two read-only enhancements only
(selection total, job progress polling).

Files (owned paths, architecture 15.2 row 19):

* `crates/web/templates/**`: `base.html` (skip link, dry-run banner, SVG glyph sprite, bottom tab bar with "Issues" and "More"
  disclosures, language and sign-out forms, flash region, `#live-region`), `components.html` (badge and csrf macros), pages and
  fragments for dashboard, issue list (also the Ignored tab), selection summary, issue detail, plan, confirmation, jobs, job,
  automatic mode, history, statistics, logs, login, error.
* `crates/web/static/app.css` (tokens light/dark, mobile first, `prefers-reduced-motion`, `prefers-contrast`, `forced-colors`),
  `crates/web/static/vendor/htmx-2.0.4.min.js` + `README.md` (digests and cross-checks).
* `crates/web/src/assets.rs` (`/static/{file}`, `HTMX_SHA256`, `HTMX_SRI`, `HTMX_FILE`, `CSS_FILE`, tests).
* `crates/web/src/handlers/**` (view wiring and rendering only), `src/i18n.rs` (plural and parameters), `src/lib.rs`
  (`pub mod assets`), `locales/en.json` and `locales/fr.json` (211 -> 499 keys each, same key set).
* Tests: `crates/web/src/handlers/tests/ui.rs` (child module of 16's `tests.rs`, so it reuses the fakes), `assets.rs` tests,
  `i18n.rs` tests.

Edits outside the strict ownership list, each minimal and needed (flag for the orchestrator):

* `crates/web/src/router.rs`: one line, `GET /static/{file}` in the public group (no route semantics changed).
* `crates/web/src/auth/login.rs` (17): the `Ctx { .. }` literal gained `flash: None, path: "/login"` (two new fields of `Ctx`).
* `crates/web/tests/auth_flow.rs` (17): one assertion now checks the translated text ("Sign-in failed") instead of the raw key
  `login.failed`, which 17 asked 19 to add to the locales.
* `crates/web/src/handlers/tests.rs` (16): scenario override slots on the fakes (`Overrides`, `FakeJobs.scenario`,
  `FakePlans.stored`), `World.logs`, `mod ui;`, and the selection-summary assertion updated to the new wording.
* `docs/parity-matrix.md`: row T-24 and the count line.

## Routes and assets

No new application route except `GET /static/{file}` (public): `app.css` (`public, max-age=0, must-revalidate`) and
`htmx-2.0.4.min.js` (`immutable`, one year); any other name is 404 (no filesystem access, embedded `include_bytes!`).

New display-only query flag: `select_all=1` on `GET /issues/{category}` and `GET /ignored` (0 or 1, else 422). It ticks exactly the
rows of the current page; the default render and every other navigation tick nothing. It is a link, not a form field.

Flash messages: a one-shot cookie `removarr_flash=<code>` (HttpOnly, SameSite=Strict, 60 s) set by `see_other_flash` after a
command; the next page renders the message and clears the cookie. Only the codes in `handlers::FLASH_CODES` are accepted; the
cookie carries no id, text or authorization. `Location` headers are unchanged (existing route tests still hold).

Locale: unchanged contract (`removarr_locale` cookie, `POST /locale`); separate from `removarr_session`; the language form lives in
the "More" panel. The login page follows the cookie or `Accept-Language`.

## Pinned HTMX

`htmx.org@2.0.4`, `dist/htmx.min.js`, 50917 bytes.
SHA-256 `e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447`, SRI
`sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+`. Verified against four independent sources, none invented:
the npm registry tarball, unpkg (same SHA-256), jsDelivr package metadata (base64 SHA-256 `4gndpcgj...PtEc=` equals the hex), and
the htmx docs at tag `v2.0.4` (`integrity="sha384-HGfzto..."`). `assets::tests::embedded_htmx_matches_the_pinned_sha256` hashes the
embedded bytes; the template emits the SRI; `base.html` carries `htmx-config` (`allowEval:false`, `includeIndicatorStyles:false`,
zero swap and settle delays) and no `hx-on`/`hx-boost`.

## UX decisions and how the hard rules are enforced

* Nothing is preselected: no `checked` attribute in any list or confirmation (test), except `select_all=1`. The hide-recent
  filter is a select (not a checkbox) for that reason. The action select starts on a disabled placeholder.
* Counts: one strip "N on this page | M in this list (size)", a selection bar with the server count (`sel_none`, or the
  select-all banner) that the HTMX `selection-summary` fragment replaces live; without script the "Selection total" button posts
  the same route and gets a full page. Truncation (1000 rows), hidden recent items and the three empty variants are stated.
* Folder vs file marker is a glyph plus a word; severity, urgency, service state and log level are badge = glyph (circle, triangle,
  octagon, info) + text + colour, never colour alone. Blockers and incomplete evidence are pills and a panel.
* Gain labels: potential ("not guaranteed"), logical, expected freed ("a prediction, not yet verified"), added, net, recorded
  (after execution, attributable) and observed (free-space delta, "can include other writers and snapshots"), each with its own
  label and help text; they are never summed in the UI.
* Destructive flow is three pages: list -> plan (draft, "Create plan" changes nothing) -> confirmation page (styled as a modal card,
  Back before Approve in DOM order, typed counts empty, unchecked acknowledgements, "cannot be undone" panel) -> approve (never
  executes) -> execute in a collapsed `<details>` on the plan page, live mode only; in dry run only "Simulate". The words Approve
  and Execute appear only on those pages (test).
* Confirmation is a page, not a `<dialog>` overlay (UX deviation 1 kept): that needs script.
* Escaping: Askama's default HTML escaping everywhere, `translate="no"` on paths, hashes and names; hostile strings (markup, quotes,
  `</title><script>`) are injected through every view that takes service or user text and asserted escaped in both locales and as
  HTMX fragments. No secret or absolute path reaches an attribute (the CSRF token is only the hidden field value, as 17 requires;
  `hx-headers` is not used, so no token sits in the `<body>` tag).
* Accessibility: skip link, landmarks, one `h1` per page (`tabindex=-1`), explicit labels, `aria-live` regions, `<progress>`,
  `<caption>`/`scope` on tables, 44 px targets (48 px primary), 16 px inputs, focus ring 3 px, safe-area insets, `aria-current`.
* Contrast: `scratchpad/contrast.py` (WCAG formula) over the final tokens, light and dark: every text pair >= 4.5:1 (body 7:1),
  focus ring and control borders >= 3:1 (borders were darkened from the spec floor values `#b8c0c8`/`#56616c` to `#6b7580`/`#7b8691`
  after the first run failed 3:1). 0 failures.

## Tests

`cargo test -p removarr-web`: 19 new UI tests plus assets and i18n tests: hostile strings on every GET route in both locales and
as fragments (no raw markup, no inline script/style/handler, one script only, SRI present, CSRF in every post form, `hx-post` only
for the selection total); every template key and every Rust-built or domain-code key (blockers, action kinds, statuses, tiers,
outcomes, evidence, provenance...) exists in both files, French not a copy of English, placeholders identical; a lint that
templates hold no literal letters (except `error.html`); no preselection and `select_all`; list columns, totals, folder/file marker,
hide-recent, filters, truncation, empty variants; Ignored tab and one-item actions; flash one-shot and allowlist; locale cookie
separate from auth; dashboard states (never scanned, stale, incomplete, running, red urgency, six service states, dry-run
banner); plan pages (draft, preview, approved live, expired, invalidated, partial with report); confirmation rules; job
states (progress, cancel requested, cancelled, failed, succeeded); automatic pause reason and resume; history/stats/logs, SVG chart
with five series, markers, dash patterns and a table per series; static assets; login contract in both locales; plain-form flows.

## Known visual limitations

No browser was run in this wave (no screenshot, no Lighthouse): layout is verified by markup and CSS tests only. For `e2e-perf`:
check at 320/360/768/1280 px that the bottom tab bar and its two `<details>` sheets do not cover the focused element, that the sticky
selection bar leaves room above the tab bar, and the 44 px target measurement. The sheets are CSS-only (`<details>`): opening
"Issues" does not close "More" and there is no Escape handling. The chart has a fixed 640x320 viewBox scaled to the width; the
y-axis labels are bytes of the cumulative values and long date ticks may overlap on very narrow screens. Numbers and dates are not
yet localized (decimal comma in French, local time): sizes use binary units with a dot and times are exact UTC ISO strings. The
ignore control is a `<details>` disclosure, not a bottom sheet.

## Backend follow-ups needed (outside my paths; for a backend-api / reporting follow-up)

1. **Localized error pages and form re-render (16, `error.rs`)**: `pages/error.html` is English only and has no navigation; an
   approval or confirmation error replaces the page instead of re-rendering the confirmation with an error summary and the ticks
   preserved (UX 6.4 last paragraph). The error type needs the request locale and, for approval errors, a way to re-render.
2. **Plan listing (15/16)**: `GET /plans` needs a listing query (id, status, mode, created, counts, gains). The UI has no Plans entry
   until then; plans are reachable from jobs, history, dashboard and issue pages (UX 6.6).
3. **Statistics view selector (15)**: `StatsQuery` has no cumulative/per-day or range selector; the chart draws the cumulative
   `chart_geometry` only.
4. **`IssueQuery` search/sort/kind/min-age (15)**: still done in `web` over the first 1000 rows; "hidden recent" and totals are exact
   for those rows only (the truncation notice says so).
5. **Detail data (15/16)**: `DetailView` has no torrent list (hash, name, state), no annex tree and no per-action planned
   consequences; there is no severity-label or recent-threshold field beyond what exists. The detail page shows what `IssueRowVm`
   carries.
6. **Dashboard (15)**: `torrents_left_stopped` stays 0 (no source); the dashboard shows the reporting explanations only through the
   stale/incomplete blocks it already computes, and a few explanation codes with byte parameters fall back to their English text
   (`explain.gain.*`, `explain.cost.*`, `explain.observed.*`, `explain.plan.dry_run`).
7. **`POST /auto/resume` acknowledgement (16, UX Q6)**: no acknowledgement field is required; the form is a single button.
8. **Locale form returns to `/`**: `POST /locale` redirects to the dashboard, not the page the user was on (16).
9. **Not built (no route, UX section 13)**: `/settings`, `/explorer`, `/media/*`, `/plans`. No navigation entry or link points to them.
10. **Optional**: log auto-refresh (Q5) is not implemented; the logs page is a plain GET with a filter form.

## Deviations from the architecture and the UX spec

* Issue list is `<ul>` cards (UX deviation 3 kept); history, stats and logs use the stacked-table pattern or lists.
* The language selector is in the "More" panel rather than a separate settings page (no settings page).
* Select-all is a link (`select_all=1`), not an `hx-get`; the full page reloads (no script needed). Pagination, filters and sort are
  plain GETs too (no `hx-boost`).
* Machine words that remain untranslated by design: service names (`sonarr`...), export format names, hashes, paths, and the
  English `text` of reporting explanations without a translated key.
* The flash mechanism is a cookie rather than a query parameter, to leave every `Location` unchanged.

## Open TODOs

* 22/23 (`e2e-perf`): real-browser pass (viewports, 44 px targets, focus order, forced-colors, reduced motion), HTMX behaviour under
  the real CSP (the selection-summary POST relies on the form's hidden `csrf_token` field), and the response-time budget of the
  list pages with 1000 rows (filtering is done in memory per request).
* Number and date localization helpers (fr decimal comma, local time with `<time datetime>`).
* A tiny first-party progressive enhancement is deliberately absent: a fetch failure of an HTMX fragment is silent (UX Q1); the
  plain-form fallback still works.

## Commands run and results (host target, macOS)

* HTMX download and digests: `curl` of the npm tarball, unpkg, jsDelivr metadata and the htmx docs at tag `v2.0.4` (one-off build
  tooling, listed above); `shasum -a 256`, `openssl dgst -sha384`.
* `cargo fmt --all`: applied; `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: see the orchestrator summary line below.
* `cargo build --workspace --release`: see below.
* Contrast script over the final tokens: 0 failures. Nothing committed; no other network use.

## Wave 16 fixes

* Interrupted jobs: status/outcome label was already localized via `job_status.interrupted`; added a dedicated
  `state.job_interrupted_text` alert on the job page (not the generic failure text), localized the warning
  "interrupted by a restart" (`jobs.warning_interrupted`, matched on the exact jobs-crate string in
  `templates/fragments/job.html`), and a critical pill with a warning glyph in the jobs list (never styled as success).
* Open item for job-runner: the re-queued job id (`StartReport.requeued`) is not exposed on `JobProgress`/`status()`,
  so the UI cannot link to it; the text only says a new run may have been queued. No contract invented.
* Selection bar at narrow widths: secondary actions (selection total, evaluate, verify content, ignore) moved into a
  closed `<details class="sel-more">`; primary row (preview / create plan) stays on one line; bar capped at 55vh with
  scroll below 600 px; select font/padding reduced and option text shortened ("Younger than {n} days: hidden").
  44 px `--tap`, focus-visible and accessible names unchanged. Keys added in both locales
  (`issues.more_actions`, `jobs.warning_interrupted`, `state.job_interrupted_text`); key sets identical (583).
* Not visually re-measured in a browser (no browser run); e2e-perf should re-screenshot 320x640.
* Gate: fmt, clippy -D warnings, `cargo test --workspace` (1047 passed, 0 failed), locale key-set check: all OK.
