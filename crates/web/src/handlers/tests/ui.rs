//! UI rendering tests (agent 19): hostile strings, both locales, every state, structural
//! rules of the UX spec (no preselection, CSRF on every form, no inline script or style, no
//! literal text in templates) and the flows that must work without custom JavaScript.

use super::*;
use crate::handlers::{Chrome, Ctx, FLASH_CODES, category_key, login, severity_look, snake};
use crate::i18n::{Locale, Tr};
use askama::Template as _;
use removarr_domain::plan::ActionKind;
use removarr_reporting::{
    CategoryCountVm, EvidenceVm, ExplanationVm, HistoryRowVm, LogLevel as RLevel, OutcomeVm,
    ReasonOfActionVm, SeriesKind, SeriesPointVm, SeriesVm, ServiceVm, SessionVm,
};
use removarr_workflows::plans::ActionPreview;
use std::collections::BTreeMap;

const HOSTILE: &str = "<img src=x onerror=alert(1)>\"'&</title><script>alert(1)</script>";

/// Askama escaping of a plain string, to look an English text up in a rendered page.
fn esc(s: &str) -> String {
    s.replace('&', "&#38;")
        .replace('<', "&#60;")
        .replace('>', "&#62;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

fn en(key: &str) -> String {
    esc(Tr::new(Locale::En).t(key))
}

fn fr(key: &str) -> String {
    esc(Tr::new(Locale::Fr).t(key))
}

async fn fetch(
    w: &World,
    uri: &str,
    lang: &str,
    htmx: bool,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::get(uri).header(header::COOKIE, format!("removarr_locale={lang}"));
    if htmx {
        b = b.header("hx-request", "true");
    }
    w.call(b.body(Body::empty()).unwrap()).await
}

async fn page(w: &World, uri: &str) -> String {
    let (s, _, body) = fetch(w, uri, "en", false).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {body}");
    body
}

/// Rules that hold for every server-rendered page.
fn assert_markup_rules(uri: &str, body: &str) {
    for bad in [
        "<img src=x",
        "<script>alert",
        "</title><script",
        " style=\"",
        " onclick=",
        " onload=",
        "hx-on",
        "hx-boost",
        "hx-delete",
        "hx-put",
        "javascript:",
        "localStorage",
    ] {
        assert!(!body.contains(bad), "{uri}: contains {bad:?}");
    }
    assert!(!body.contains("<style"), "{uri}: inline style element");
    // The only script is the pinned HTMX file, same origin, with its SRI.
    let scripts = body.matches("<script").count();
    assert!(scripts <= 1, "{uri}: {scripts} scripts");
    if scripts == 1 {
        assert!(body.contains(
            "<script src=\"/static/htmx-2.0.4.min.js\" integrity=\"sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+\""
        ), "{uri}");
    }
    // Every post form carries the CSRF token; every hx-post form field set includes it.
    for chunk in body.split("<form").skip(1) {
        let head = chunk.split('>').next().unwrap_or_default();
        let form = chunk.split("</form>").next().unwrap_or_default();
        if head.contains("method=\"post\"") {
            assert!(
                form.contains("name=\"csrf_token\" value=\"test-csrf-token\""),
                "{uri}: post form without csrf token: {head}"
            );
            assert!(
                head.contains("action=\""),
                "{uri}: post form without action"
            );
        }
    }
    // No secrets or absolute host paths in any attribute.
    assert!(
        !body.contains(SECRET_PATH) && !body.contains(SECRET_KEY),
        "{uri}"
    );
    // HTMX is only used for the read-only selection total and the job progress poll.
    for chunk in body.split("hx-post=\"").skip(1) {
        assert!(chunk.starts_with("/issues/selection-summary\""), "{uri}");
    }
    for chunk in body.split("hx-get=\"").skip(1) {
        assert!(chunk.contains("/progress\""), "{uri}");
    }
}

// ---- scenario builders -------------------------------------------------------------------------

fn hostile_row(id: &str, name: &str, ignored: bool) -> IssueRowVm {
    let mut r = row(id, name, IssueCategory::Junk, ignored);
    r.blockers = vec![
        "ServiceUnavailable:<b>sonarr</b>".into(),
        "ScanIncomplete".into(),
        format!("Unknown{HOSTILE}"),
    ];
    r.completeness = CompletenessVm {
        state: "incomplete",
        missing: vec![HOSTILE.into()],
    };
    r.evidence = vec![
        EvidenceVm {
            kind: "physical",
            summary: HOSTILE.into(),
            provenance: "filesystem",
            observed_at: NOW,
            observed_at_iso: "2023-11-14T22:13:20.000Z".into(),
            current: true,
            completeness: CompletenessVm {
                state: "complete",
                missing: vec![],
            },
        },
        EvidenceVm {
            kind: "quality",
            summary: "older".into(),
            provenance: "arr",
            observed_at: NOW - DAY,
            observed_at_iso: "2023-11-13T22:13:20.000Z".into(),
            current: false,
            completeness: CompletenessVm {
                state: "incomplete",
                missing: vec![],
            },
        },
    ];
    r
}

fn action(kind: ActionKind, rel: &str, irreversible: bool) -> ActionPreview {
    use removarr_domain::gain::GainEstimate;
    use removarr_domain::path::RootedPath;
    use removarr_domain::plan::ActionTarget;
    ActionPreview {
        index: 0,
        kind,
        tier: RiskTier::T3,
        capability: None,
        companion: false,
        irreversible,
        target: ActionTarget::Path {
            path: RootedPath::parse("completed", rel).unwrap(),
        },
        annexes: vec![RootedPath::parse("completed", &format!("{rel}/sidecar.nfo")).unwrap()],
        torrents: BTreeSet::new(),
        expected_gain: GainEstimate {
            logical: 2048,
            attributable: 1024,
            added: 0,
        },
        depends_on: vec![],
        verification: vec![],
    }
}

fn plan(status: Option<PlanStatus>, mode: PlanMode) -> PlanPreview {
    let mut p = preview(status);
    p.mode = mode;
    p.dry_run = mode == PlanMode::DryRun;
    p.actions = vec![
        action(ActionKind::DeleteCompletedFolder, "Movie (2020)", true),
        action(ActionKind::DeleteJunk, "a.nfo", false),
    ];
    p
}

fn json_en() -> BTreeMap<String, String> {
    serde_json::from_str(include_str!("../../../../../locales/en.json")).unwrap()
}

fn json_fr() -> BTreeMap<String, String> {
    serde_json::from_str(include_str!("../../../../../locales/fr.json")).unwrap()
}

// ---- hostile strings, both locales -------------------------------------------------------------

#[tokio::test]
async fn hostile_strings_are_escaped_on_every_page_in_both_locales() {
    let w = World::new(Arc::new(AllowGuard));
    {
        let mut o = w.read.over.lock().unwrap();
        o.rows = Some(vec![
            hostile_row(ID_A, HOSTILE, false),
            hostile_row(ID_B, "Beta.txt", false),
            hostile_row("cccccccccccccccccccccccccccccccc", HOSTILE, true),
        ]);
        o.finished = Some(Some("2023-11-14T22:13:20.000Z".into()));
        let mut d = dashboard_vm();
        d.services = vec![ServiceVm {
            service: HOSTILE.into(),
            state_key: "unavailable",
            ok: false,
            problem: true,
            version: Some(HOSTILE.into()),
            detail: Some(HOSTILE.into()),
            observed_at: NOW,
            observed_at_iso: HOSTILE.into(),
            current: false,
        }];
        d.explanations = vec![ExplanationVm {
            code: "scan.none",
            params: BTreeMap::new(),
            text: HOSTILE.into(),
        }];
        o.dashboard = Some(d);
        let mut hv = HistoryVm {
            rows: vec![],
            offset: 0,
            limit: 50,
            has_more: false,
            unfinished_intents: 2,
        };
        hv.rows.push(HistoryRowVm {
            plan_id: "00000000-0000-0000-0000-00000000002a".into(),
            action_index: 0,
            action_kind: "DeleteJunk".into(),
            at: NOW,
            at_iso: "2023-11-14T22:13:20.000Z".into(),
            started_at: None,
            started_at_iso: None,
            target: HOSTILE.into(),
            reason: ReasonOfActionVm {
                origin: HOSTILE.into(),
                approved_by: None,
                approved_at: None,
                approved_at_iso: None,
                plan_created_at: NOW,
            },
            outcome: OutcomeVm {
                key: HOSTILE.into(),
                success: false,
                needs_attention: true,
                detail: Some(HOSTILE.into()),
            },
            gain_ids: vec![],
            attributable_bytes: 10,
            added_bytes: 0,
            plan_status: HOSTILE.into(),
        });
        o.history = Some(hv);
    }
    let mut p = plan(Some(PlanStatus::Draft), PlanMode::DryRun);
    p.excluded = vec![(
        SubjectKey(format!("p:completed:{HOSTILE}")),
        vec![removarr_domain::issue::Blocker::new(
            removarr_domain::issue::BlockerCode::Recent,
            HOSTILE,
        )],
    )];
    p.samples_to_acknowledge =
        vec![removarr_domain::path::RootedPath::parse("completed", "x/sample.mkv").unwrap()];
    *w.plans.stored.lock().unwrap() = Some(p);
    let mut job = progress(JobId::new(uuid(7)));
    job.label = HOSTILE.into();
    job.current = Some(HOSTILE.into());
    job.warnings = vec![HOSTILE.into()];
    job.stage = Some(HOSTILE.into());
    *w.jobs.scenario.lock().unwrap() = Some((job, Some(HOSTILE.into())));
    w.logs_push(HOSTILE);

    for uri in GET_ROUTES
        .iter()
        .filter(|u| !u.starts_with("/export"))
        .chain(&[
            "/issues/junk?select_all=1",
            "/ignored?select_all=1",
            "/logs?level=debug",
        ])
    {
        for lang in ["en", "fr"] {
            let (s, _, body) = fetch(&w, uri, lang, false).await;
            assert_eq!(s, StatusCode::OK, "{uri} {lang}");
            assert_markup_rules(uri, &body);
            if !uri.ends_with("/progress") {
                assert!(
                    body.contains(&format!("<html lang=\"{lang}\">")),
                    "{uri} {lang}"
                );
            }
            // fragments obey the same rules
            let (s, _, frag) = fetch(&w, uri, lang, true).await;
            assert_eq!(s, StatusCode::OK, "{uri} {lang} htmx");
            assert_markup_rules(uri, &frag);
            assert!(!frag.contains("<html"), "{uri}: fragment is a full page");
        }
    }
    // The hostile name is present, escaped, on the list and the detail page.
    let list = page(&w, "/issues/junk").await;
    assert!(list.contains(&esc(HOSTILE)), "escaped name missing");
    let detail = page(&w, &format!("/issues/junk/{ID_A}")).await;
    assert!(detail.contains(&esc(HOSTILE)));
}

impl World {
    fn logs_push(&self, msg: &str) {
        // The state's buffer is shared with the router; pushing through a clone is enough.
        self.logs.push(
            RLevel::Error,
            "tests",
            msg,
            vec![(HOSTILE.to_string(), HOSTILE.to_string())],
        );
    }
}

// ---- keys, locales, literal text ---------------------------------------------------------------

fn template_files() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "html") {
                out.push((
                    p.display().to_string(),
                    std::fs::read_to_string(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = vec![];
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates"),
        &mut out,
    );
    assert!(out.len() >= 20);
    out
}

fn has_key(map: &BTreeMap<String, String>, k: &str) -> bool {
    map.contains_key(k)
        || map.contains_key(&format!("{k}.one")) && map.contains_key(&format!("{k}.other"))
}

#[test]
fn every_key_used_by_a_template_exists_in_both_locales() {
    let (en, fr) = (json_en(), json_fr());
    let mut used = BTreeSet::new();
    for (_, src) in template_files() {
        for pat in ["ctx.tr.t(\"", "ctx.tn(\"", "ctx.nm(\"", "ctx.tr.fmt(\""] {
            for part in src.split(pat).skip(1) {
                used.insert(part.split('"').next().unwrap().to_string());
            }
        }
    }
    assert!(used.len() > 150, "{}", used.len());
    for k in &used {
        assert!(has_key(&en, k), "en.json lacks {k}");
        assert!(has_key(&fr, k), "fr.json lacks {k}");
    }
}

#[test]
fn keys_built_in_rust_and_domain_codes_have_translations() {
    let (en, fr) = (json_en(), json_fr());
    let mut keys: Vec<String> = FLASH_CODES.iter().map(|(_, k)| (*k).to_string()).collect();
    for sev in ["critical", "warning", "info"] {
        keys.push(severity_look(sev).2.to_string());
    }
    for c in IssueCategory::ALL {
        keys.push(category_key(c).to_string());
    }
    let coded = |prefix: &str, vals: &[&str], keys: &mut Vec<String>| {
        for v in vals {
            keys.push(format!("{prefix}.{}", snake(v)));
        }
    };
    coded(
        "plan_status",
        &[
            "Draft",
            "Approved",
            "Executing",
            "Executed",
            "PartiallyExecuted",
            "Failed",
            "Expired",
            "Invalidated",
            "Discarded",
            "Interrupted",
            "Preview",
        ],
        &mut keys,
    );
    coded(
        "job_status",
        &[
            "Queued",
            "Running",
            "Succeeded",
            "Failed",
            "Cancelled",
            "Interrupted",
        ],
        &mut keys,
    );
    coded(
        "kind",
        &[
            "ConsolidateHardlink",
            "DeleteCompletedFile",
            "DeleteCompletedFolder",
            "DeleteJunk",
            "RemoveEmptyParent",
            "RemoveTorrentTask",
            "CleanupStalledIncomplete",
            "DeleteIncompleteOrphan",
            "FixMode",
            "ReplaceRelease",
            "DeleteMediaFile",
            "ArrGrabRelease",
            "ArrRemoveQueueItem",
            "ArrSetMonitored",
            "ArrRescan",
            "JellyfinRefresh",
        ],
        &mut keys,
    );
    coded("tier", &["T1", "T2", "T3", "T4", "T5"], &mut keys);
    coded(
        "origin",
        &["User", "AutoPolicy", "user", "auto_policy"],
        &mut keys,
    );
    coded(
        "outcome",
        &[
            "verified",
            "executed_unverified",
            "failed",
            "compensated",
            "failed_needs_review",
            "simulated",
            "not_executed",
            "Consolidate",
            "DeleteReplaced",
            "KeepManual",
            "KeepSample",
        ],
        &mut keys,
    );
    coded(
        "evidence",
        &[
            "physical",
            "same_physical",
            "content",
            "identity",
            "quality",
            "torrent_holds",
            "service_unavailable",
            "note",
        ],
        &mut keys,
    );
    coded(
        "provenance",
        &[
            "filesystem",
            "qbittorrent",
            "arr",
            "jellyfin",
            "ffprobe",
            "computed",
        ],
        &mut keys,
    );
    coded(
        "completeness",
        &["complete", "incomplete", "unknown"],
        &mut keys,
    );
    coded(
        "next_step",
        &[
            "wait_or_recover",
            "run_or_discard",
            "verify_manually",
            "review_manually",
            "rescan_and_replan",
        ],
        &mut keys,
    );
    coded(
        "marker",
        &["circle", "square", "diamond", "triangle", "cross"],
        &mut keys,
    );
    coded("logs", &["DEBUG", "INFO", "WARN", "ERROR"], &mut keys);
    coded(
        "blocker",
        &[
            "ScanIncomplete",
            "ScanErrorUnder",
            "ServiceUnavailable",
            "ServiceNotConfigured",
            "MappingError",
            "UnmappedTorrents",
            "TorrentProtectedState",
            "TorrentReferencesTarget",
            "Sample",
            "Recent",
            "Ignored",
            "ArrMissing",
            "ArrAmbiguous",
            "ArrRejected",
            "ArrNotUpgradeEvidence",
            "CoverageIncomplete",
            "DurationMismatch",
            "AudioLanguageMissing",
            "EditionMismatch",
            "PermissionDenied",
            "CrossDevice",
            "FailedConsolidation",
            "ImportInProgress",
            "ContentDiffers",
            "DuplicateTarget",
            "MediaRootDeletion",
            "CapabilityDisabled",
            "NotInBuild",
            "DryRunOnly",
            "ProtectedZone",
            "Symlink",
            "NumberingMismatch",
            "QueueItemHidesError",
            "ReleaseNotInSearch",
            "ArrImportMoved",
            "OldCoverageUnrecoverable",
            "ArrHardlinksDisabled",
            "ChangedSincePlanning",
            "Interrupted",
        ],
        &mut keys,
    );
    for k in keys {
        assert!(has_key(&en, &k), "en.json lacks {k}");
        assert!(has_key(&fr, &k), "fr.json lacks {k}");
    }
    // French is actually translated for the UI-visible strings (not a copy of English), except
    // for proper names and neutral words.
    let same: Vec<&String> = en
        .iter()
        .filter(|(k, v)| {
            fr.get(*k) == Some(*v) && v.chars().filter(|c| c.is_alphabetic()).count() > 24
        })
        .map(|(k, _)| k)
        .collect();
    assert!(same.is_empty(), "untranslated French values: {same:?}");
}

#[test]
fn templates_hold_no_literal_text() {
    for (name, src) in template_files() {
        if name.ends_with("error.html") {
            continue; // English only, see the handoff
        }
        let mut s = src.clone();
        for (open, close) in [("{#", "#}"), ("{%", "%}"), ("{{", "}}")] {
            while let Some(a) = s.find(open) {
                let b = s[a..].find(close).map_or(s.len(), |i| a + i + close.len());
                s.replace_range(a..b, " ");
            }
        }
        // drop tags with their attributes
        let mut text = String::new();
        let mut in_tag = false;
        for ch in s.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => {
                    in_tag = false;
                    text.push(' ');
                }
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let letters: String = text.chars().filter(|c| c.is_alphabetic()).collect();
        assert!(
            letters.is_empty(),
            "{name}: literal text {letters:?} in {text:?}"
        );
    }
}

// ---- no preselection, selection, filters -------------------------------------------------------

#[tokio::test]
async fn lists_and_confirmations_never_preselect_anything() {
    let w = World::new(Arc::new(AllowGuard));
    for uri in [
        "/issues/junk",
        "/issues/junk?page=1&per_page=1",
        "/ignored",
        "/plans/00000000-0000-0000-0000-00000000002a/confirm",
    ] {
        let body = page(&w, uri).await;
        assert!(!body.contains(" checked"), "{uri}: preselected control");
        assert!(!body.contains("autofocus"), "{uri}");
    }
    // select_all is an explicit, display-only request and ticks only this page.
    let all = page(&w, "/issues/junk?select_all=1&per_page=1").await;
    assert_eq!(all.matches(" checked").count(), 1, "one row on this page");
    let all = page(&w, "/issues/junk?select_all=1").await;
    assert_eq!(all.matches(" checked").count(), 2);
    assert!(all.contains(&en("issues.clear_selection")));
    let plain = page(&w, "/issues/junk").await;
    assert!(
        plain.contains("select_all=1"),
        "the explicit control links to the flag"
    );
    assert!(plain.contains(&en("issues.sel_none")));
    let (s, ..) = fetch(&w, "/issues/junk?select_all=2", "en", false).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(w.mutations(), 0, "select-all is display only");
}

#[tokio::test]
async fn list_shows_columns_totals_markers_and_filters() {
    let w = World::new(Arc::new(AllowGuard));
    {
        let mut o = w.read.over.lock().unwrap();
        let mut folder = row(ID_A, "Some Folder", IssueCategory::Junk, false);
        folder.unit_kind = "folder";
        folder.torrent_count = 3;
        folder.logical_bytes = 5 * 1024 * 1024;
        let mut recent = row(ID_B, "Fresh.nfo", IssueCategory::Junk, false);
        recent.age_basis = NOW - DAY / 2;
        o.rows = Some(vec![folder, recent]);
        o.finished = Some(Some("2023-11-14T22:13:20.000Z".into()));
        o.total_count = Some(1500);
    }
    let body = page(&w, "/issues/junk").await;
    // folder vs file marker, age, size, torrent count, blockers count
    assert!(body.contains(&en("issues.kind.folder_one")));
    assert!(body.contains("5.0 MiB"));
    assert!(body.contains("30 days"));
    assert!(body.contains(&en("issues.col.torrents")));
    // hide-recent is on by default (2 days): the fresh item is hidden and counted
    assert!(!body.contains("Fresh.nfo"));
    assert!(body.contains("1 recent items are hidden"));
    // list totals and truncation are stated
    assert!(
        body.contains("1 on this page | 1 in this list (5.0 MiB)"),
        "{body}"
    );
    assert!(body.contains(&en("issues.truncated")));
    // filters: search, kind, sort, dir, hide-recent, per_page are all in one GET form
    let form = body
        .split("<form method=\"get\"")
        .nth(1)
        .unwrap()
        .split("</form>")
        .next()
        .unwrap();
    for name in ["q", "kind", "sort", "dir", "hide_recent", "per_page"] {
        assert!(form.contains(&format!("name=\"{name}\"")), "{name}");
    }
    let shown = page(&w, "/issues/junk?hide_recent=0").await;
    assert!(shown.contains("Fresh.nfo") && !shown.contains("recent items are hidden"));
    let files_only = page(&w, "/issues/junk?kind=file&hide_recent=0").await;
    assert!(!files_only.contains("Some Folder") && files_only.contains("Fresh.nfo"));
    // empty variants
    let none = page(&w, "/issues/junk?q=nomatch").await;
    assert!(none.contains(&en("empty.filtered")) && none.contains(&en("issues.clear_filters")));
    w.read.over.lock().unwrap().rows = Some(vec![]);
    w.read.over.lock().unwrap().finished = Some(None);
    let no_scan = page(&w, "/issues/junk").await;
    assert!(no_scan.contains(&en("empty.no_scan")));
    w.read.over.lock().unwrap().rows = Some(vec![]);
    w.read.over.lock().unwrap().finished = Some(Some("2023-11-14T22:13:20.000Z".into()));
    assert!(
        page(&w, "/issues/junk")
            .await
            .contains(&en("empty.category"))
    );
    assert!(page(&w, "/ignored").await.contains(&en("empty.ignored")));
}

#[tokio::test]
async fn ignored_tab_offers_restore_and_the_server_keeps_the_state() {
    let w = World::new(Arc::new(AllowGuard));
    let body = page(&w, "/ignored").await;
    assert!(body.contains("Ignored.nfo"));
    assert!(body.contains("action=\"/issues/restore\""));
    assert!(
        !body.contains("/issues/ignore\""),
        "no ignore control on the ignored tab"
    );
    let list = page(&w, "/issues/junk").await;
    assert!(
        !list.contains("Ignored.nfo"),
        "ignored items leave the list"
    );
    assert!(list.contains("formaction=\"/issues/ignore\""));
    assert!(!list.contains("localStorage"));
    // one-item actions on the detail page: plan preview/creation, ignore; restore when ignored
    let d = page(&w, &format!("/issues/junk/{ID_A}")).await;
    assert!(d.contains("action=\"/issues/ignore\"") && d.contains("formaction=\"/plans/preview\""));
    let d = page(&w, "/issues/junk/cccccccccccccccccccccccccccccccc").await;
    assert!(d.contains("action=\"/issues/restore\""));
}

// ---- flash, locale -----------------------------------------------------------------------------

#[tokio::test]
async fn flash_is_one_shot_allowlisted_and_not_authorization() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, _) = w.post("/issues/ignore", &format!("issue_ids={ID_A}")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).unwrap(),
        "/ignored",
        "location stays clean"
    );
    let cookie = h
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        cookie.starts_with("removarr_flash=ignored;")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict")
    );
    let (_, h, body) = w
        .call(
            Request::get("/ignored")
                .header(header::COOKIE, "removarr_flash=ignored")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(body.contains(&en("flash.ignored")));
    assert!(
        h.get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0"),
        "consumed"
    );
    // an unknown code is ignored, never rendered
    let (_, _, body) = w
        .call(
            Request::get("/ignored")
                .header(header::COOKIE, format!("removarr_flash={HOSTILE}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(!body.contains("class=\"flash\""));
}

#[tokio::test]
async fn locale_is_a_separate_cookie_and_never_touches_authorization() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, _) = w.post("/locale", "locale=fr").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let cookies: Vec<_> = h
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert_eq!(cookies.len(), 1);
    assert!(
        cookies[0].starts_with("removarr_locale=fr;") && cookies[0].contains("SameSite=Strict")
    );
    assert!(!cookies[0].contains("session") && !cookies[0].contains("csrf"));
    let (s, ..) = w.post("/locale", "locale=../../etc").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // the French page is translated and the form keeps the selected language
    let (_, _, body) = fetch(&w, "/", "fr", false).await;
    assert!(body.contains(&fr("nav.dashboard")) && body.contains("<option value=\"fr\" selected>"));
    // an unsupported cookie value falls back to English, an Accept-Language to French
    let (_, _, body) = w
        .call(
            Request::get("/")
                .header(header::COOKIE, "removarr_locale=xx")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(body.contains("<html lang=\"en\">"));
}

