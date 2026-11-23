# removarr UX specification

Status: wave 13, agent 18 (ux-design). Audience: `web-ui` (19). Scope: server-rendered pages (Askama) enhanced by HTMX 2.0.4.
No SPA, no client framework, no inline script, no `hx-on`, no `eval` (CSP `script-src 'self'`). Every action works with
plain forms and redirects; HTMX only swaps fragments. Wireflows are in `docs/ux/wireflows.md`. Every string a user can see is a key
(section 12); new keys are listed there.

Conventions: "MUST" is a testable requirement. `ctx` is `handlers::Ctx`. Routes are those of `docs/handoffs/16-backend-api.md`;
routes marked NOT BUILT have no backend yet, and the page must degrade as specified (section 13).

## 1. Principles

1. Mobile first: design at 360 px width, then enhance at 600 px and 960 px. No horizontal scroll at 320 px except inside a
   code or path block (which wraps, see 4.4).
2. Nothing destructive is ever preselected, defaulted, focused first, or reachable by a single tap from a list. Destructive
   paths are: list selection -> plan preview (draft, stored, harmless) -> confirmation (approve) -> execute (separate command).
3. Colour is never the only carrier: every status = text label + icon glyph (shape differs per level) + colour.
4. Numbers never merge: potential, expected, attributable, observed and added cost are always separate, each with its label.
5. Everything is dated. A value that comes from an older observation says "as observed at <time>".
6. No absolute host path is ever rendered; paths are `root:relative/path` (contract of the views).
7. Dry run is a global banner (the mode is read once at startup) and every execute-type control reads "Simulate" in dry run.

## 2. Page shell and navigation

### 2.1 Information architecture

| Entry | Route | Notes |
|---|---|---|
| Dashboard | `/` | cards, services, freshness, jobs, auto status |
| Issues | `/issues/{category}` | one tab per category; detail at `/issues/{category}/{id}` |
| Ignored | `/ignored` | restore |
| Plans | `/plans/{id}` (+ `/confirm`) | list `/plans` is NOT BUILT, see 8.6 |
| Jobs | `/jobs`, `/jobs/{id}` | progress, cancel |
| History | `/history` | action journal |
| Statistics | `/stats` | SVG charts + data table |
| Logs | `/logs` | sanitized ring |
| Automatic mode | `/auto` | status, pause reason, resume |
| Settings | `/settings` | NOT BUILT; read-only page specified in section 11 |
| Language | `POST /locale` | in the shell footer / settings |

### 2.2 Shell (`base.html`)

```
<a class="skip-link" href="#main">Skip to content</a>
<header class="topbar">  [brand] [mode badge: Dry run | Live] [scan freshness chip] [menu button (<960)]
[dry-run banner, role="status", when ctx.chrome.dry_run]
<nav aria-label="Main">   primary navigation (see below)
<main id="main" tabindex="-1">  page content; h1 = page title
<div id="live-region" aria-live="polite" role="status"> (empty; HTMX results announce here)
<footer> language form, user, logout (when 17 provides it)
```

Navigation by breakpoint:

* Below 600 px: fixed **bottom tab bar** with 5 slots, each min 56 px tall and 20 vw wide: Dashboard, Issues, History, Logs, More.
  "More" opens a plain `<details>` sheet (no JS) with Statistics, Ignored, Jobs, Automatic mode, Settings, Language, Sign out.
  Reason: thumb reach; the five tasks named in the brief are one tap away. The tab bar is `<nav aria-label="Main">`; the current
  entry has `aria-current="page"` plus a visible underline bar and bold weight (not colour only).
* 600-959 px: top bar with the same entries as a horizontally wrapping list (wraps, never scrolls sideways).
* 960 px and up: left vertical rail (240 px) with all entries and counts; content max width 1200 px.
* The Issues entry on mobile opens `/issues/{first category with issues}`; the category switch is the **category selector**
  (4.1), not a scrolling tab strip. On 600 px and up the categories are a wrapping tab list with counts.
* Issue tabs semantic: these are links to different URLs, so use `<nav aria-label="Issue categories">` with `aria-current="page"`,
  NOT `role="tablist"` (tabs that navigate must not claim the tablist pattern).
* Skip link first in tab order. `main` receives focus after a full navigation via `tabindex="-1"` and the skip target; after an
  HTMX swap focus rules are in 10.3.

### 2.3 Category selector (mobile)

A `<details class="cat-picker">` whose `<summary>` shows the current category name, its count and severity badge, min height 48 px.
Open content: list of links, one per category (`ctx.tabs()`), each row: name, count, severity badge, 48 px tall. Selected row has
a check glyph + `aria-current="page"`. Closes on navigation (new page). Works without JS.

Category order and names: `category.*` keys (13 categories). Order: permission, junk, symlink, hardlink_missing, duplicate,
orphan_completed, incomplete_orphan, incomplete_stalled, torrent_missing_files, unmapped_torrent, torrent_protected, arr_queue,
media_without_source. `media_without_source` is information only: its tab has no action buttons and shows
`issues.info_only` text.

## 3. Dashboard (`GET /`, `DashboardVm`)

### 3.1 Layout (single column on mobile, 2 columns at 600, 3 at 960)

Order top to bottom:

1. **Urgency card** (full width).
2. **Scan freshness card** with Scan now button.
3. **Space card.**
4. **Issues by category** (card list, 4.1 style compact rows).
5. **Services card.**
6. **Running jobs card.**
7. **Automatic mode card.**
8. **Recovery and left-stopped card** (only when `recovery_pending` or `torrents_left_stopped > 0` or `unfinished_intents > 0`).
9. **Explanations** (`explanations`, translated by code+params).

### 3.2 Severity presentation (all components)

| Level | Label key | Glyph (decorative, `aria-hidden`) | Colour token | Text pair |
|---|---|---|---|---|
| green | `urgency.green` | check in circle (circle) | `--ok` | "OK" |
| orange | `urgency.orange` | triangle with bang | `--warn` | "Attention" |
| red | `urgency.red` | octagon with cross | `--crit` | "Action required" |

Rules:
* The glyph and label are always rendered together. The glyph shape differs per level (circle, triangle, octagon) so it
  survives greyscale. `UrgencyVm.icon` names the glyph; web-ui maps it to an inline SVG symbol sprite (`<symbol>` in `base.html`,
  no external file, `fill: currentColor`).
* A badge is `<span class="badge badge--{level}"><svg aria-hidden="true"><use href="#i-{icon}"/></svg><span>Label</span></span>`.
  Screen readers read the label; the glyph has no accessible name.
* Card border: 4 px left border in the level colour AND the badge. Never colour alone, never a colour-only dot.
* Issue row severity has its own labels `severity.critical|warning|info`: Critical = red + octagon, Warning = orange + triangle,
  Information = neutral + info circle. Same badge component, same rules.
* Urgency reasons (`UrgencyVm.reasons[]`, key + optional count) render as a bullet list under the badge, each translated via
  `reason.*` (e.g. "Critical issues (3)"). Red/orange with no reason is a bug; render `reason.other`.

### 3.3 Cards

**Urgency card**: badge (large), one sentence `dashboard.urgency_summary_{level}`, reasons list, "generated at" time.
Whole card is `role="region"` `aria-labelledby` its heading.

**Scan freshness card** (`last_scan: Option<SessionVm>`, `next_scan`):
* States: never (no scan), fresh, stale (`stale = true`), incomplete (`complete = false` or `scan_errors > 0`), running.
* Content: "Last scan" + absolute time + relative age ("3 h ago", from `age_ms`; the absolute ISO time is in `<time datetime>`), status badge, scanned count,
  errors count, "Next scheduled scan".
* Badge mapping: fresh -> green "Fresh"; stale -> red "Stale" (`scan.stale`, because urgency is red when older than twice the interval);
  incomplete -> orange "Incomplete" (red if urgency says so); never -> red "No scan yet" (`reason.scan_missing`);
  running -> neutral "Running" with progress link.
* "Scan now" button (`POST /scan`, `command.scan`), secondary style, never destructive; disabled with a visible explanation
  while a scan job is running (`dashboard.scan_running`), link to that job.
* Explanatory line when `issues_session != last_scan.id`: `issues.from_older_session` ("Issues shown come from an older scan").

**Space card**: four separate rows, each label + value + help text, never summed:
1. Potential space (`space.potential_not_guaranteed`, with the "estimate" tag glyph `stats.estimate`), and below it a quiet line
   "N excluded because shared with another issue" (`space.overlap_excluded`, from `potential_overlap_excluded_bytes`, hidden at 0).
2. Space shared by hardlinks (`space.shared`).
3. Unique allocated (`space.unique_allocated`).
4. Logical size (`plan.gain_logical`).
Under the rows: "as observed by the last scan" (`dashboard.as_of_last_scan`) + `observed_at`. Per-root breakdown (`space.roots`)
in a `<details>`; each root row: root label, logical, unique, shared.
Potential is never in a success colour and never has an up-arrow or "saved" wording.

