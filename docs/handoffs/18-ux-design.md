# Handoff — 18-ux-design (wave 13)

## Delivered scope

Documentation only. Files: `docs/ux-spec.md` (full specification, 14 sections), `docs/ux/wireflows.md` (ASCII wireframes at 360 px and
Mermaid flows), this handoff. No code, template or locale was edited.

Covers: mobile-first navigation (bottom tab bar, "More" sheet, category picker, >= 600/960 layouts); dashboard cards (urgency,
scan freshness, space with four separate quantities, categories, services, jobs, auto mode, recovery); severity system (label + distinct glyph +
colour, tokens with contrast ratios); card-based responsive lists, with the stacked-table pattern for history/jobs/stats;
search, filters, sort, pagination, selection model and counts; issue detail (blockers first, evidence with currency and completeness,
inodes, torrents, planned consequences, five gain kinds); plan page, dry-run preview and approval confirmation as separate pages with a
separate execute step; plan list (specified, not buildable yet); ignored, history, statistics (chart via `reporting::svg::chart_geometry` + data table),
logs; automatic mode and pause reason; read-only settings; loading, empty (7 variants with priority), stale, incomplete, error, cancelled, success states;
accessibility; 300+ new translation keys (spec section 12.2).

## Component and state contracts for `web-ui`

Components (all server-rendered Askama partials; names are suggestions, markup rules are in the spec section cited):

| Component | Inputs | States | Spec |
|---|---|---|---|
| `badge(level, label_key, glyph)` | urgency/severity/status | green, orange, red, neutral, info; always glyph + text | 3.2 |
| `shell` | `ctx.chrome` | dry-run banner, tab bar, skip link, `#live-region` | 2.2 |
| `category_picker` | `ctx.tabs()` + counts | closed/open | 2.3 |
| `scan_card` | `SessionVm?` | never, fresh, stale, incomplete, running | 3.3 |
| `space_card` | `SpaceVm` | normal, overlap note, no data | 3.3 |
| `service_row` | `ServiceVm` | six states x current/historical | 3.3 |
| `issue_card` | `RowView` | default, selected (user-ticked), blocked pill, incomplete pill | 4.2 |
| `list_toolbar` | query | search, filter chips, sort, per_page | 4.5-4.6 |
| `selection_bar` | none / summary fragment | empty, N selected, unknown, blockers | 4.8 |
| `pager` | page, pages | first, middle, last | 4.7 |
| `gain_label(kind, value)` | potential, expected, attributable, observed, added | each with tag + help | 5.2 |
| `blocker_list`, `evidence_card` | `DetailView` | none, N, incomplete, current vs dated | 5 |
| `plan_page` | `PlanView` | per status, preview-only, expired | 6.2 |
| `confirm_form` | `PlanView` | not draft, ready, error summary | 6.4 |
| `execute_area` | `PlanView` | live, dry-run (simulate only), expired | 6.5 |
| `chart(series)` + `series_table` | `StatsView` | empty, one point, many | 7.1 |
| `log_entry` | log entry | level badge + collapsed fields | 7.3 |
| `state_panel(kind)` | key + params | loading, empty variant, stale, incomplete, error, cancelled, success | 8 |

Hard rules to turn into template tests: no `checked` attribute in any server-rendered list or confirmation form except the explicit
`select_all=1` render; no `hx-boost`, no command via `hx-post` (only `selection-summary`); no `hx-on`, no inline script/style; every
`<input>`/`<button>` target >= 44 px; no literal letter text in templates (12.3); destructive button text appears only on the confirm page
and the execute area; Back is before Approve in DOM order.

State/view data required (all present in current views unless listed under open questions): `RowView` fields, `ListView` truncation,
`PlanView` (`can_approve|can_simulate|can_execute`, `required_count`, `required_torrent_removals`, `samples`, `unrecoverable`),
`JobView.active|cancellable|cancel_requested`, `DashboardVm` (`urgency.icon`, `reasons`, `ServiceVm.current`, `SessionVm.stale|complete`),
`AutoVm.paused_reason`, `SeriesVm.guaranteed`.

## New translation keys

About 330 keys, listed with English values in `docs/ux-spec.md` section 12.2, grouped: `shell.*`, `severity.*`, `badge.*`, `status.*`,
`unit.*` (with `.one`/`.other`), `dashboard.*`, `service.*`, `space.*`, `issues.*`, `ignore.*`, `ignored.*`, `plan.*`, `plans.*`, `confirm.*`,
`gain.*`, `effect.*`, `blocker.*` (one trio per domain blocker code, to enumerate), `evidence.*`, `jobs.*`, `auto.*`, `history.*`, `stats.*`,
`logs.*`, `state.*`, `error.nothing_changed|try_again|title|retry_after_seconds`, `empty.*`, `flash.*`, `settings.*`, `explain.*` (one per reporting
explanation code), `next_step.*`, plus `nav.more|settings|categories|skip|open_menu|close_menu|current`. Existing keys keep their values.
`fr.json` must be extended with the same set (19).

## Deviations and decisions

1. Confirmation is a dedicated page styled as a modal card, not a `<dialog>` overlay (opening a modal dialog needs script; HTMX only).
2. No custom JavaScript: "select all" is a server round trip (`select_all=1`), and fragment-failure messages are not shown (see Q1).
3. Issue list is cards at every width (grid at >= 960); stacked-table pattern only for history, jobs, stats.
4. Approve and execute remain separate steps, matching 16 (approve never executes); the extra "Execute..." disclosure on the plan page is UX only.
5. `/plans` list, media, explorer, settings are specified or omitted per spec section 13; no nav entry until built.

## Open questions and TODOs

* Q1 (16/19): add a display-only `select_all=1` query flag that checks only the rows of the current page. Decide whether to allow the htmx
  response-targets extension (or a tiny first-party listener) to show fetch failure text; without it fragment failures are silent.
* Q2 (16): flash messages after redirect (`?flash=<code>` or a short-lived cookie); fallback is the destination's own status.
* Q3 (15/16): plan listing query and `GET /plans`.
* Q4 (15/16): `StatsQuery` view (cumulative vs per-day) and chart route is optional (inline SVG suffices).
* Q5 (16): optional log auto-refresh; not specified.
* Q6 (16): should `POST /auto/resume` require an acknowledgement field? The checkbox is UX only today.
* Q7 (19): `Tr` plural support (`.one`/`.other`) and parameter interpolation, if absent.
* Q8 (16): hidden-recent count in `ListView`, history filters (plan, origin, outcome), `ListView` showing "recent" threshold days, severity labels per
  row, evidence/provenance code enumeration, blocker code list for the `blocker.*` keys, torrent list (hash, name, state) in `DetailView`, annex tree.
* Contrast values in 10.1 are floors; 19 must verify the final palette with a script or tool.
* Error pages are English-only today (16 deviation 6); the spec expects localized error pages.

## Commands run and results

Read-only research (file reads and greps of `MASTER-PROMPT.md`, `docs/architecture.md`, handoffs 15 and 16, `locales/en.json`, web view structs).
No build or test run (documentation only). Handoff 17 did not exist yet; the guard contract of 16 was used. Nothing committed.