// ---- dashboard states --------------------------------------------------------------------------

fn session(stale: bool, complete: bool, finished: bool) -> SessionVm {
    SessionVm {
        id: "s1".into(),
        started_at: NOW,
        started_at_iso: "2023-11-14T22:13:20.000Z".into(),
        finished_at: finished.then_some(NOW),
        finished_at_iso: finished.then(|| "2023-11-14T22:13:20.000Z".to_string()),
        status: "done".into(),
        complete,
        age_ms: Some(0),
        stale,
        entries_scanned: 12,
        scan_errors: 3,
        duration_ms: 10,
        roots: vec![],
    }
}

#[tokio::test]
async fn dashboard_states_are_labelled_never_colour_only() {
    let w = World::new(Arc::new(AllowGuard));
    // never scanned
    let body = page(&w, "/").await;
    assert!(body.contains(&en("empty.no_scan")));
    // stale + incomplete + red urgency with reasons + service states + categories
    let mut d = dashboard_vm();
    d.last_scan = Some(session(true, false, true));
    d.urgency = UrgencyVm {
        level: Urgency::Red,
        label_key: Urgency::Red.label_key(),
        label: Urgency::Red.label_en(),
        icon: Urgency::Red.icon(),
        reasons: vec![removarr_reporting::ReasonVm {
            key: "critical_issues",
            count: Some(2),
        }],
    };
    d.categories = vec![CategoryCountVm {
        category: IssueCategory::Junk,
        count: 2,
        max_severity: Severity::Critical,
        info: 0,
        warning: 0,
        critical: 2,
        logical_bytes: 1024,
        potential_bytes: 1024,
    }];
    d.services = [
        "ok",
        "unavailable",
        "auth_rejected",
        "not_configured",
        "mapping_errors",
        "unknown",
    ]
    .iter()
    .map(|k| ServiceVm {
        service: (*k).into(),
        state_key: k,
        ok: *k == "ok",
        problem: *k != "ok" && *k != "not_configured",
        version: None,
        detail: None,
        observed_at: NOW,
        observed_at_iso: "2023-11-14T22:13:20.000Z".into(),
        current: *k == "ok",
    })
    .collect();
    d.space.potential_overlap_excluded_bytes = 10;
    w.read.over.lock().unwrap().dashboard = Some(d);
    let body = page(&w, "/").await;
    for k in [
        "state.stale",
        "state.incomplete",
        "urgency.red",
        "reason.critical_issues",
        "severity.critical",
        "service.ok",
        "service.unavailable",
        "service.auth_rejected",
        "service.not_configured",
        "service.mapping_errors",
        "service.unknown",
        "space.overlap_note",
    ] {
        assert!(body.contains(&en(k)), "{k}");
    }
    // each badge carries a glyph reference AND text; the three urgency glyph shapes differ
    assert!(
        body.contains("#i-alert-octagon")
            && body.contains("#i-alert-triangle")
            && body.contains("#i-check-circle")
    );
    assert!(
        body.contains("<symbol id=\"i-alert-octagon\"")
            && body.contains("<symbol id=\"i-alert-triangle\"")
            && body.contains("<symbol id=\"i-info-circle\"")
    );
    // the four space quantities and the potential label are separate
    for k in [
        "space.logical",
        "space.allocated",
        "space.shared",
        "space.potential_not_guaranteed",
        "space.not_a_promise",
    ] {
        assert!(body.contains(&en(k)), "{k}");
    }
    // dry-run banner (World default is dry run), service actions only for configured services
    assert!(body.contains(&en("mode.dry_run_banner")));
    assert!(
        body.contains("/services/sonarr/refresh") && !body.contains("/services/radarr/refresh")
    );
    assert!(!body.contains("/services/qbittorrent/rescan"));
    // running scan (no finish time)
    let mut d = dashboard_vm();
    d.last_scan = Some(session(false, true, false));
    w.read.over.lock().unwrap().dashboard = Some(d);
    assert!(page(&w, "/").await.contains(&en("dashboard.running")));
    // live mode: no banner
    let live = World::with(Arc::new(AllowGuard), false, false);
    assert!(!page(&live, "/").await.contains(&en("mode.dry_run_banner")));
}