**Issues by category**: one row per `CategoryCountVm`: name (link to tab), count, severity badge. Zero-count rows are listed last,
muted, with count "0" shown (not hidden) so the user sees the category was checked. Row min height 48 px. The ignored count is a final row
linking to `/ignored` ("Ignored: N", `dashboard.ignored`).

**Services card** (`ServiceVm`): one row per service (qBittorrent, Sonarr, Radarr, Jellyfin): name, state badge, version, observed time.
State -> badge: `ok` green "OK" ; `unavailable` red "Unavailable"; `auth_rejected` red "Authentication rejected"; `mapping_errors`
orange "Path mapping errors"; `not_configured` neutral "Not configured" (not a problem); `unknown` orange "Unknown".
If `current = false`, append "(as of last scan, <time>)" (`service.historical`) and show the badge with a dashed outline so it
does not look live. Row action: "Refresh" (`POST /services/{s}/refresh`, `command.refresh`) for configured services; for sonarr/radarr/jellyfin an
extra "Rescan" (`command.rescan`) that creates a draft plan (so its label is followed by an info line `service.rescan_is_plan`; it goes
to a plan page, not executed). `detail` is shown below as sanitized text.

**Running jobs card**: `running_jobs` rows: kind, status, stage, started time, link to `/jobs/{id}`. Empty: `jobs.none`.

**Automatic mode card**: see section 9.

## 4. Lists (issue tabs, ignored, history, jobs, logs)

### 4.1 Page anatomy of `/issues/{category}`

```
h1 Category name                         [Refresh]
(category info line: what this category is, 1 sentence)  [issues.snapshot_of <time>]  (stale / truncated notices)
Category selector (mobile) | tab list (>=600)
Toolbar: Search | Filters (<details> on mobile, inline >=600) | Sort
Result summary bar: "Showing 1-50 of 212 | Total size 41.3 GB"
Selection bar (sticky, bottom on mobile above tab bar, top of list >=600)
List (cards on mobile, table >=960)
Pagination
```

### 4.2 Responsive representation

One DOM per list, styled by CSS only. The issue list is a semantic `<ul class="rows">` of `<li class="row">` cards at every
width; at >= 960 px the same `<li>` elements use `display: grid` with named columns under a header strip of sort links. A real
`<table>` is used only where column comparison is the point (history, jobs, stats data table). It is one table markup: at >= 960 px it
is a normal table; below 960 px CSS sets `display: block` on table, row and cell, so each row becomes a stacked card whose cells
show their column name through a `data-label` attribute (text from the translation key) rendered with `::before`. Because
`display: block` strips table semantics in some screen readers, 19 MUST keep `role="table"`, `role="row"`, `role="cell"`,
`role="columnheader"` explicit on those elements. No table is ever squeezed onto a phone and none scrolls sideways.

**Issue card (mobile, 360 px)**:

```
+--------------------------------------------------+
| [ ] Name of the folder or file (wraps, 2 lines)  |  <- checkbox 44x44 at the left, name is a link to detail
|     [folder icon] Folder - 3 torrents            |
|     41.3 GB  |  12 days  |  [! Warning]          |
|     [blocked: 2 blockers]  (only if blockers>0)  |
+--------------------------------------------------+
```

* Row height: auto, min 72 px. Whole card body links to the detail (the name `<a>` is stretched with `::after` to cover the card);
  the checkbox sits above the stretched link (own stacking) and its hit box is 44 x 44 px.
* Fields order: name; kind (icon+text "Folder"/"File"); torrent count ("3 torrents", singular/plural keys); size; age ("12 days");
  severity badge; blocker pill ("2 blockers", icon plus text; orange).
* Blocked rows are not selectable for an action that they would block? NO: parity with backend, blocked rows remain selectable, the
  plan excludes and lists them with blockers. The row shows the blocker pill so the user knows; selection-summary shows "N selected,
  M with blockers (will be excluded)".
* Truncation: names wrap, never ellipsize on mobile; on >= 960 the name cell ellipsizes with the full name in `title` AND in the detail.

**>= 960 px** grid columns: checkbox 44 px | Name (1fr) | Type 90 px | Torrents 90 px | Size 110 px | Age 90 px | Severity 120 px | Blockers 110 px.
Header labels are links for sortable columns (Name, Size, Age) with `aria-sort` on the header cell wrapper and an arrow glyph (text
"ascending"/"descending" as visually hidden text), not only an arrow.

### 4.3 Columns required by the master prompt

Name, age (days), size, torrent count (always shown in that order on mobile card: name, torrents, size, age). Torrent count 0 is
shown as "0 torrents" (it matters for orphans).

### 4.4 Paths and long text

Paths (`root:relative/path`) use `overflow-wrap: anywhere`, monospace, a copy-free plain text (no clipboard script). The root part is
a separate `<span class="path-root">` with bold weight so the user sees which root first. Never render absolute host paths.

### 4.5 Search

* `<form method="get" role="search">` with one text input `q` (`type="search"`, `inputmode="search"`, `enterkeyhint="search"`,
  `autocomplete="off"`, visible `<label>` "Search", min height 48 px) and a submit button "Search" (`issues.search`). Max length 200.
* HTMX enhancement: `hx-get` same URL, `hx-trigger="search, keyup changed delay:400ms"` (delay avoids request storms), `hx-target="#list-region"`,
  `hx-push-url="true"`, `hx-sync="this:replace"`. Without JS the button submits.
* Searches name and paths. Empty result -> empty state 8.2 variant "no matches" with a "Clear search" link.
* A `Clear` link appears when `q` non-empty.

### 4.6 Filters and sort (form `method=get`, same form as search so all params persist)

| Control | Param | Values | Control type |
|---|---|---|---|
| Type | `kind` | all, folder, file | segmented radio group `<fieldset><legend>`, 3 options, "All" checked by default |
| Hide recent items | `hide_recent` | 0/1 | single checkbox, label "Hide recent items" plus help "Items newer than N days" (threshold is config, shown in the help text) ; unchecked by default |
| Sort by | `sort` | name, size, age | `<select>` 48 px |
| Order | `dir` | asc, desc | `<select>`, labels "Ascending"/"Descending" |
| Per page | `per_page` | 25, 50, 100, 200 | `<select>`, default 50 |

* Mobile: filters sit in `<details class="filters">` whose summary shows "Filters" + active filter count badge ("2 active",
  `issues.filters_active`). Open by default only when a filter is active. Apply button "Apply" (`issues.apply`) and "Reset filters" link.
* Changing any filter resets `page` to 1 and clears selection (selection is per page render, see 4.8). The form says so
  in a help line `issues.selection_resets` only when a selection is non-empty (HTMX) ; for non-JS the reload clears anyway.
* Active filters are echoed as removable chips above the list: chip text "Type: Folders" with a remove link (min 44 px target; text "Remove filter").
* Default state of every filter is "no filter" (nothing hidden surprisingly). "Hide recent" is off by default so a user sees all by default;
  when on, the result bar states "N recent items hidden" (`issues.hidden_recent_count`) if the view model provides it; otherwise
  just the filter chip (TODO for 16, see handoff).
* Ignored items never appear here; the result bar shows a link "N ignored" (`dashboard.ignored`) to `/ignored`.
* Refresh button: a link `GET` of the same URL (HTMX: `hx-get` with `hx-target="#list-region"`), label "Refresh" (`issues.refresh`), shows
  the new "Snapshot of the scan finished at <time>". It does NOT rescan. A separate neighbouring help text states
  "Refresh reloads the list; use Scan now to look at the disks again" (`issues.refresh_help`).

### 4.7 Pagination

* Server paging (`page`, `per_page`). Controls: Previous, "Page 3 of 9" (`issues.page_of`), Next; Previous/Next are links (buttons-looking),
  min 48 x 48 px; disabled state is a non-link `<span aria-disabled="true">` with the same text (never hidden, never colour only).
  A direct page `<select>` is not offered on mobile; at >= 960 a numeric go-to input is allowed.
* `rel="prev"`/`rel="next"` on the links. HTMX: `hx-get`, `hx-target="#list-region"`, `hx-push-url="true"`; after swap, focus goes
  to the list heading (`#list-heading`, `tabindex=-1`) and scroll to top of the list.
