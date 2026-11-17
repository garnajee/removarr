//! Follow-up after wave 14: localized error pages, confirmation re-render, plan listing,
//! statistics selector, SQL-backed issue queries, detail data and the locale return path.

use super::*;
use crate::i18n::{Locale, Tr};
use removarr_domain::plan::PlanMode;
use removarr_reporting::{
    PlanListVm, PlanRowVm, PlannedActionVm, SeriesKind, SeriesPointVm, SeriesVm, StatsRange,
    TorrentVm,
};

const PLAN: &str = "/plans/00000000-0000-0000-0000-00000000002a";
const HOSTILE: &str = "<img src=x onerror=alert(1)>\"'&";

fn t(l: Locale, key: &str) -> String {
    Tr::new(l)
        .t(key)
        .replace('&', "&#38;")
        .replace('<', "&#60;")
        .replace('>', "&#62;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

pub(super) async fn get_as(
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

async fn post_as(
    w: &World,
    uri: &str,
    form: &str,
    lang: &str,
    htmx: bool,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("removarr_locale={lang}"))
        .header("x-csrf-token", CSRF);
    if htmx {
        b = b.header("hx-request", "true");
    }
    w.call(b.body(Body::from(form.to_string())).unwrap()).await
}

// ---- item 1: localized errors, confirmation re-render --------------------------------------------

#[tokio::test]
async fn error_pages_are_localized_and_carry_navigation() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, body) = get_as(&w, "/issues/nosuchcategory", "fr", false).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(body.contains("<html lang=\"fr\">"), "{body}");
    assert!(body.contains(&t(Locale::Fr, "error.not_found")), "{body}");
    for (href, key) in [
        ("/", "nav.dashboard"),
        ("/plans", "nav.plans"),
        ("/jobs", "nav.jobs"),
    ] {
        assert!(
            body.contains(&format!("<a href=\"{href}\">{}</a>", t(Locale::Fr, key))),
            "{href}: {body}"
        );
    }
    assert!(
        h.get(header::CONTENT_LENGTH)
            .is_none_or(|v| { v.to_str().unwrap().parse::<usize>().unwrap() == body.len() })
    );
    assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-store");

    // the same page in English, and a validation detail translated like the message
    let (_, _, en_body) = get_as(&w, "/issues/nosuchcategory", "en", false).await;
    assert!(en_body.contains("<html lang=\"en\">") && en_body.contains("Not found."));
    let (s, _, body) = get_as(&w, "/issues/junk?sort=bogus", "fr", false).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains(&t(Locale::Fr, "error.validation")));
    assert!(
        body.contains(&format!("sort: {}", t(Locale::Fr, "problem.use_sort"))),
        "{body}"
    );
    let (_, _, body) = get_as(&w, "/issues/junk?sort=bogus", "en", false).await;
    assert!(body.contains("sort: use name, size or age"));

    // Accept-Language is honoured without a cookie
    let (_, _, body) = w
        .call(
            Request::get("/nope")
                .header(header::ACCEPT_LANGUAGE, "fr-FR,fr;q=0.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(body.contains(&t(Locale::Fr, "error.not_found")));

    // errors raised outside the handlers (guard refusal on a command) are localized too
    let denied = World::new(Arc::new(FailClosedGuard));
    let (s, _, body) = post_as(&denied, "/scan", "", "fr", false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(
        body.contains(&t(Locale::Fr, "error.unauthorized")),
        "{body}"
    );
    assert_eq!(w.mutations() + denied.mutations(), 0);
}

#[tokio::test]
async fn blocker_codes_in_error_details_are_translated_and_paths_stay_out() {
    let w = World::new(Arc::new(AllowGuard));
    *w.plans.fail.lock().unwrap() = Some(UseCaseError {
        class: ErrorClass::Validation,
        key: "error.plan.nothing_plannable",
        excluded: vec![crate::state::ExcludedItem {
            subject: "p:completed:Rel/a.mkv".into(),
            blockers: vec!["Sample".into()],
        }],
        log_detail: format!("{SECRET_PATH} {SECRET_KEY}"),
    });
    let (s, _, body) = post_as(
        &w,
        "/plans",
        &format!("category=junk&action=delete&issue_ids={ID_A}"),
        "fr",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains(&t(Locale::Fr, "error.plan.nothing_plannable")));
    assert!(
        body.contains(&format!(
            "p:completed:Rel/a.mkv: {}",
            t(Locale::Fr, "blocker.sample")
        )),
        "{body}"
    );
    assert!(!body.contains(SECRET_PATH) && !body.contains("SUPERSECRET"));
}

fn confirm_plan(w: &World) {
    let mut p = super::ui::plan(Some(PlanStatus::Draft), PlanMode::Live);
    p.samples_to_acknowledge = ["x/s1.mkv", "x/s2.mkv"]
        .iter()
        .map(|r| removarr_domain::path::RootedPath::parse("completed", r).unwrap())
        .collect();
    p.unrecoverable_to_acknowledge =
        vec![removarr_domain::path::RootedPath::parse("completed", "x/old.mkv").unwrap()];
    *w.plans.stored.lock().unwrap() = Some(p);
}

#[tokio::test]
async fn a_refused_approval_rerenders_the_confirmation_with_the_ticks_kept() {
    let w = World::new(Arc::new(AllowGuard));
    confirm_plan(&w);
    *w.plans.fail.lock().unwrap() = Some(UseCaseError::new(
        ErrorClass::Validation,
        "error.plan.confirmation_mismatch",
        format!("typed 1, expected 1; {SECRET_PATH}"),
    ));
    let form = format!(
        "plan_hash={}&confirmed_count=1&acknowledged_samples=completed:x/s2.mkv&acknowledged_unrecoverable=completed:x/old.mkv",
        "a".repeat(64)
    );
    let (s, h, body) = post_as(&w, &format!("{PLAN}/approve"), &form, "fr", false).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert!(body.contains("id=\"confirm-error\""), "{body}");
    assert!(body.contains(&t(Locale::Fr, "error.plan.confirmation_mismatch")));
    assert!(body.contains(&t(Locale::Fr, "confirm.error_title")));
    // the confirmation itself is back, with a fresh token and the same plan hash
    assert!(body.contains("name=\"confirmed_count\"") && body.contains(&"a".repeat(64)));
    assert!(body.contains("name=\"csrf_token\" value=\"test-csrf-token\""));
    // exactly the ticked acknowledgements stay ticked; the typed count is not kept
    assert_eq!(body.matches(" checked").count(), 2, "{body}");
    assert!(body.contains("value=\"completed:x/s2.mkv\" checked"));
    assert!(!body.contains("value=\"completed:x/s1.mkv\" checked"));
    assert!(body.contains("value=\"completed:x/old.mkv\" checked"));
    assert!(!body.contains("name=\"confirmed_count\" inputmode=\"numeric\" pattern=\"[0-9]*\" autocomplete=\"off\" value"));
    assert!(!body.contains(SECRET_PATH));
    assert!(w.plans.approvals.lock().unwrap().is_empty());
    assert!(w.jobs.submitted.lock().unwrap().is_empty());

    // an input error (not a number) takes the same path, before any use case runs
    *w.plans.fail.lock().unwrap() = None;
    let form = format!(
        "plan_hash={}&confirmed_count=abc&acknowledged_samples=completed:x/s1.mkv",
        "a".repeat(64)
    );
    let (s, _, body) = post_as(&w, &format!("{PLAN}/approve"), &form, "en", false).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body.contains("id=\"confirm-error\"") && body.contains("confirmed_count: not a number")
    );
    assert!(body.contains("value=\"completed:x/s1.mkv\" checked"));
    assert_eq!(body.matches(" checked").count(), 1);
    assert!(w.plans.approvals.lock().unwrap().is_empty());

    // HTMX gets a swappable status
    let (s, ..) = post_as(&w, &format!("{PLAN}/approve"), &form, "en", true).await;
    assert_eq!(s, StatusCode::OK);

    // a state conflict is shown on the confirmation too
    *w.plans.fail.lock().unwrap() = Some(UseCaseError::new(
        ErrorClass::StateChanged,
        "error.plan.hash_mismatch",
        "hash",
    ));
    let (s, _, body) = post_as(
        &w,
        &format!("{PLAN}/approve"),
        &form.replace("abc", "1"),
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(body.contains("id=\"confirm-error\"") && body.contains("changed since it was shown"));

    // server failures, security refusals and unknown plans stay plain error pages
    *w.plans.fail.lock().unwrap() = Some(UseCaseError::new(
        ErrorClass::Storage,
        "error.storage",
        "db",
    ));
    let (s, _, body) = post_as(
        &w,
        &format!("{PLAN}/approve"),
        &form.replace("abc", "1"),
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!body.contains("confirmed_count"));
    let (s, _, body) = post_as(
        &w,
        "/plans/00000000-0000-0000-0000-0000000000ff/approve",
        &form,
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!body.contains("confirmed_count"));
}

#[tokio::test]
async fn a_successful_approval_still_redirects_and_never_executes() {
    let w = World::new(Arc::new(AllowGuard));
    confirm_plan(&w);
    let form = format!(
        "plan_hash={}&confirmed_count=1&acknowledged_samples=completed:x/s1.mkv&acknowledged_samples=completed:x/s2.mkv&acknowledged_unrecoverable=completed:x/old.mkv",
        "a".repeat(64)
    );
    let (s, h, _) = post_as(&w, &format!("{PLAN}/approve"), &form, "en", false).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h.get(header::LOCATION).unwrap(), PLAN);
    assert!(w.jobs.submitted.lock().unwrap().is_empty());
}

// ---- item 2: plan listing --------------------------------------------------------------------------

fn plan_row(id: &str, status: &str, mode: &str, origin: &str) -> PlanRowVm {
    PlanRowVm {
        id: id.into(),
        status: status.into(),
        mode: mode.into(),
        origin: origin.into(),
        created_at: NOW,
        created_at_iso: "2023-11-14T22:13:20.000Z".into(),
        actions: 3,
        actions_done: 2,
        actions_failed: 1,
        planned_attributable: 2048,
        planned_added: 1024,
        recorded_attributable: 1024,
        recorded_added: 0,
    }
}

#[tokio::test]
async fn the_plan_listing_is_a_read_only_page_with_a_navigation_entry() {
    let w = World::new(Arc::new(AllowGuard));
    w.read.over.lock().unwrap().plans = Some(PlanListVm {
        rows: vec![
            plan_row(
                "00000000-0000-0000-0000-00000000002a",
                "executed",
                "live",
                "user",
            ),
            plan_row(
                "00000000-0000-0000-0000-00000000002b",
                "draft",
                "dry_run",
                "auto_policy",
            ),
        ],
        offset: 0,
        limit: 1,
        has_more: true,
    });
    let (s, _, body) = get_as(&w, "/plans?per_page=1", "en", false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("href=\"/plans/00000000-0000-0000-0000-00000000002a\""));
    for text in [
        "Executed",
        "Draft",
        "Live",
        "Dry run",
        "Automatic",
        "3 actions, 2 done",
        "1 failed",
        "2.0 KiB",
        "1.0 KiB",
    ] {
        assert!(body.contains(text), "{text}: {body}");
    }
    assert!(
        body.contains("href=\"/plans?page=2&#38;per_page=1\""),
        "next page link"
    );
    assert!(!body.contains("rel=\"prev\""));
    // listed in the navigation of every page
    let (_, _, home) = get_as(&w, "/auto", "en", false).await;
    assert!(home.contains("<a href=\"/plans\">Plans</a>"));
    // French and fragment variants
    let (_, _, fr_body) = get_as(&w, "/plans", "fr", false).await;
    assert!(
        fr_body.contains(&t(Locale::Fr, "plans.caption"))
            && fr_body.contains(&t(Locale::Fr, "plan_status.executed"))
    );
    let (_, _, frag) = get_as(&w, "/plans", "en", true).await;
    assert!(frag.starts_with("<section id=\"plans\"") || frag.contains("id=\"plans\""));
    assert!(!frag.contains("<html"));
    // validation and empty state
    assert_eq!(
        get_as(&w, "/plans?per_page=9999", "en", false).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    w.read.over.lock().unwrap().plans = None;
    assert!(
        get_as(&w, "/plans", "en", false)
            .await
            .2
            .contains("No plan has been created yet.")
    );
    assert_eq!(w.mutations(), 0);
}

// ---- item 3: statistics selector ---------------------------------------------------------------

#[tokio::test]
async fn the_statistics_view_and_range_reach_the_read_model_and_the_chart() {
    let w = World::new(Arc::new(AllowGuard));
    let pts = |v: &[(i64, i64, i64)]| -> Vec<SeriesPointVm> {
        v.iter()
            .map(|(at, b, c)| SeriesPointVm {
                at: *at,
                at_iso: "2023-11-14T00:00:00.000Z".into(),
                bytes: *b,
                cumulative: *c,
            })
            .collect()
    };
    let mut sv = stats_vm();
    sv.series = vec![SeriesVm {
        kind: SeriesKind::Attributable,
        label_key: SeriesKind::Attributable.label_key(),
        label: SeriesKind::Attributable.label_en(),
        points: pts(&[(NOW, 10, 10), (NOW + DAY, 5, 15)]),
        total: 15,
        guaranteed: true,
    }];
    w.read.over.lock().unwrap().stats = Some(sv);

    let (s, _, cumulative) = get_as(&w, "/stats", "en", false).await;
    assert_eq!(s, StatusCode::OK);
    let (_, _, per_day) = get_as(&w, "/stats?view=per_day&range=30d", "en", false).await;
    let queries = w.read.stats_queries.lock().unwrap().clone();
    assert_eq!(queries[0].range, StatsRange::All);
    assert_eq!(queries[1].range, StatsRange::Days30);
    // the plotted value follows the selected view
    let chart = |b: &str| {
        b.split("<svg class=\"chart\"")
            .nth(1)
            .unwrap()
            .split("</svg>")
            .next()
            .unwrap()
            .to_string()
    };
    assert!(chart(&cumulative).contains(">15 B<") && !chart(&per_day).contains(">15 B<"));
    assert!(chart(&per_day).contains(">10 B<"));
    // the selector is a plain GET form that keeps the current choice
    assert!(per_day.contains("<option value=\"per_day\" selected>"));
    assert!(per_day.contains("<option value=\"30d\" selected>"));
    assert!(cumulative.contains("<option value=\"cumulative\" selected>"));
    assert!(per_day.contains(&t(Locale::En, "stats.totals_all_time")));
    assert!(!cumulative.contains(&t(Locale::En, "stats.totals_all_time")));
    let (_, _, fr_body) = get_as(&w, "/stats?range=7d", "fr", false).await;
    assert!(fr_body.contains(&t(Locale::Fr, "stats.range.7d")));
    // validation
    for bad in ["/stats?view=sideways", "/stats?range=3d"] {
        assert_eq!(
            get_as(&w, bad, "en", false).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}"
        );
    }
    assert_eq!(w.mutations(), 0);
}

// ---- item 4: the read model does the work, any id resolves ------------------------------------------

#[tokio::test]
async fn list_filters_sort_and_page_are_sent_to_the_read_model() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, ..) = get_as(
        &w,
        "/issues/junk?q=Alpha&kind=file&sort=name&dir=asc&hide_recent=1&page=3&per_page=5",
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let qs = w.read.issue_queries.lock().unwrap().clone();
    assert_eq!(qs.len(), 2, "the page and the hidden-recent count");
    let q = &qs[0];
    assert_eq!(q.category, Some(IssueCategory::Junk));
    assert_eq!(q.search.as_deref(), Some("Alpha"));
    assert_eq!(q.unit_kind, Some(removarr_reporting::IssueUnitKind::File));
    assert_eq!((q.sort, q.descending), (IssueSort::Name, false));
    assert_eq!((q.offset, q.limit), (10, 5));
    assert_eq!((q.min_age_days, q.ignored), (Some(2), Some(false)));
    assert_eq!((qs[1].min_age_days, qs[1].limit), (None, 1));

    w.read.issue_queries.lock().unwrap().clear();
    get_as(&w, "/ignored?sort=age&dir=desc", "en", false).await;
    let q = w.read.issue_queries.lock().unwrap()[0].clone();
    assert_eq!((q.category, q.ignored), (None, Some(true)));
    assert_eq!(
        (q.sort, q.descending, q.min_age_days),
        (IssueSort::Age, true, None)
    );
}