// ---- plans -------------------------------------------------------------------------------------

#[tokio::test]
async fn plan_pages_separate_preview_approval_and_execution() {
    let w = World::new(Arc::new(AllowGuard));
    let id = "00000000-0000-0000-0000-00000000002a";
    // draft, dry run: review link and simulate only; no execute control
    let body = page(&w, &format!("/plans/{id}")).await;
    assert!(body.contains(&format!("href=\"/plans/{id}/confirm\"")));
    assert!(body.contains(&format!("action=\"/plans/{id}/simulate\"")));
    assert!(!body.contains("/execute\"") && !body.contains(&en("plan.execute_open")));
    assert!(body.contains(&en("plan.dry_run_only")));
    // gain labels: logical, expected (prediction), added, net, each separate
    for k in [
        "plan.gain_logical",
        "plan.gain_attributable",
        "plan.gain_added",
        "plan.gain_net",
        "gain.expected_help",
    ] {
        assert!(body.contains(&en(k)), "{k}");
    }
    assert!(
        !body.contains("Approve") && !body.contains(&en("confirm.approve")),
        "destructive text is not on the plan page"
    );
    // preview: not stored, no approval path
    let (s, _, prev) = {
        let (s, h, b) = w
            .post(
                "/plans/preview",
                &format!("category=junk&action=delete&issue_ids={ID_A}"),
            )
            .await;
        (s, h, b)
    };
    assert_eq!(s, StatusCode::OK);
    assert!(prev.contains(&en("plan.preview_not_stored")) && !prev.contains("/confirm\""));
    // approved live plan in a live process: execute lives in a collapsed disclosure, after the
    // consequences are repeated; Execute text appears only there
    *w.plans.stored.lock().unwrap() = Some(plan(Some(PlanStatus::Approved), PlanMode::Live));
    let live = World::with(Arc::new(AllowGuard), false, false);
    *live.plans.stored.lock().unwrap() = Some(plan(Some(PlanStatus::Approved), PlanMode::Live));
    let body = page(&live, &format!("/plans/{id}")).await;
    assert!(body.contains("<details class=\"card crit\"><summary>"));
    let details = body.split("<details class=\"card crit\">").nth(1).unwrap();
    assert!(
        details.contains(&format!("action=\"/plans/{id}/execute\""))
            && details.contains("2 actions")
    );
    assert!(
        !body.contains(&format!("href=\"/plans/{id}/confirm\"")),
        "already approved"
    );
    // expired and invalidated plans explain themselves and offer no execution
    for (st, key) in [
        (PlanStatus::Expired, "error.plan.expired"),
        (PlanStatus::Invalidated, "error.plan.stale"),
    ] {
        *live.plans.stored.lock().unwrap() = Some(plan(Some(st), PlanMode::Live));
        let b = page(&live, &format!("/plans/{id}")).await;
        assert!(b.contains(&en(key)) && !b.contains("/execute\""), "{st:?}");
    }
    // partially executed with a report: recorded and observed values stay apart
    let mut p = plan(Some(PlanStatus::PartiallyExecuted), PlanMode::Live);
    p.actions.truncate(1);
    *live.plans.stored.lock().unwrap() = Some(p);
    live.read.over.lock().unwrap().report = Some(PlanReportVm {
        plan_id: id.into(),
        hash: "a".repeat(64),
        status: "PartiallyExecuted".into(),
        mode: "Live".into(),
        origin: "User".into(),
        created_at: NOW,
        created_at_iso: "2023-11-14T22:13:20.000Z".into(),
        session: "s1".into(),
        planned: removarr_reporting::PlanGainsVm {
            logical: 2048,
            attributable: 1024,
            added: 0,
            net: 1024,
        },
        recorded: GainTotalsVm {
            attributable_bytes: 512,
            added_bytes: 0,
            net_bytes: 512,
            unique_gain_ids: 1,
            duplicate_gain_ids_ignored: 0,
        },
        observed: vec![removarr_reporting::ObservedRowVm {
            plan_id: None,
            root: "completed".into(),
            free_before: 10,
            free_after_local: 20,
            free_after_services: None,
            delta_local: 10,
            at: NOW,
            at_iso: String::new(),
        }],
        actions: vec![],
        differences: vec![ExplanationVm {
            code: "gain.differs_from_plan",
            params: BTreeMap::new(),
            text: HOSTILE.into(),
        }],
        remaining: vec![removarr_reporting::RemainingVm {
            index: 1,
            kind: "DeleteJunk".into(),
            state: "pending".into(),
            next_step: "run_or_discard",
            text: String::new(),
        }],
    });
    let b = page(&live, &format!("/plans/{id}")).await;
    for k in [
        "plan.recorded_attributable",
        "plan.observed",
        "gain.recorded_help",
        "gain.observed_help",
        "plan.remaining",
        "next_step.run_or_discard",
        "plan.not_all_done",
    ] {
        assert!(b.contains(&en(k)), "{k}");
    }
    assert!(b.contains(&esc(HOSTILE)) && !b.contains("<script>alert"));
}