* Truncation notice: when `ListView.truncated`, an orange notice (icon + text): `issues.truncated` ("Only the first 1000 issues of
  this category are listed") placed above the list AND repeated under pagination. Totals say "of the listed items".
* Totals always from the server: list total count and size of the whole filtered set (not just the page). The label states
  scope: "Total (all pages)" `issues.total_all` and "On this page" only if different.

### 4.8 Selection model

Rules (hard):
* No checkbox is ever checked in server-rendered HTML. No "select all" is checked by default. The first selection action is by the user.
* Selection lives in the DOM as checkboxes in a single `<form id="selection">` (name `issue_ids`, value = issue id), so
  it works without JS and submits exactly the set the user sees. There is no hidden state that can differ from what is visible.
* Selection is scoped to the **current page**. Moving to another page, applying a filter or sorting discards it (with a warning
  `issues.selection_resets` if non-empty, shown before navigation via the HTMX confirm text only for `hx-get` of pagination links;
  without JS the discard is silent but the selection bar was never persisted, which is the documented behaviour). The UI therefore
  never claims "selected 200" when the user sees 50 items.
* **Select all on this page**: an explicit control, label "Select all N items on this page" (`issues.select_page`), unchecked and
  never a default. No custom script is used (HTMX only), so it is a link/button `GET <same query>&select_all=1` (HTMX: `hx-get`,
  target `#list-region`) that re-renders the same page with every row of THIS page checked, plus a banner "All N items on this page
  are selected. Nothing has been done yet." (`issues.select_all_banner`). The server renders checked rows only for that explicit
  request; the default render and every other navigation never check anything. Backend impact: `select_all` is a new display-only
  query flag (open question Q1; 16 has none today). Until it exists the control is omitted and the user ticks rows one by one.
  "Select all" never spans pages and never exceeds `per_page`.
* A "Clear selection" control (`issues.clear_selection`): a `<button type="reset">` for the selection form (works without JS).
* Selection counts (the "clear bulk-selection counts" requirement). The **selection bar** is always present on list pages, and
  states in plain text, updated live (`aria-live="polite"`, `aria-atomic="true"`):
  * 0 selected: "Nothing selected. Pick items to act on them." (`issues.sel_none`). Action buttons are disabled with
    `aria-disabled` and the reason is the adjacent text.
  * N selected: "N selected on this page of M listed items" / total size "Selection total: 12.4 GB" (`command.selection_total`)
    and unknown count "K no longer found" (`issues.selection_unknown`), and "J have blockers and will be excluded" when known.
  * The numbers come from `POST /issues/selection-summary` (fragment; `hx-trigger="change from:#selection"`, delay 150 ms,
    `hx-sync="this:replace"`, `hx-target="#selection-summary"`), and have a server fallback on submit (the plan preview repeats
    the counts). Before the first response the bar shows the DOM count computed server-side as "0".
* Count wording MUST distinguish three numbers and never use "all" ambiguously: selected on this page, listed in this category
  (after filters), whole category (if truncated). Example strip: `3 selected | 50 on this page | 212 in this list`.

### 4.9 Actions on a list

* Per-row action: the detail page (5). The row has no destructive one-tap action. A kebab/overflow menu is NOT used.
* Bulk actions live in the selection bar: a single primary "Create plan..." (`command.create_plan`) and, where relevant, a
  `<select name="action">` with the allowed actions for the category, whose first option is a disabled placeholder
  "Choose an action" (`issues.choose_action`), selected by default (so nothing is preselected). Secondary: Ignore (`command.ignore`, opens ignore
  sheet with optional reason, `issues.ignore_reason`), Evaluate with ARR (`command.evaluate`), Verify content (`command.verify_content`).
* "Create plan" posts to `/plans` and lands on the plan page in draft status; this step changes no files. Its label says "Create
  plan" not "Delete". Button text for destructive actions never contains "Delete" until the confirmation step (the plan page and the
  confirmation use action names).
* Ignore: a bottom sheet / `<dialog>` with count, reason field (optional, max 500, counter), buttons "Ignore N items" and "Cancel";
  Cancel is the default focus. Ignoring is reversible; the sheet says so (`ignore.reversible`).
* After a command, redirect with a flash result (section 8.7).

### 4.10 Other lists

* **Ignored** (`/ignored`): same card anatomy with reason and ignored-at, filters q/sort/kind, selection of items for "Restore" (`command.restore`).
  Restore confirmation is light (reversible): button text "Restore N items", no modal, result flash. Empty: `ignored.empty`.
* **History** (`/history`, `HistoryView`): mobile = card per action (time exact with ms in `<time>`, action name, target path, outcome
  badge with icon+label, reason, freed bytes with its gain kind label, plan link); >= 960 = real `<table>` with `<th scope="col">` columns
  Time, Action, Target, Reason, Outcome, Freed. Paging Previous/Next as in 4.7. Filter row (NOT BUILT in contract, only paging;
  see handoff). Unfinished intents banner at top (`history.unfinished`) with count and link to recovery (dashboard card).
* **Jobs** (`/jobs`): card list with status badge (queued, running, succeeded, failed, cancelled, interrupted), kind, lane, times.
  Active row shows live progress partial. `/jobs/{id}`: progress bar `<progress>` with text "12 of 200" and stage; cancel button
  (`command.cancel`) needs a lightweight confirm step on the same page (a `<details>` "Cancel this job" -> "Yes, request cancellation" -> state "cancellation requested").
* **Logs** (`/logs`, section 7).

## 5. Issue detail (`GET /issues/{category}/{issue_id}`, `DetailView`)

Single column; sections as `<section aria-labelledby>`; on >= 960 two columns (summary + actions left, evidence right).

Sections, in order:

1. **Header**: name, kind (folder/file icon+text), severity badge, category link, first seen (`issues.first_seen`, absolute + age days), ignored state
   if applicable (badge "Ignored" + reason, with Restore action).
2. **Summary strip** with five labelled facts: Logical size, Potential space (with the `space.potential_not_guaranteed` label and
   the `potential_counted_bytes` note "N counted in totals, M already counted by another issue"), torrents, age, evidence completeness.
3. **Blockers** (above evidence because it gates actions). If `blockers` empty: "No blockers" with check glyph. Otherwise an ordered list:
   each = icon + translated blocker title (`blocker.<code>`) + one sentence explanation (`blocker.<code>.help`) + what resolves it
   (`blocker.<code>.fix`). Heading says "This item will be excluded from any plan until its blockers are resolved". Unknown codes
   fall back to `blocker.unknown` with the raw code (code is a static identifier, safe).
4. **Evidence** (`EvidenceVm[]`): cards, each: kind title (`evidence.kind.<kind>`), summary (sanitized text), provenance
   (`evidence.provenance.<value>`), observed time, a **currency badge**: `current` -> "Current (latest scan)" green check; else
   "Dated observation (<time>)" (`issues.dated`) neutral-with-clock glyph. Completeness badge per card: complete (check),
   incomplete (triangle + "Missing: a, b"), unknown (question glyph + "Unknown"). Whole-issue completeness banner at top of the section
   when not complete: `issues.incomplete_evidence` with consequence "Actions that need proof stay blocked".