#[tokio::test]
async fn totals_come_from_the_read_model_and_an_issue_beyond_row_1000_has_a_detail_page() {
    let w = World::new(Arc::new(AllowGuard));
    let rows: Vec<IssueRowVm> = (0..1500u32)
        .map(|i| {
            row(
                &format!("{i:032x}"),
                &format!("File{i:04}.nfo"),
                IssueCategory::Junk,
                false,
            )
        })
        .collect();
    w.read.over.lock().unwrap().rows = Some(rows);
    w.read.over.lock().unwrap().finished = Some(Some("2023-11-14T22:13:20.000Z".into()));
    let (_, _, list) = get_as(
        &w,
        "/issues/junk?hide_recent=0&per_page=50&page=30",
        "en",
        false,
    )
    .await;
    assert!(
        list.contains("50 on this page | 1500 in this list"),
        "{list}"
    );
    assert!(list.contains("File1499.nfo"));
    assert!(!list.contains("first 1000"));
    let (s, _, detail) = get_as(&w, &format!("/issues/junk/{:032x}", 1499), "en", false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(detail.contains("File1499.nfo"));
    // wrong category and unknown id are 404
    assert_eq!(
        get_as(&w, &format!("/issues/duplicate/{:032x}", 1499), "en", false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_as(&w, &format!("/issues/junk/{:032x}", 1600), "en", false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // bulk commands resolve ids beyond the first 1000 too
    let (s, _, body) = post_as(
        &w,
        "/issues/selection-summary",
        &format!("issue_ids={:032x}&issue_ids={:032x}", 1400, 1499),
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("2 items selected"), "{body}");
    let (s, ..) = post_as(
        &w,
        "/issues/ignore",
        &format!("issue_ids={:032x}", 1450),
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        w.ignores.ignored.lock().unwrap()[0].0[0].0,
        "p:completed:File1450.nfo"
    );
}

// ---- item 5: detail data ---------------------------------------------------------------------------

#[tokio::test]
async fn the_detail_page_shows_torrents_the_annex_tree_and_planned_consequences() {
    let w = World::new(Arc::new(AllowGuard));
    let mut r = row(ID_A, "Some Show", IssueCategory::Junk, false);
    r.unit_kind = "folder";
    r.paths = vec!["completed:Some Show".into()];
    r.torrents = vec![TorrentVm {
        hash: "ab".repeat(20),
        name: HOSTILE.into(),
        state: "stalledUP".into(),
    }];
    r.torrent_count = 1;
    r.annexes = vec![
        "completed:Some Show/Subs/en/a.srt".into(),
        "completed:Some Show/info.nfo".into(),
    ];
    r.planned = vec![PlannedActionVm {
        plan_id: "00000000-0000-0000-0000-00000000002a".into(),
        plan_status: "approved".into(),
        mode: "live".into(),
        index: 0,
        kind: "DeleteCompletedFolder".into(),
        state: "pending".into(),
        expected_logical: 4096,
        expected_attributable: 2048,
        expected_added: 0,
        torrents: 1,
    }];
    w.read.over.lock().unwrap().rows = Some(vec![r]);
    let (s, _, body) = get_as(&w, &format!("/issues/junk/{ID_A}"), "en", false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("stalledUP") && body.contains(&"ab".repeat(20)));
    assert!(
        !body.contains(HOSTILE) && body.contains("&#60;img src=x"),
        "torrent names are escaped"
    );
    // nested annex tree: Subs > en > a.srt, then info.nfo at the top
    let tree = body
        .split("<ul class=\"tree\">")
        .nth(1)
        .unwrap()
        .split("</ul></li></ul>")
        .next()
        .unwrap();
    assert!(tree.find("Subs").unwrap() < tree.find("a.srt").unwrap());
    assert!(body.contains("info.nfo"));
    assert_eq!(
        body.matches("<ul>").count() + body.matches("<ul class").count(),
        body.matches("</ul>").count()
    );
    // planned consequence links to the plan and states what is expected
    assert!(body.contains("href=\"/plans/00000000-0000-0000-0000-00000000002a\""));
    assert!(
        body.contains("Delete a completed folder")
            && body.contains("2.0 KiB")
            && body.contains("Pending")
    );
    let (_, _, fr_body) = get_as(&w, &format!("/issues/junk/{ID_A}"), "fr", false).await;
    assert!(
        fr_body.contains(&t(Locale::Fr, "detail.planned"))
            && fr_body.contains(&t(Locale::Fr, "detail.annexes"))
    );
    // the detail query asks for the detail data and nothing else is read for it
    let q = w.read.issue_queries.lock().unwrap()[0].clone();
    assert!(q.include_detail && q.ids == [ID_A.to_string()] && q.limit == 1);
    let (_, _, plain) = get_as(&w, "/issues/junk", "en", false).await;
    assert!(!plain.contains("stalledUP"));
    assert!(
        w.read
            .issue_queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.ids.is_empty())
            .all(|q| !q.include_detail),
        "list rows skip the detail queries"
    );
    assert_eq!(w.mutations(), 0);
}

// ---- item 8: locale return path ---------------------------------------------------------------------

#[tokio::test]
async fn the_locale_form_returns_to_the_page_and_never_to_another_origin() {
    let w = World::new(Arc::new(AllowGuard));
    let loc = |h: &axum::http::HeaderMap| {
        h.get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    let (s, h, _) = post_as(
        &w,
        "/locale",
        "locale=fr&return_to=%2Fissues%2Fjunk%3Fpage%3D2%26q%3Da",
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc(&h), "/issues/junk?page=2&q=a");
    assert!(
        h.get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("removarr_locale=fr;")
    );
    for evil in [
        "%2F%2Fevil.example%2Fx",
        "https%3A%2F%2Fevil.example%2F",
        "%2F%5Cevil.example",
        "javascript%3Aalert(1)",
        "evil.example",
        "%2Fa%0D%0ASet-Cookie%3A%20x%3Dy",
    ] {
        let (s, h, _) = post_as(
            &w,
            "/locale",
            &format!("locale=en&return_to={evil}"),
            "en",
            false,
        )
        .await;
        assert_eq!(s, StatusCode::SEE_OTHER, "{evil}");
        assert_eq!(loc(&h), "/", "{evil}");
    }
    let (_, h, _) = post_as(&w, "/locale", "locale=fr", "en", false).await;
    assert_eq!(loc(&h), "/", "no return path");
    let (s, h, _) = post_as(&w, "/locale", "locale=fr&return_to=%2Fjobs", "en", true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("hx-redirect").unwrap(), "/jobs");
    // an invalid locale is still refused, whatever the return path
    let (s, ..) = post_as(&w, "/locale", "locale=de&return_to=%2Fjobs", "en", false).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // the form on every page names the current page
    let (_, _, page) = get_as(&w, "/issues/junk", "en", false).await;
    assert!(page.contains("name=\"return_to\" value=\"/issues/junk\""));
}