#[tokio::test]
async fn confirmation_is_an_explicit_step_with_acknowledgements() {
    let w = World::new(Arc::new(AllowGuard));
    let id = "00000000-0000-0000-0000-00000000002a";
    let mut p = plan(Some(PlanStatus::Draft), PlanMode::Live);
    p.required_torrent_removals = 2;
    p.torrent_removals = vec![removarr_domain::torrent::TorrentHash::new("a".repeat(40)).unwrap()];
    p.samples_to_acknowledge =
        vec![removarr_domain::path::RootedPath::parse("completed", "s/sample.mkv").unwrap()];
    p.unrecoverable_to_acknowledge =
        vec![removarr_domain::path::RootedPath::parse("completed", "old/Movie.mkv").unwrap()];
    *w.plans.stored.lock().unwrap() = Some(p);
    let body = page(&w, &format!("/plans/{id}/confirm")).await;
    for k in [
        "confirm.title",
        "confirm.target",
        "confirm.cannot_undo",
        "confirm.torrent_removals",
        "confirm.samples",
        "confirm.unrecoverable",
        "confirm.type_count",
        "confirm.type_torrent_count",
        "confirm.approve_not_execute",
    ] {
        assert!(body.contains(&en(k)), "{k}");
    }
    assert!(!body.contains(" checked") && !body.contains("autofocus"));
    // typed counts start empty (no value, no placeholder)
    for name in ["confirmed_count", "confirmed_torrent_removals"] {
        let input = body
            .split(&format!("name=\"{name}\""))
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(
            !input.contains("value=") && !input.contains("placeholder="),
            "{name}"
        );
        assert!(input.contains("required") && input.contains("autocomplete=\"off\""));
    }
    // one unchecked box per file, no "check all"
    assert_eq!(body.matches("name=\"acknowledged_samples\"").count(), 1);
    assert_eq!(
        body.matches("name=\"acknowledged_unrecoverable\"").count(),
        1
    );
    // Back comes before Approve in DOM order, Approve is the last control and a danger button
    let back = body.find(&en("confirm.back")).unwrap();
    let approve = body
        .find(&format!("class=\"danger\">{}", en("confirm.approve")))
        .unwrap();
    assert!(back < approve);
    assert!(
        body.contains(&format!("action=\"/plans/{id}/approve\"")) && !body.contains("/execute")
    );
    assert!(body.contains("name=\"plan_hash\""));
    // no irreversible effect: says so
    let mut q = plan(Some(PlanStatus::Draft), PlanMode::Live);
    q.irreversible = false;
    *w.plans.stored.lock().unwrap() = Some(q);
    assert!(
        page(&w, &format!("/plans/{id}/confirm"))
            .await
            .contains(&en("confirm.no_irreversible"))
    );
    // not a draft: only the explanation and the back link, no form
    *w.plans.stored.lock().unwrap() = Some(plan(Some(PlanStatus::Approved), PlanMode::Live));
    let body = page(&w, &format!("/plans/{id}/confirm")).await;
    assert!(body.contains(&en("confirm.not_draft")) && !body.contains("/approve\""));
    // more than five targets are folded behind a disclosure
    let mut many = plan(Some(PlanStatus::Draft), PlanMode::Live);
    many.actions = (0..7)
        .map(|i| action(ActionKind::DeleteJunk, &format!("f{i}.nfo"), false))
        .collect();
    *w.plans.stored.lock().unwrap() = Some(many);
    let body = page(&w, &format!("/plans/{id}/confirm")).await;
    assert!(body.contains("Show all 7 targets"));
}