5. **Paths and inodes**: a list of `root:relative` paths with kind per path (physical copy vs hardlink) and `dev:ino` list
   (`issues.inodes`). Hardlink groups visually grouped with a text header "Same physical file (3 paths)" (`issues.same_inode`). Folder units: tree of annexes
   (subtitles, NFO, images, samples, subfolders) listed with sizes in a `<details>` open by default ("All N annex files will be
   removed together", all-or-nothing note `issues.folder_all_or_nothing`).
6. **Torrents**: one row per associated hash (hash shortened with first 8 chars + full in `title`, never tracker URLs), name, state with the
   protected/eligible text badge. Note "An action applies to all N torrents of this content" (`issues.all_torrents`).
7. **Available actions** (`available_actions`): per action a card: action label (`action.*`), what happens (`action.<kind>.effect`),
   consequences list (see 5.1), a "Create plan for this item" button (`command.create_plan`) -> plan page (draft). The single-item
   path uses the same endpoint and same plan page as bulk; nothing happens on this click except creating a draft plan, and the
   button caption reads "Create plan" with the action name on the line above, never "Delete now".
   Also: Ignore, Evaluate with ARR, Verify content.
8. **Gain labels** (5.2).

### 5.1 Planned consequences (per available action, shown before creating a plan)

A definition list, all keys: Will do (effect), Files affected (count, annexes), Torrent tasks affected (count), Space effect (see 5.2),
Reversibility ("Reversible" / "Irreversible": text + icon; irreversible uses the red octagon glyph and the word "Irreversible"),
Requires (e.g. "approval with typed count"). Irreversible effects list items use `effect.<code>` keys (e.g. `effect.file_removed_unrecoverable`,
`effect.torrent_task_removed_data_kept`, `effect.hardlink_replaces_copy`).

### 5.2 Gain labels (four kinds and added cost, always separate, always labelled)

| Kind | Label key | Meaning line key | Marker | Guaranteed? |
|---|---|---|---|---|
| Potential | `series.potential` | `gain.potential.help` "Estimate. Not guaranteed to be recovered." | dashed outline + "estimate" tag | no |
| Expected | `series.expected` / `plan.gain_attributable` | `gain.expected.help` "What an approved plan predicts it will free." | dotted outline + "predicted" tag | no |
| Attributable | `series.attributable` | `gain.attributable.help` "Measured as freed by removarr actions." | solid + "measured" tag | yes |
| Observed | `series.observed` | `gain.observed.help` "Change in free space seen on the disk. Can be affected by other writes and by snapshots." | alternating dash + "observed" tag | no, may be negative |
| Added cost | `series.added_cost` | `gain.added.help` "New bytes written (for example copies)." | hatched | yes |

Rules: the word "saved" or "recovered" is never used for potential or expected; each value always carries its kind label next to the
number; a tag (text) accompanies the visual marker; observed is never added to attributable; negative values show a minus sign and the
words "free space decreased". On the detail page only potential and expected are shown (the others exist only after execution).

## 6. Plans: preview, confirmation, execution (distinct steps)

### 6.1 Step model

| Step | Route | Changes anything? | Page title key | Primary button |
|---|---|---|---|---|
| 0 Select | list | no | | "Create plan..." |
| 1 Create plan | `POST /plans` -> `GET /plans/{id}` (draft) | writes a draft row only | `plan.title` | "Review and approve" (link to step 2), "Simulate", "Discard" |
| 1b Preview (optional) | `POST /plans/preview` | nothing stored | `plan.preview_title` | none besides "Create plan" (stored) |
| 2 Confirm (approve) | `GET /plans/{id}/confirm` -> `POST /plans/{id}/approve` | marks plan approved; changes no file | `confirm.title` | "Approve plan" |
| 3 Execute | `POST /plans/{id}/execute` (live) or `/simulate` | YES in live; recorded only in dry run | plan page when approved | "Execute plan" / "Simulate" |

Approve and execute are different requests and different pages. After approving, the user is back on the plan page (status
Approved); the Execute button is a separate form on that page with its own small confirm area (see 6.5). No single click both approves
and executes. In dry run the Execute button is replaced by Simulate (the only run available) and an explanation (`plan.dry_run_only`).

### 6.2 Plan page (`PlanView`)

Order: status badge + mode badge (Dry run/Live) + max risk tier (`plan.max_tier`) -> irreversible banner (red, glyph, `plan.irreversible`)
when `irreversible` -> **Totals** (Actions count; Logical size; Expected freed `plan.gain_attributable`; Added space `plan.gain_added`; Net
`plan.gain_net`; each with gain labels from 5.2 and the line "Expected values are predictions") -> **Actions list** (cards; one per
action: kind label, target `root:rel`, annexes count + `<details>` list, torrents count, logical size, expected gain, tier,
irreversible tag, "companion" tag) -> **Torrent removals** list (if any) -> **Excluded items and their blockers** (`plan.excluded`,
each subject with blocker list; heading shows count "7 excluded") -> Warnings -> Expiry/created/origin -> buttons by state.

Button availability (from `can_approve`, `can_simulate`, `can_execute`, `status`): buttons that are not available are NOT rendered as
disabled mysteries; instead a line states why (`plan.cannot_approve_*`: not draft, expired, preview only). Preview-only
(`stored = false`) shows `plan.preview_not_stored` and the single action "Create plan".

Statuses and badges: draft (neutral, pencil), approved (blue, check), executing (animated-optional spinner glyph + "Executing"), executed
(green, check), partially_executed (orange, triangle), interrupted (red, octagon), discarded (neutral, cross), expired (neutral, clock),
failed (red). Each with text.

### 6.3 Dry-run preview (step 1)

Purpose: show exactly what the plan would do without any approval. Rules:
* Banner (info, not warning) "Preview: nothing has been changed" (`plan.preview_banner`) on step 1 pages always, incl. in live mode.
* In dry run the banner also states `mode.dry_run_banner`.
* The preview shows the same sections as 6.2 and in addition, after a `Simulate`, a **Results** section (`plan.results`):
  per action step list (step name, outcome badge, "simulated" tag) from the dry-run journal, labelled "Simulated: no file was
  changed" (`plan.simulated_label`). Each result row carries icon+text.
* The preview and simulate buttons never carry destructive wording and use secondary button style (outline). The primary style is
  reserved for the one safest forward action on that step.
* The **Approve** link text is "Review and approve" and leads to the confirmation page; this is navigation, not a submit.

### 6.4 Destructive confirmation (step 2, `GET /plans/{id}/confirm`)

A full page (no JS needed) and also presented as a modal `<dialog>` when reached via HTMX from the plan page. Content in this
exact order (so it can be read top to bottom on a phone):

1. **Title** "Confirm the plan" (`confirm.title`), the dry-run/live badge.
2. **Target block** (labelled `confirm.target`): list of targets as `root:relative/path`. More than 5 -> first 5 + `<details>` "Show all N targets".
   The summary sentence names the action: "Delete 14 items (folders: 3, files: 11)" (`confirm.summary`, params). Never "this plan" alone.
3. **Count**: "N actions" (`confirm.targets`) big number.
4. **Space impact** with gain labels: Logical size X; Expected freed Y (prediction); Added space Z; Net W (all 5.2 labels).
5. **Irreversible effects** (red panel with octagon glyph and the heading "This cannot be undone"): bullet list of
   `effect.*` keys. If none irreversible: panel says "No irreversible effect" (check). Items that cannot be recovered are
   flagged as such (e.g. "The old media files below cannot be recovered").
6. **Torrent removals** (if any): list of names (hash short), count, sentence `confirm.torrent_removals` ("data is never deleted by qBittorrent").
7. **Samples to acknowledge** (`confirm.samples`) and **unrecoverable old media files** (`confirm.unrecoverable`): one **unchecked**
   checkbox per item, label = the item path, name `acknowledged_samples` / `acknowledged_unrecoverable` value `root:rel`. No
   "check all" control on this page. Each must be ticked; if the user misses one the server error key is shown (`error.plan.*_not_acknowledged`)
   and the form is re-rendered with the user's ticks preserved (the server may echo only already ticked values).
8. **Typed counts**: input "Type the number of actions to confirm" (`confirm.type_count`), name `confirmed_count`, `inputmode="numeric"`,
   `autocomplete="off"`, empty by default, no placeholder that equals the answer. Second input for torrent removals when
   `required_torrent_removals > 0` (`confirm.type_torrent_count`).
9. Hidden `plan_hash` and `csrf_token`. 
10. **Buttons**: "Back to the plan" (`confirm.back`, link, receives initial focus when it is a dialog) and "Approve plan"
    (`confirm.approve`, danger style, red, text, 48 px). The submit is last in DOM and tab order. Approve is a normal button, not
    `autofocus`, not the form's default via Enter in a text field? Pressing Enter in the typed-count input submits the form;
    that is acceptable because every confirmation condition is still validated server side (typed count must match).
11. Footer note: "Approving does not run anything. Execution is a separate step." (`confirm.approve_not_execute`).

If the plan is not draft: only `confirm.not_draft` and the back link are shown.

Approve error handling: re-render the same page with a summary `role="alert"` at top listing error messages (links to the offending
fields), `aria-invalid="true"` on bad fields, focus moves to the summary.

### 6.5 Execution step (step 3, on the plan page after approval)

* Live mode: a separate form "Execute plan" with a small confirmation area: `<details>` "Execute..." that expands a sentence repeating
  the target count and expected freed space plus irreversible tag, and a button "Execute N actions now" (danger style). Collapsed by
  default; the button is not reachable by tab until expanded (inside the `<details>`). No typed count here (already given in step 2) to avoid
  retyping fatigue, but the sentence makes the consequences explicit. The expiry time is shown ("Expires in 3 h"); an expired plan shows
  `error.plan.expired` and no execute control.
* Dry-run mode: "Simulate" button only (secondary style) plus `plan.dry_run_only` text.
* After submit: redirect to the job page with progress (section 8, 8.7).

### 6.6 Plan list (`GET /plans`, NOT BUILT)

Until built, "Plans" has no nav entry; plans are reached from the issue/ job pages and the dashboard. Specified for 16/19:
card list (mobile) / table (>= 960): created time, status badge, mode badge, origin (user/auto), actions count, expected freed, link.
Filter by status (select), paging as 4.7. Empty state `plans.empty`. See open question Q3.

### 6.7 Service rescan plans

`POST /services/{s}/rescan` creates a one-action draft plan; the plan page is the normal one with a short explainer
"Rescan asks <service> to look at its files again. It does not delete anything." (`plan.rescan_help`). In dry run only Simulate is available.

## 7. History, statistics, logs

### 7.1 Statistics (`/stats`, `StatsView`)

* Top: **gain summary cards** (stacked on mobile): Attributable (measured), Added cost, Net, plus the "Observed" and "Potential/Expected"
  cards labelled as estimates per 5.2, each separately with a kind tag.
* **Chart**: inline `<svg role="img" aria-labelledby="chart-title chart-desc">` produced from `reporting::svg::chart_geometry(series,
  w, h)` (coordinates, ticks, dash pattern, marker per series). web-ui writes markup only; no scale recomputation. Use `viewBox`
  `0 0 480 300` and `width: 100%`, `height: auto`, so it scales. Tick and label font is 18 user units, so at 360 px (scale 0.75)
  text renders at about 13.5 px (never below 12 px rendered). Pass `w=480, h=300` to `chart_geometry`.
* Series are distinguished by **dash pattern + marker shape + direct label**, never only colour (5.2 table). A legend below the chart lists each
  series: swatch drawn with the same dash and marker, label, kind tag, total, and a toggle? No toggles (no JS): all shown.
* Cumulative vs per day: a segmented link control (`stats.cumulative`, `stats.day`) as plain links `?view=`. NOT BUILT in the contract
  (StatsQuery only since/max_points); fallback: only cumulative plus table of per-day points. Open question Q4.
* Data table alternative (always rendered after the chart, inside `<details>` open at >= 960, closed on mobile): `<table>` with
  caption, columns Day, then one column per series with kind labels (`stats.day`, `series.*`) and units "bytes" formatted human +
  `title` with exact. Mobile: the table becomes a per-day card list (definition list of 5 values), not a squeezed table.
* Chart empty (no points): empty state `stats.empty` and no chart frame. One point: render a single marker + the table (not a line).
* Observed note: always below the chart: `stats.btrfs_note` ("Btrfs snapshots can retain space, so the observed free space may not
  grow when files are removed.") shown if `caveat.btrfs_snapshots_may_retain_space` explanation exists, else generic.
* Explanations (`observed.differs_from_attributable|missing`, ...) listed as info notes translated by code.
* `<noscript>` not needed: charts are server SVG.

### 7.2 History

As 4.10. Additional: a banner when `include_dry_run` is false: "Simulated runs are not listed" (`history.simulated_not_listed`).
Row detail link to the plan. Outcome badges: succeeded (check), failed (octagon), skipped (dash), unverified (question) all text+icon. "Freed" column
shows the number with kind tag "Attributable" only when it came from a gain row, else "Not recorded" (`gain.not_recorded`).

### 7.3 Logs (`/logs`, `LogsView`)

* Controls: Minimum level `<select>` (Debug, Info, Warn, Error; default Info), search `q`, Apply, Refresh. Same form pattern as 4.5.
* Entries as a list (`<ol reversed>`? newest last per LogsVm; the UI shows **newest first** by reversing server-side, and states
  "Newest first"). Each entry: time (`<time>`), level badge (icon+text: Debug neutral, Info info-circle, Warn triangle, Error octagon), target (muted),
  message (wraps, monospace), `<details>` for fields (key: value; sanitized, already redacted).
* Mobile: entry card = first line time + level badge; second line message; fields collapsed. No horizontal scroll.
* Footer note: "Messages are sanitized: secrets are removed" (`logs.sanitized`) and "Showing the newest 200 entries" (`logs.limit`).
* Auto refresh is OFF by default; an optional "Auto-refresh" checkbox? Not specified by backend; skipped (open question Q5). Refresh button only.
* Empty: `logs.empty` with filter hint.

## 8. States

### 8.1 Loading

* Page navigations: browser-native. HTMX swaps: `hx-indicator` on the region, `aria-busy="true"` on the target while in flight (htmx
  adds `htmx-request` class; `includeIndicatorStyles` false, so own CSS). The indicator is a text "Loading..." (`state.loading`) with a spinner glyph;
  spinner animation removed under `prefers-reduced-motion` (replaced by static ellipsis text).
* Skeletons are NOT used (they communicate nothing to assistive tech); the previous content stays visible, dimmed to 60% opacity with
  `inert`-free `aria-busy`, so there is no layout jump. Buttons that triggered a request get `aria-disabled` via `hx-disabled-elt="this"` to prevent double submit.
* Jobs: progress fragment polls `every 2s` only while `active`; the fragment of a finished job has no trigger, so polling stops.
  The live region announces stage changes at most every 10 s (do not announce each tick).
* Timeouts: server timeout 30 s -> 408 error state (8.5).

### 8.2 Empty

Each list has a specific empty state with icon, one sentence explaining why, one sentence of next step, and a primary link where sensible:

| Context | Cause | Text keys |
|---|---|---|
| Category with zero issues, scan complete | Good news | `empty.issues.clean` "No issues in this category as of the last scan" + scan time |
| Category empty, no scan yet | Unknown, not good news | `empty.issues.no_scan` "No scan has run yet. Run a scan to look for issues." + Scan now |
| Empty after filters/search | Filter | `empty.filtered` "No items match your filters." + Reset filters |
| Empty, scan incomplete | Uncertain | `empty.issues.incomplete` "The last scan was incomplete, so this list may be missing items." |
| Ignored | none ignored | `ignored.empty` |
| History | none | `empty.history` |
| Stats | none | `stats.empty` |
| Logs | none at level | `logs.empty` |
| Jobs | none | `jobs.none` |
| Plan: nothing plannable | all blocked | `error.plan.nothing_plannable` + excluded list |

"Clean" empty text must not appear when the scan is incomplete, stale or missing (the three variants above are mutually exclusive and
chosen in that priority: no scan, incomplete, stale -> `empty.issues.stale`, filtered, clean).

### 8.3 Stale

* Scan stale (`SessionVm.stale`): a notice banner (orange: triangle + "Stale") at the top of the dashboard and of every issue list:
  "The last scan is X hours old. Items may have changed." (`state.stale_scan`) with Scan now.
* Selection stale (`error.plan.selection_stale`): after a rejected plan creation, banner + Refresh link.
* Service state historical (`current=false`): see 3.3.
* Evidence dated (`current=false`): see 5.
* Plan expired / hash mismatch / stale (`error.plan.expired|hash_mismatch|stale|state_changed`): the plan page shows the key text in an
  orange panel and one next step "Create a new plan" (`plan.rebuild`) ; the confirm form is not offered.
* Actions on stale data are NOT blocked client-side; the server decides; the UI only informs.

### 8.4 Incomplete

* Incomplete scan: dashboard card badge orange/red per 3.3, text lists what is missing (`explanations`: `scan.root_not_scanned`,
  `scan.root_errors`, `scan.complete_with_errors`).
* Incomplete evidence: per issue (5) and in list rows an "Incomplete evidence" pill (triangle + text) next to the blocker pill.
* Incomplete counts/totals: when a total is derived from a truncated list (`truncated`), prefix "At least" (`issues.at_least`).
* Statistics missing data: `gain.not_recorded`, `observed.missing` text rather than zero. A missing value is rendered as "No data", never "0".

### 8.5 Error

* Error pages (standalone `pages/error.html`): title (translated key by class), explanation, correlation reference (`error.reference` + id, monospace,
  selectable), actions: "Back" and "Dashboard" links. For 401 on HTMX `HX-Redirect: /login`.
* Form errors (422 on forms): inline at the field (`aria-describedby`) + summary with `role="alert"`; never lose user input except the typed
  confirmation counts and acknowledgements which are intentionally re-asked? NOT: acknowledgements the user ticked stay ticked; typed counts are cleared (fresh deliberate entry).
* HTMX errors: **all commands are plain forms (no `hx-boost`, no `hx-post` on commands except read-only `selection-summary`)**, so a command
  error always renders the full error page with a Back link. Only read fragments (search, pagination, sort, refresh, selection-summary,
  progress) use HTMX; htmx does not swap 4xx/5xx by default, so on a fragment failure the earlier content simply stays in place, and
  every fragment control is also a real link or form, so the user can retry with a normal navigation. A visible failure message
  (`state.refresh_failed`) needs the htmx response-targets extension or a custom listener; neither is in scope (open question Q1);
  until then `aria-busy` is cleared and the dimming removed so the stale list is not mistaken for loading.
* Rate limit: `error.rate_limited` with `Retry-After` seconds as countdown text (static, no live countdown: "Try again in 30 seconds").
* Service-specific: `error.unavailable`, `error.rejected`, `error.service_not_configured`, `error.capability_disabled` (orange not red when the user
  could not have known; the capability message explains "disabled by configuration or dry run").
* Error tone: say what failed, whether anything changed ("Nothing was changed" `error.nothing_changed` when the failure is a refusal), what to do next.

### 8.6 Cancelled

* Job cancelled: status badge "Cancelled" (neutral, stop glyph), page text `error.cancelled` plus what was done before cancelling ("12 of 40
  actions completed; 28 not started") and whether a recovery step is needed. A cooperative cancel shows "Cancellation requested"
  (`jobs.cancel_requested`) until the job reaches a terminal state.
* User cancelling a dialog/confirmation: closes with no request, focus returns to the opener, no flash.
* Plan discarded: status "Discarded", a one-line confirmation flash.
* A cancelled execution leaves the plan `interrupted` or `partially_executed`: the plan page shows remaining actions
  (`RemainingVm.next_step` translated: wait or recover, run or discard, verify manually, review manually, rescan and replan) and the
  plan-vs-result differences (`explain` codes). Never show "Success" for it.

### 8.7 Success

* Success is a flash message after the redirect: `role="status"`, polite, with check glyph + text, dismissable (a close control 44 px), kept in DOM
  until navigation. Text states what happened AND what did not: "Plan created. Nothing has been changed yet." / "Plan approved. Nothing has run yet."
  / "Execution finished: 12 of 12 actions succeeded." / "Simulation finished. No file was changed."
  Flash mechanism: query parameter `?flash=<code>` is NOT in the backend contract; open question Q2. Fallback: the destination page states its own status (plan status badge, job outcome), which is always accurate.
* Execution result page (job done): outcome badge, counts (succeeded, failed, skipped), freed bytes with kind label (attributable) and
  the "expected vs actual" difference note when `gain.differs_from_plan`, link to history entries.
* A partial success is never shown green; it is orange with a "Partially completed" label.
* Dry-run success is labelled "Simulated" everywhere (tag on the badge) so it cannot be mistaken for a live success.

## 9. Automatic mode (`/auto`, `AutoView`; dashboard card)

* Status badge: Disabled (neutral, dash glyph, `auto.disabled`), Enabled (green check, `auto.enabled`), Paused (red octagon, `auto.paused`). Enabled+paused = Paused.
* Always shown: "Automatic mode and dry run are read once at startup; changing them requires a restart." (`auto.read_once`). No toggle exists.
* Dry-run flag shown next to it ("Automatic mode in dry run only simulates").
* **Paused reason**: panel (red) with the reason translated from the persisted reason, mapped to `reason.*` keys
  (`auto_paused`, `critical_issues`, `recovery_pending`, `scan_incomplete`, `scan_missing`, `scan_stale`, `service_not_ok`,
  `torrents_left_stopped`, `warning_issues`, `other`); unknown reason text -> `reason.other` and the sanitized raw text in a
  `<details>`. Paused time (`paused_at_iso`). A line "It stops at the first uncertainty and waits for you." (`auto.stops_on_uncertainty`).
* **Resume**: button "Resume automatic mode" (`command.resume_auto`) is a danger-neutral secondary button in its own form, shown only when paused, preceded
  by a checklist of what to verify first (each item a link: recovery, services, scan freshness, torrents left stopped) and requires an
  explicit tick of "I checked the reason above" (`auto.resume_ack`, unchecked, required) before the button works (server accepts without; the
  checkbox is `required` in HTML, a UX guard only; open question Q6 for backend). Success flash "Automatic mode resumed" (`auto.resumed`).
* Next scheduled scan (`next_scan`), the last automatic cycle outcome if available (NOT in contract; omitted).
* Dashboard card shows badge, reason (short) and link to `/auto`.

## 10. Accessibility

### 10.1 Standards and tokens

Target WCAG 2.2 AA. Semantic HTML first; ARIA only to fill gaps. `<html lang>` = `ctx.chrome.lang`.

Colour tokens (light theme; dark theme through `prefers-color-scheme: dark`, same ratios). Contrast figures are minimums to verify in 19's CSS test;
adjust hex but never below ratio:

| Token | Use | Light | Dark | Min ratio |
|---|---|---|---|---|
| `--text` on `--bg` | body | #1b1f23 on #ffffff | #e8eaed on #14171a | 7:1 |
| `--muted` on `--bg` | secondary text | #4a5560 | #aab2bb | 4.5:1 |
| `--ok` text/icon on badge bg | green | #0f5c2e on #e3f4e9 | #7ddc9e on #12301f | 4.5:1 text, 3:1 border/icon |
| `--warn` | orange | #7a4a00 on #fff1d6 | #ffc766 on #3a2a08 | 4.5:1 |
| `--crit` | red | #8f1111 on #fde4e4 | #ff9d9d on #3d1414 | 4.5:1 |
| `--link` | links | #0b4fa8 | #8ab8ff | 4.5:1, links underlined in text blocks |
| `--focus` | focus ring | #0b57d0 / #8ab4ff | | 3:1 against adjacent colours |

Badge border 1 px in the level colour (non-text 3:1 against background). Disabled controls: 3:1 minimum is not required by WCAG for disabled, but
disabled controls MUST be explained in text and also not be the only way to know a state.

### 10.2 Touch and size

* Every interactive target >= 44 x 44 CSS px (buttons, checkboxes incl. hit area, links in navigation, pagination, chips remove, dialog close); 48 px for
  primary buttons and inputs; 8 px minimum gap between adjacent targets (7 px visual gap plus the padding). Inline text links inside paragraphs are exempt (WCAG 2.2 inline exception) but list-row links are not.
* Inputs: font-size >= 16 px (prevents iOS zoom). Text base 16 px, line-height 1.5; support zoom to 200% and text spacing overrides without loss.
* Sticky selection bar must not cover content: reserve bottom padding equal to bar + tab bar height; the bar is sticky only while it has content, and
  never covers the focused element (`scroll-padding-bottom` set).
* Safe-area insets (`env(safe-area-inset-bottom)`) added to the tab bar.
* No hover-only or long-press-only interactions; no swipe gestures; no drag. Tooltips (`title`) never carry unique information.

### 10.3 Keyboard and focus

* Tab order = DOM order = visual order (no positive tabindex). Skip link first. Landmarks: header, nav (labelled), main, footer; one `h1` per page, ordered headings.
* Focus ring: 3 px solid `--focus` with 2 px offset, always visible (`:focus-visible`); never `outline: none` without a replacement. Ring also on dark theme.
* Search and filter form: Enter submits. Escape closes `<dialog>`/`<details>` sheets? `<dialog>` handles Escape natively; `<details>` does not: skip.
* **Dialogs**: opening a native modal `<dialog>` needs `showModal()` (script), which is out of scope. Decision: the confirmation is a
  **page** (`GET /plans/{id}/confirm`, works without JS), styled as a modal card (centered, max 640 px, scrim background) with a Back link;
  the "modal" requirement of the brief is met by this dedicated full step, not by an overlay. The ignore sheet and job-cancel area are
  `<details>` disclosures. If a later wave adds real dialogs it MUST: use native `<dialog>` with `showModal()`, set
  `aria-labelledby` to the title and `aria-describedby` to the summary sentence, put initial focus on the Back/Cancel control (never on a
  destructive one), close on Escape, and restore focus to the opener.
* After an HTMX swap into a region: focus is not stolen except (a) pagination/sort/filter result: move focus to `#list-heading` (`tabindex=-1`),
  (b) validation error summary: focus the summary, (c) selection summary updates: no focus change, announce in live region. 
* Selection bar buttons keep DOM focus when their label changes. The disabled "Create plan" uses `aria-disabled="true"` (stays focusable, tells why via `aria-describedby`)
  and the server rejects empty selections anyway.
* Keyboard shortcuts: none (avoids conflicts). Access keys: none.
* Checkbox lists: each checkbox has an explicit `<label>` with the item name ("Select <name>", `issues.select_item`, visually hidden prefix) so screen readers do not hear
  "checkbox" repeated. The select-all control: label "Select all N items on this page".
* Tables (history, jobs, stats): `<caption>`, `<th scope>`; sortable headers contain links, `aria-sort` on `<th>`.
* Live regions: `#live-region` (polite) for flash, selection counts, refresh failures; `role="alert"` only for form error summaries and blocking
  errors. Progress: `<progress value max>` with accessible text.

### 10.4 Reduced motion and other preferences

* No essential motion. With `prefers-reduced-motion: reduce`: all transitions and animations are `none`; spinners replaced by static text;
  `scroll-behavior: auto`; htmx swap/settle delays `0ms` (`htmx-config` `defaultSwapDelay`: 0, `defaultSettleDelay`: 0 always; the only animation is CSS opacity transition 120 ms for dimming, removed under the media query).
* `prefers-contrast: more`: thicker borders (2 px), no translucent overlays. `forced-colors: active`: badges keep borders and glyphs (they are SVG with `currentColor`;
  use `forced-color-adjust: auto`), focus ring uses `Highlight`.
* `prefers-color-scheme` honoured; no manual theme switch (out of scope).
* Text alternatives: SVG glyphs `aria-hidden`; the chart has title+desc and a data table; no information only in images.
* Language: each translated string in the locale of the page; machine data (paths, hashes, codes) in `<span translate="no">` / `lang="zxx"` is not
  needed; use `translate="no"` on paths and hashes.
* Time and numbers: formatted by the server by locale (thousands separators, decimal comma in fr); `<time datetime>` with exact ISO.

## 11. Settings (read-only, NOT BUILT: `GET /settings`, `SettingsVm`)

Scope decision: read-only page, no editing (config is file/env, read at startup). Contents when built: mode flags (Dry run/Live; Automatic enabled/disabled,
`auto.read_once`), capabilities table (nine flags, each: name, On/Off text+icon), roots (label only, never host path), scan interval and
minimum age thresholds, services diagnostics (state, version, last observed), plan TTL, language form (`POST /locale`), version/build.
No secret ever shown (not even masked lengths). Until 19/16 build it, "Settings" in "More" is omitted and the language form lives in the footer.

## 12. Translation keys

Rules: a visible string is a key in `locales/en.json` and `fr.json`, resolved by `Tr::t`; no literal text in templates. Dynamic values are
parameters (`{count}`, `{name}`, `{time}`) so word order can change per language; plural forms use `.one` / `.other` suffix pairs (the `Tr`
helper must pick by count; if not supported, 19 adds `Tr::plural`; see open question Q7). Backend-supplied labels that are
already keys (`label_key`, `state_key`, `potential_label_key`, `reason` keys) are used directly, never the English `label` fallback.
Non-translated on purpose: paths, hashes, version strings, log message text (already English, sanitized), numeric/ISO times (formatted per locale).
Accessible names (`aria-label`, `title`, visually hidden text, `alt`, `<title>`, `<meta>` text) are visible strings for this rule.

### 12.1 Existing keys used (from `locales/en.json`, 211 keys) 

All of `action.*`, `auto.*`, `category.*`, `command.*`, `confirm.*`, `dashboard.*`, `error.*`, `history.*`, `issues.*`, `jobs.*`, `locale.*`,
`login.*`, `logs.level`, `mode.*`, `nav.*`, `plan.*`, `reason.*`, `recovery.*`, `series.*`, `service.*`, `space.*`, `stats.*`, `urgency.*` keep
their current meaning. Mapping of existing keys to UI elements: `nav.main` nav label; `nav.issues|dashboard|history|logs|stats|ignored|jobs|auto`
entries; `command.selection_total` selection total label; `issues.hide_recent`, `issues.kind.*`, `issues.sort`, `issues.dir`, `issues.search`, `issues.refresh`,
`issues.select`, `issues.selection`, `issues.truncated`, `issues.next|prev|pages`, `issues.snapshot_of`, `issues.selection_unknown`, `issues.total`
list controls; `issues.col.*` sort/column labels; `issues.evidence|completeness|inodes|paths|first_seen|dated` detail; `plan.*` and `confirm.*` per section 6;
`mode.dry_run_banner` banner; `error.reference` correlation id.
Needed alterations of existing keys: none (additions only). One value to review: `command.simulate` ("Simulate") fine; `command.execute`
("Execute") fine; `command.preview` ("Preview") fine.

### 12.2 New keys (English values; `fr` to be added by 19 with the same key set)

Shell and navigation
```
nav.more                     More
nav.settings                 Settings
nav.categories               Issue categories
nav.skip                     Skip to content
nav.open_menu                Open menu
nav.close_menu               Close menu
nav.current                  (current page)
shell.language               Language
shell.dry_run_badge          Dry run
shell.live_badge             Live
shell.sign_out               Sign out
shell.scan_freshness         Last scan
state.region_live            Updates
```
Severity, status badges and generic
```
severity.critical            Critical
severity.warning             Warning
severity.info                Information
badge.fresh                  Fresh
badge.stale                  Stale
badge.incomplete             Incomplete
badge.running                Running
badge.ignored                Ignored
badge.simulated              Simulated
badge.estimate               Estimate
badge.predicted              Predicted
badge.measured               Measured
badge.observed               Observed
badge.irreversible           Irreversible
badge.reversible             Reversible
badge.protected              Protected
badge.eligible               Eligible
badge.blocked                Blocked
badge.no_data                No data
status.queued                Queued
status.running               Running
status.succeeded             Succeeded
status.failed                Failed
status.cancelled             Cancelled
status.interrupted           Interrupted
status.skipped               Skipped
status.unverified            Not verified
status.draft                 Draft
status.approved              Approved
status.executing             Executing
status.executed              Executed
status.partially_executed    Partially executed
status.discarded             Discarded
status.expired               Expired
unit.torrents.one            {count} torrent
unit.torrents.other          {count} torrents
unit.days.one                {count} day
unit.days.other              {count} days
unit.items.one               {count} item
unit.items.other             {count} items
unit.blockers.one            {count} blocker
unit.blockers.other          {count} blockers
unit.age_ago                 {age} ago
unit.unknown_size            Unknown size
unit.kind.folder             Folder
unit.kind.file               File
unit.kind.torrent            Torrent
unit.kind.pair               Pair
```
Dashboard
```
dashboard.title                    Dashboard
dashboard.urgency_summary_green    Nothing needs your attention.
dashboard.urgency_summary_orange   Some items need a look.
dashboard.urgency_summary_red      Something needs action before you rely on the results.
dashboard.generated_at             Generated at {time}
dashboard.scan_running             A scan is running.
dashboard.scan_open_job            View the scan
dashboard.scan_scanned             {count} entries scanned
dashboard.scan_errors              {count} scan errors
dashboard.ignored                  Ignored: {count}
dashboard.running_jobs             Running jobs
dashboard.auto                     Automatic mode
dashboard.recovery_needed          Recovery needs attention
dashboard.torrents_left_stopped    {count} torrents left stopped
dashboard.unfinished_intents       {count} unfinished operations
dashboard.explanations             Notes
dashboard.zero_checked             0 (checked)
service.historical                 as of the last scan, {time}
service.rescan_is_plan             Rescan creates a plan you review first. It deletes nothing.
service.version                    Version {version}
service.observed                   Observed {time}
space.unique_allocated             Unique allocated space
space.overlap_excluded             {size} not counted: shared with another issue
space.roots                        By root
space.root_row                     Root {root}
```
Lists, filters, selection
```
issues.title                       {category}
issues.info_only                   Information only. No action is offered for this category.
issues.category_picker             Category: {category}
issues.filters                     Filters
issues.filters_active              {count} active
issues.apply                       Apply
issues.reset_filters               Reset filters
issues.clear_search                Clear search
issues.remove_filter               Remove filter
issues.filter_chip_kind            Type: {value}
issues.filter_chip_hide_recent     Recent items hidden
issues.hide_recent_help            Items newer than {days} days are hidden.
issues.hidden_recent_count         {count} recent items hidden
issues.refresh_help                Refresh reloads the list. Use Scan now to look at the disks again.
issues.per_page                    Per page
issues.dir.asc                     Ascending
issues.dir.desc                    Descending
issues.sort.name                   Name
issues.sort.size                   Size
issues.sort.age                    Age
issues.sorted_asc                  sorted ascending
issues.sorted_desc                 sorted descending
issues.page_of                     Page {page} of {pages}
issues.showing                     Showing {from} to {to} of {total}
issues.total_all                   Total (all pages)
issues.total_size                  Total size {size}
issues.at_least                    At least {value}
issues.truncated_repeat            The list is cut at 1000 issues. Totals cover the listed items only.
issues.select_item                 Select {name}
issues.select_page                 Select all {count} items on this page
issues.select_all_banner           All {count} items on this page are selected. Nothing has been done yet.
issues.clear_selection             Clear selection
issues.selection_resets            Changing the page, filter or order clears the selection.
issues.sel_none                    Nothing selected. Pick items to act on them.
issues.sel_count                   {selected} selected | {page} on this page | {listed} in this list
issues.sel_blockers                {count} have blockers and will be excluded
issues.sel_total_size              Selection total: {size}
issues.choose_action               Choose an action
issues.action_disabled_why         Select at least one item and choose an action.
issues.blocked_pill                Blocked ({count})
issues.incomplete_pill             Incomplete evidence
issues.open_detail                 Open details of {name}
issues.from_older_session          The issues shown come from an older scan.
issues.incomplete_evidence         Some evidence is incomplete. Actions that need proof stay blocked.
issues.same_inode                  Same physical file ({count} paths)
issues.folder_all_or_nothing       All {count} annex files are removed together with the folder, or none.
issues.all_torrents                An action applies to all {count} torrents of this content.
issues.potential_counted           {counted} counted in totals; {excluded} already counted by another issue.
issues.empty_blockers              No blockers.
issues.blockers_heading            Blockers
issues.blockers_intro              This item is excluded from any plan until its blockers are resolved.
issues.available_actions           Available actions
issues.no_actions                  No action is available for this item.
issues.create_plan_for_item        Create a plan for this item
issues.consequences                Planned consequences
issues.torrents_heading            Torrents
issues.annexes                     Annex files
issues.summary                     Summary
issues.currency_current            Current (latest scan)
issues.currency_dated              Dated observation ({time})
issues.missing                     Missing: {list}
issues.completeness_complete       Complete
issues.completeness_incomplete     Incomplete
issues.completeness_unknown        Unknown
```
Ignore and restore
```
ignore.title                 Ignore {count} items
ignore.reversible            Ignored items disappear from lists and bulk actions. You can restore them later.
ignore.confirm               Ignore {count} items
ignore.restore_confirm       Restore {count} items
ignored.title                Ignored items
ignored.empty                No items are ignored.
ignored.reason               Reason
ignored.at                   Ignored on
```
Plans and confirmation
```
plan.preview_title            Plan preview
plan.preview_banner           Preview: nothing has been changed.
plan.simulated_label          Simulated: no file was changed.
plan.dry_run_only             Dry run is on, so you can only simulate this plan.
plan.totals                   Totals
plan.expected_note            Expected values are predictions, not measurements.
plan.actions_count            {count} actions
plan.excluded_count           {count} excluded
plan.torrent_removals         Torrent tasks to remove
plan.cannot_approve_not_draft Only a draft plan can be approved.
plan.cannot_approve_expired   This plan expired. Create a new plan.
plan.cannot_execute_unapproved Approve the plan before executing it.
plan.expires_in               Expires in {duration}
plan.rebuild                  Create a new plan
plan.rescan_help              Rescan asks {service} to look at its files again. It does not delete anything.
plan.step_review              1. Review
plan.step_approve             2. Approve
plan.step_execute             3. Execute
plan.execute_open             Execute...
plan.execute_summary          This will run {count} actions and is expected to free {size}. Irreversible effects apply.
plan.execute_now              Execute {count} actions now
plan.simulate_now             Simulate
plan.back_to_issues           Back to the list
plans.title                   Plans
plans.empty                   No plans yet.
confirm.target                Target
confirm.summary               {action}: {count} items ({folders} folders, {files} files)
confirm.show_all_targets      Show all {count} targets
confirm.count                 Number of actions
confirm.space_impact          Space impact
confirm.irreversible_heading  This cannot be undone
confirm.no_irreversible       No irreversible effect.
confirm.approve               Approve plan
confirm.approve_not_execute   Approving does not run anything. Execution is a separate step.
confirm.errors_heading        Please fix the following
confirm.torrent_removal_count {count} torrent tasks will be removed.
confirm.ack_required          Required
gain.potential.help           Estimate. Not guaranteed to be recovered.
gain.expected.help            What an approved plan predicts it will free.
gain.attributable.help        Measured as freed by removarr actions.
gain.observed.help            Change in free space seen on the disk. Other writes and snapshots can affect it.
gain.added.help               New bytes written, for example copies.
gain.not_recorded             Not recorded
gain.free_decreased           Free space decreased
gain.differs_from_plan        The result differs from the plan.
effect.file_removed_unrecoverable   Files are removed and cannot be recovered.
effect.old_media_unrecoverable      The old media file cannot be recovered.
effect.torrent_task_removed_data_kept The torrent task is removed. qBittorrent keeps the data.
effect.hardlink_replaces_copy       A copy is replaced by a hardlink. The separate copy is gone.
effect.permissions_changed          Owner or permissions of the target change.
effect.arr_queue_item_removed       The ARR queue item is removed.
effect.release_replaced             The release is replaced through ARR.
```
Blockers (one trio per backend blocker code; web-ui enumerates the codes from `domain`; generic entries below)
```
blocker.unknown               Blocked
blocker.unknown.help          A safety check refused this item.
blocker.unknown.fix           Open the evidence below or rescan.
blocker.<code>                (title, one key per code)
blocker.<code>.help           (explanation)
blocker.<code>.fix            (how to resolve)
evidence.kind.physical        Physical file check
evidence.kind.same_physical   Same physical file
evidence.kind.content         Content comparison
evidence.kind.identity        Identity
evidence.kind.quality         Quality decision
evidence.kind.torrent_holds   Torrent holds the data
evidence.kind.service_unavailable Service unavailable
evidence.kind.note            Note
evidence.provenance.<value>   (one key per provenance value)
```
Jobs
```
jobs.title                    Jobs
jobs.open                     Open job
jobs.progress_of              {done} of {total}
jobs.cancel_open              Cancel this job
jobs.cancel_confirm           Yes, request cancellation
jobs.cancel_help              The job stops at the next safe point. Work already done is kept.
jobs.cancelled_summary        {done} of {total} done; {left} not started.
jobs.kind.<kind>              (one key per job kind)
jobs.status_polling_off       Finished
```
Auto
```
auto.title                    Automatic mode
auto.status                   Status
auto.paused_at                Paused at {time}
auto.stops_on_uncertainty     It stops at the first uncertainty and waits for you.
auto.dry_run_only             In dry run, automatic mode only simulates.
auto.check_before_resume      Check before resuming
auto.resume_ack               I checked the reason above
auto.resumed                  Automatic mode resumed.
auto.raw_reason               Technical detail
```
History, stats, logs
```
history.title                 History
history.simulated_not_listed  Simulated runs are not listed.
history.open_plan             Open plan
history.not_recorded          Not recorded
history.page                  Page {page}
stats.title                   Statistics
stats.chart_title             Space over time
stats.chart_desc              Lines for potential, expected, attributable and observed space per day. The table below has the same data.
stats.legend                  Legend
stats.table_caption           Space per day
stats.empty                   No data yet. Statistics appear after plans run.
stats.single_point            Only one data point so far.
stats.btrfs_note              Snapshots (for example on Btrfs) can keep space in use, so observed free space may not grow after files are removed.
stats.view_cumulative         Cumulative
stats.view_day                Per day
stats.attributable_card       Measured freed space
stats.added_card              Added cost
stats.net_card                Net
logs.title                    Logs
logs.search                   Search the logs
logs.newest_first             Newest first
logs.sanitized                Messages are sanitized: secrets are removed.
logs.limit                    Showing the newest {count} entries.
logs.empty                    No log entries at this level.
logs.fields                   Details
logs.level.debug              Debug
logs.level.info               Information
logs.level.warn               Warning
logs.level.error              Error
logs.time                     Time
```
States and generic feedback
```
state.loading                 Loading...
state.refresh_failed          Could not refresh. Showing earlier data.
state.stale_scan              The last scan is {age} old. Items may have changed.
state.not_run                 Not run yet
state.back                    Back
state.dashboard_link          Go to the dashboard
state.close                   Close
state.dismiss                 Dismiss
error.nothing_changed         Nothing was changed.
error.try_again               Try again
error.title                   Something went wrong
error.retry_after_seconds     Try again in {seconds} seconds.
empty.issues.clean            No issues in this category as of the last scan ({time}).
empty.issues.no_scan          No scan has run yet. Run a scan to look for issues.
empty.issues.incomplete       The last scan was incomplete, so this list may be missing items.
empty.issues.stale            The last scan is old, so this list may be out of date.
empty.filtered                No items match your filters.
empty.history                 No actions recorded yet.
flash.plan_created            Plan created. Nothing has been changed yet.
flash.plan_approved           Plan approved. Nothing has run yet.
flash.plan_discarded          Plan discarded.
flash.job_enqueued            Job queued.
flash.execution_done          Execution finished: {ok} of {total} actions succeeded.
flash.simulation_done         Simulation finished. No file was changed.
flash.ignored                 {count} items ignored.
flash.restored                {count} items restored.
flash.partial                 Partially completed: {ok} of {total} actions succeeded.
```
Settings (only when built)
```
settings.title                Settings
settings.read_only            Read only. Settings are set in the configuration file and environment.
settings.capabilities         Capabilities
settings.capability_on        On
settings.capability_off       Off
settings.roots                Roots
settings.thresholds           Thresholds
settings.version              Version
```
Explanations (reporting `ExplanationVm` codes; one `explain.<code>` key with params each, translated by `code`)
```
explain.scan.none | explain.scan.complete_with_errors | explain.scan.not_complete | explain.scan.root_not_scanned | explain.scan.root_errors |
explain.scan.stale | explain.issues.from_older_session | explain.issues.incomplete_evidence | explain.service.not_ok | explain.auto.paused |
explain.plan.dry_run | explain.plan.not_finished | explain.plan.unfinished_intents | explain.action.not_verified | explain.gain.not_recorded |
explain.gain.differs_from_plan | explain.gain.total_differs_from_plan | explain.cost.differs_from_plan | explain.observed.differs_from_attributable |
explain.observed.missing | explain.caveat.btrfs_snapshots_may_retain_space
next_step.wait_or_recover | next_step.run_or_discard | next_step.verify_manually | next_step.review_manually | next_step.rescan_and_replan
```
Existing keys superseded by exact duplicates: `issues.refresh` equals `command.refresh` (keep both; templates use `issues.refresh` in
lists). `issues.selection_unknown` ("Not found") is too terse in a count line; the spec composes it as `{count} no longer found` using the new key
`issues.sel_unknown_count` ("{count} no longer found").

### 12.3 Coverage rule for web-ui tests

A test renders every page with a pseudo-locale fake (`tr` returns `[key]`) and asserts no text node outside markup tokens remains
(a template lint: no literal letter run in templates outside `{{ }}` and attributes `class`, `id`, `name`, `href`, `type`).
A second test asserts `en.json` and `fr.json` have the same key set and every `{param}` in `en` exists in `fr`.

## 13. Degradation for routes not built

| Missing | Behaviour until built |
|---|---|
| `GET /plans` | No Plans nav entry; plans linked from job, issue and dashboard pages |
| `/media/*`, `/explorer`, `/settings` | No nav entry; no link anywhere |
| `/stats/chart/{kind}.svg` | Chart is inline `<svg>` in `/stats`, no separate route needed; the route is optional |
| `/static/*` | 19 builds it; the page works without CSS/JS as unstyled forms |
| `select_all` param, `hide_recent` count, history filters, flash codes | see handoff open questions |

## 14. Acceptance checklist (for 19 and QA)

1. 320, 360, 768, 1280 px: no horizontal page scroll; no issue list is a `<table>`.
2. A greyscale screenshot still distinguishes green, orange and red states by glyph and text.
3. No checkbox is `checked` in any server-rendered list or confirmation form; the `confirm` page has no pre-filled typed counts.
4. No single request both approves and executes; `POST /plans` never executes; approve redirects to the plan page.
5. Confirmation shows target, count, space impact (labelled kinds), irreversible effects, torrent removals, acknowledgements.
6. All interactive controls measure >= 44 px in a layout test; focus ring visible in every theme.
7. `prefers-reduced-motion` removes every animation and transition.
8. i18n lint (12.3) passes; `fr` key set equals `en`.
9. Every state in section 8 exists as a fixture and renders (loading dimming, 8 empty variants, stale, incomplete, error, cancelled, success).
10. Totals, selection count and truncation notice agree with the visible list in a fixture of 1200 rows.