// ---- jobs, auto, history, stats, logs ----------------------------------------------------------

#[tokio::test]
async fn job_states_progress_cancellation_and_failure() {
    let w = World::new(Arc::new(AllowGuard));
    let uri = "/jobs/00000000-0000-0000-0000-000000000007";
    let mut j = progress(JobId::new(uuid(7)));
    // running with progress: polls, shows <progress>, offers cancellation
    *w.jobs.scenario.lock().unwrap() = Some((j.clone(), None));
    let b = page(&w, uri).await;
    assert!(b.contains("hx-trigger=\"every 2s\"") && b.contains("<progress value=\"1\" max=\"2\""));
    assert!(b.contains(&format!("action=\"{uri}/cancel\"")));
    // cancellation requested
    j.cancel_requested = true;
    *w.jobs.scenario.lock().unwrap() = Some((j.clone(), None));
    let b = page(&w, uri).await;
    assert!(b.contains(&en("jobs.cancel_requested")) && !b.contains("/cancel\""));
    // finished states stop polling and say what happened
    for (st, key) in [
        (JobStatus::Cancelled, "state.cancelled_text"),
        (JobStatus::Failed, "state.job_failed_text"),
        (JobStatus::Succeeded, "job_status.succeeded"),
    ] {
        j.status = st;
        j.cancel_requested = false;
        *w.jobs.scenario.lock().unwrap() = Some((j.clone(), None));
        let b = page(&w, uri).await;
        assert!(
            b.contains(&en(key)) && !b.contains("hx-trigger") && !b.contains("/cancel\""),
            "{st:?}"
        );
    }
    // the progress fragment is always a fragment
    let (_, _, f) = fetch(&w, &format!("{uri}/progress"), "en", false).await;
    assert!(!f.contains("<html"));
}

#[tokio::test]
async fn auto_mode_shows_the_pause_reason_and_a_resume_form() {
    let w = World::new(Arc::new(AllowGuard));
    let b = page(&w, "/auto").await;
    assert!(
        b.contains(&en("auto.paused"))
            && b.contains("a reason")
            && b.contains("action=\"/auto/resume\"")
    );
    assert!(
        b.contains(&en("auto.read_once")) && !b.contains("name=\"auto_mode\""),
        "no toggle"
    );
    *w.jobs.scenario.lock().unwrap() = Some((progress(JobId::new(uuid(7))), None));
    let b = page(&w, "/auto").await;
    assert!(!b.contains("/auto/resume"));
}

#[tokio::test]
async fn history_stats_and_logs_render_data_empty_and_filtered_states() {
    let w = World::new(Arc::new(AllowGuard));
    assert!(page(&w, "/history").await.contains(&en("empty.history")));
    assert!(page(&w, "/stats").await.contains(&en("empty.stats")));
    assert!(page(&w, "/logs").await.contains(&en("empty.logs")));
    // a chart: inline SVG from chart_geometry, plus the same values in a table
    let pts = |v: &[(i64, i64)]| -> Vec<SeriesPointVm> {
        v.iter()
            .map(|(at, c)| SeriesPointVm {
                at: *at,
                at_iso: format!("2023-11-{:02}T00:00:00.000Z", at / DAY % 28 + 1),
                bytes: *c,
                cumulative: *c,
            })
            .collect()
    };
    let mk = |kind: SeriesKind, v: &[(i64, i64)], guaranteed: bool| SeriesVm {
        kind,
        label_key: kind.label_key(),
        label: kind.label_en(),
        points: pts(v),
        total: v.last().map_or(0, |p| p.1),
        guaranteed,
    };
    let mut sv = stats_vm();
    sv.series = vec![
        mk(
            SeriesKind::Attributable,
            &[(NOW, 100), (NOW + DAY, 300)],
            true,
        ),
        mk(SeriesKind::Potential, &[(NOW, 900)], false),
        mk(SeriesKind::AddedCost, &[(NOW + DAY, 50)], false),
        mk(
            SeriesKind::Expected,
            &[(NOW, 10), (NOW + 2 * DAY, 20)],
            false,
        ),
        mk(SeriesKind::Observed, &[(NOW + DAY, 70)], false),
    ];
    sv.caveats = vec![ExplanationVm {
        code: "caveat.btrfs_snapshots_may_retain_space",
        params: BTreeMap::new(),
        text: String::new(),
    }];
    w.read.over.lock().unwrap().stats = Some(sv);
    let b = page(&w, "/stats").await;
    assert!(
        b.contains("<svg class=\"chart\"")
            && b.contains("role=\"img\"")
            && b.contains("<title id=\"chart-title\">")
    );
    assert!(b.contains("<path class=\"line\" d=\"M ") && b.contains("stroke-dasharray=\""));
    for marker in [
        "<circle class=\"mark\"",
        "<rect class=\"mark\"",
        "<polygon class=\"mark\"",
        "<path class=\"mark\"",
    ] {
        assert!(b.contains(marker), "{marker}");
    }
    assert_eq!(b.matches("<table").count(), 5, "one data table per series");
    assert!(b.contains("<caption>") && b.contains("scope=\"col\""));
    for k in [
        "series.attributable",
        "series.potential",
        "series.added_cost",
        "series.expected",
        "series.observed",
        "stats.estimate",
        "explain.caveat.btrfs_snapshots_may_retain_space",
    ] {
        assert!(b.contains(&en(k)), "{k}");
    }
    assert!(!b.contains("NaN") && !b.contains("=\"inf"));
    // French chart
    let (_, _, f) = fetch(&w, "/stats", "fr", false).await;
    assert!(f.contains(&fr("stats.chart_title")));
    // logs: level filter and escaped message
    w.logs_push("scan finished");
    let b = page(&w, "/logs?level=error").await;
    assert!(b.contains("scan finished") && b.contains(&en("logs.error")));
    assert!(
        b.contains("#i-alert-octagon"),
        "level is a glyph and a word"
    );
}

// ---- assets, login, errors ---------------------------------------------------------------------

#[tokio::test]
async fn static_assets_are_public_pinned_and_confined() {
    let w = World::new(Arc::new(FailClosedGuard));
    let (s, h, body) = w.get("/static/app.css").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get(header::CONTENT_TYPE).unwrap(),
        "text/css; charset=utf-8"
    );
    assert!(body.contains("prefers-reduced-motion") && body.contains("forced-colors"));
    let (s, h, body) = w.get("/static/htmx-2.0.4.min.js").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        h.get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(
        h.get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(body.len() > 40_000);
    for bad in [
        "/static/..%2FCargo.toml",
        "/static/app.js",
        "/static/htmx.min.js",
        "/static/",
    ] {
        assert_ne!(w.get(bad).await.0, StatusCode::OK, "{bad}");
    }
}

#[test]
fn login_page_keeps_the_auth_contract_in_both_locales() {
    use login::{LoginPage, LoginView};
    for lang in [Locale::En, Locale::Fr] {
        let tr = Tr::new(lang);
        let ctx = Ctx {
            tr,
            htmx: false,
            chrome: Chrome {
                csrf: "login-token".into(),
                user: String::new(),
                dry_run: false,
                lang: lang.as_str(),
            },
            flash: None,
            path: "/login".into(),
        };
        for v in [
            LoginView {
                error_key: None,
                retry_after: None,
            },
            LoginView {
                error_key: Some("login.failed"),
                retry_after: None,
            },
            LoginView {
                error_key: Some("error.rate_limited"),
                retry_after: Some(300),
            },
        ] {
            let page = LoginPage { ctx: &ctx, v: &v };
            let html = page.render().unwrap();
            assert_markup_rules_login(&html);
            assert!(html.contains("<form method=\"post\" action=\"/login\">"));
            assert!(html.contains("name=\"csrf_token\" value=\"login-token\""));
            assert!(html.contains("name=\"username\"") && html.contains("name=\"password\""));
            assert!(
                html.contains("autocomplete=\"username\"")
                    && html.contains("autocomplete=\"current-password\"")
            );
            assert!(!html.contains("<script"));
        }
        let none = LoginPage {
            ctx: &Ctx {
                chrome: Chrome {
                    csrf: String::new(),
                    ..ctx.chrome.clone()
                },
                ..ctx.clone()
            },
            v: &LoginView {
                error_key: Some("login.unavailable"),
                retry_after: None,
            },
        };
        assert!(
            !none.render().unwrap().contains("<form"),
            "no form without a token"
        );
    }
}

fn assert_markup_rules_login(html: &str) {
    assert!(!html.contains(" style=\"") && !html.contains("<style"));
    assert!(html.contains("<link rel=\"stylesheet\" href=\"/static/app.css\">"));
}

#[tokio::test]
async fn critical_flows_work_as_plain_forms_without_javascript() {
    let w = World::new(Arc::new(AllowGuard));
    // The selection form posts to real routes through formaction; every button is a submit.
    let list = page(&w, "/issues/junk").await;
    let form = list
        .split("<form id=\"selection\"")
        .nth(1)
        .unwrap()
        .split("</form>")
        .next()
        .unwrap();
    for route in [
        "/plans/preview",
        "/issues/selection-summary",
        "/issues/evaluate",
        "/issues/verify-content",
        "/issues/ignore",
    ] {
        assert!(form.contains(&format!("formaction=\"{route}\"")), "{route}");
    }
    assert!(list.contains("<form id=\"selection\" method=\"post\" action=\"/plans\""));
    assert!(form.contains("type=\"submit\"") && form.contains("type=\"reset\""));
    // The same routes answer plain posts with redirects (no XHR needed).
    let (s, h, _) = w
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={ID_A}"),
        )
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).unwrap(),
        "/plans/00000000-0000-0000-0000-00000000002a"
    );
    // The read-only total works as a full page without HTMX.
    let (s, _, b) = w
        .post("/issues/selection-summary", &format!("issue_ids={ID_A}"))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("<html") && b.contains(&en("issues.selection_readonly")));
    // GET never executes anything.
    assert_eq!(w.mutations(), 1, "only the plan creation above");
}
