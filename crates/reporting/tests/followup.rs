#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Wave-14 follow-up: SQL filtering and sorting of issues, detail data, plan listing and the
//! statistics range.

mod common;

use common::*;
use removarr_domain::issue::Unit;
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_reporting::{
    IssueQuery, IssueSort, IssueUnitKind, PlanListQuery, StatsQuery, StatsRange,
};

const DAY: i64 = 24 * HOUR;

fn rooted(rel: &str) -> RootedPath {
    RootedPath::new(RootId::completed(), RelPath::new(rel).unwrap())
}

fn folder_unit(dir: &str, members: &[&str], annexes: &[&str]) -> String {
    serde_json::to_string(&Unit::Folder {
        dir: rooted(dir),
        members: members.iter().map(|m| rooted(m)).collect(),
        annexes: annexes.iter().map(|m| rooted(m)).collect(),
    })
    .unwrap()
}

async fn add(fx: &Fx, id: &str, subject: &str, unit: &str, logical: i64, age_days: i64, ino: u64) {
    issue(
        &fx.w,
        id,
        "junk",
        "info",
        subject,
        unit,
        &[],
        logical,
        logical,
        &[(1, ino)],
    )
    .await;
    exec(
        &fx.w,
        &format!(
            "UPDATE issues SET age_basis = {} WHERE id = '{id}'",
            NOW - age_days * DAY
        ),
    )
    .await;
}

async fn ids(fx: &Fx, q: &IssueQuery) -> Vec<String> {
    fx.r.issues(q)
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.id.chars().next().unwrap().to_string())
        .collect()
}

async fn small_set(fx: &Fx) {
    session(&fx.w, S1, NOW - 2 * HOUR, Some(NOW - HOUR), "complete").await;
    exec(
        &fx.w,
        &format!("INSERT INTO meta VALUES ('issues_session', '{S1}')"),
    )
    .await;
    add(
        fx,
        &"a".repeat(32),
        "p:completed:Zeta/z.nfo",
        &file_unit("Zeta/z.nfo"),
        300,
        40,
        1,
    )
    .await;
    add(
        fx,
        &"b".repeat(32),
        "p:completed:Alpha",
        &folder_unit(
            "Alpha",
            &["Alpha/ep1.mkv", "Alpha/Sample/s.mkv"],
            &["Alpha/info.nfo"],
        ),
        100,
        2,
        2,
    )
    .await;
    add(
        fx,
        &"c".repeat(32),
        "p:completed:mid/Beta.txt",
        &file_unit("mid/Beta.txt"),
        200,
        10,
        3,
    )
    .await;
    // ignored
    add(
        fx,
        &"d".repeat(32),
        "p:completed:hidden/H.nfo",
        &file_unit("hidden/H.nfo"),
        50,
        100,
        4,
    )
    .await;
    exec(
        &fx.w,
        &format!(
            "INSERT INTO ignores (subject_key, reason, created_at) VALUES ('p:completed:hidden/H.nfo', NULL, {NOW})"
        ),
    )
    .await;
}

#[tokio::test]
async fn sql_sort_orders_by_name_size_and_age_with_stable_ties() {
    let fx = fixture().await;
    small_set(&fx).await;
    let q = |sort, descending| IssueQuery {
        sort,
        descending,
        ignored: Some(false),
        ..IssueQuery::default()
    };
    assert_eq!(ids(&fx, &q(IssueSort::Name, false)).await, ["b", "c", "a"]);
    assert_eq!(ids(&fx, &q(IssueSort::Name, true)).await, ["a", "c", "b"]);
    assert_eq!(ids(&fx, &q(IssueSort::Size, false)).await, ["b", "c", "a"]);
    assert_eq!(ids(&fx, &q(IssueSort::Size, true)).await, ["a", "c", "b"]);
    // ascending age = newest first
    assert_eq!(ids(&fx, &q(IssueSort::Age, false)).await, ["b", "c", "a"]);
    assert_eq!(ids(&fx, &q(IssueSort::Age, true)).await, ["a", "c", "b"]);
}

#[tokio::test]
async fn sql_filters_search_kind_age_and_ignored_with_exact_totals() {
    let fx = fixture().await;
    small_set(&fx).await;
    let base = IssueQuery {
        ignored: Some(false),
        ..IssueQuery::default()
    };
    let all = fx.r.issues(&base).await.unwrap();
    assert_eq!((all.total_count, all.total_logical_bytes), (3, 600));
    let ign =
        fx.r.issues(&IssueQuery {
            ignored: Some(true),
            ..IssueQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(ign.rows.len(), 1);
    assert!(ign.rows[0].ignored);

    let search = |s: &str| IssueQuery {
        search: Some(s.into()),
        ..base.clone()
    };
    // name, case-insensitive
    assert_eq!(ids(&fx, &search("BETA")).await, ["c"]);
    // subject
    assert_eq!(ids(&fx, &search("p:completed:zeta")).await, ["a"]);
    // a member and an annex of the folder unit
    assert_eq!(ids(&fx, &search("ep1.mkv")).await, ["b"]);
    assert_eq!(ids(&fx, &search("info.nfo")).await, ["b"]);
    // JSON keys are not searchable text
    assert!(ids(&fx, &search("rel")).await.is_empty());
    assert!(ids(&fx, &search("File")).await.is_empty());
    // LIKE wildcards are plain characters
    assert!(ids(&fx, &search("%")).await.is_empty());
    let s = fx.r.issues(&search("nfo")).await.unwrap();
    assert_eq!((s.total_count, s.total_logical_bytes), (2, 400));

    let kind = |k| IssueQuery {
        unit_kind: Some(k),
        sort: IssueSort::Name,
        ..base.clone()
    };
    assert_eq!(ids(&fx, &kind(IssueUnitKind::Folder)).await, ["b"]);
    assert_eq!(ids(&fx, &kind(IssueUnitKind::File)).await, ["c", "a"]);
    assert!(ids(&fx, &kind(IssueUnitKind::Torrent)).await.is_empty());

    let old =
        fx.r.issues(&IssueQuery {
            min_age_days: Some(10),
            sort: IssueSort::Name,
            ..base.clone()
        })
        .await
        .unwrap();
    assert_eq!(old.rows.len(), 2);
    assert_eq!(old.total_count, 2, "hidden recent items are not counted");

    // paging is applied after the filters; totals still cover every match
    let page =
        fx.r.issues(&IssueQuery {
            offset: 1,
            limit: 1,
            sort: IssueSort::Name,
            ..base
        })
        .await
        .unwrap();
    assert_eq!((page.rows.len(), page.total_count), (1, 3));
    assert_eq!(page.rows[0].id, "c".repeat(32));
}

#[tokio::test]
async fn an_issue_beyond_row_1000_resolves_by_id_and_pages_are_exact() {
    let fx = fixture().await;
    session(&fx.w, S1, NOW - 2 * HOUR, Some(NOW - HOUR), "complete").await;
    let mut script = String::new();
    for i in 0..1100u32 {
        let id = format!("{i:032x}");
        let unit = q(&file_unit(&format!("bulk/f{i:04}.nfo")));
        script.push_str(&format!(
            "INSERT INTO issues (id, category, subject, severity, outcome, unit, evidence, blockers, torrent_hashes, physical_ids, logical_bytes, potential_bytes, age_basis, first_seen_at, body_hash) \
             VALUES ('{id}', 'junk', 's{i}', 'info', 'keep_manual', '{unit}', '[]', '[]', '[]', '[]', {i}, 0, {NOW}, {NOW}, 'h');\n"
        ));
    }
    sqlx::raw_sql(&script).execute(&fx.w).await.unwrap();

    let last = format!("{:032x}", 1099);
    let one =
        fx.r.issues(&IssueQuery {
            ids: vec![last.clone()],
            limit: 1,
            include_detail: true,
            ..IssueQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(one.rows.len(), 1);
    assert_eq!(one.rows[0].id, last);
    assert_eq!(one.total_count, 1);

    // sorted by size descending the first rank is the last id; the 1001st row is reachable
    let page =
        fx.r.issues(&IssueQuery {
            sort: IssueSort::Size,
            descending: true,
            offset: 1050,
            limit: 100,
            ..IssueQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total_count, 1100);
    assert_eq!(page.rows.len(), 50);
    let searched =
        fx.r.issues(&IssueQuery {
            search: Some("f1099".into()),
            ..IssueQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(searched.total_count, 1);
}

#[tokio::test]
async fn detail_carries_torrents_annexes_and_planned_consequences() {
    let fx = fixture().await;
    small_set(&fx).await;
    exec(
        &fx.w,
        &format!(
            "UPDATE issues SET torrent_hashes = '[\"{}\"]' WHERE id = '{}'",
            "ab".repeat(20),
            "b".repeat(32)
        ),
    )
    .await;
    exec(
        &fx.w,
        &format!("INSERT INTO torrents (hash, name, state, category, size, info, files) VALUES ('{}', 'Alpha.S01', 'stalledUP', 'tv', 1, '{{}}', '[]')", "ab".repeat(20)),
    )
    .await;
    let body = r#"{"actions":[{"index":0,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"Alpha"}}},"expected_gain":{"logical":100,"attributable":90,"added":0},"torrent_hashes":["abababababababababababababababababababab"]},{"index":1,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"other"}}},"expected_gain":{"logical":5,"attributable":5,"added":0},"torrent_hashes":[]}],"gains":{"logical":105,"attributable":95,"added":0,"net":95}}"#;
    plan(&fx.w, P1, '1', S1, "live", "user", body, NOW - HOUR).await;
    action(&fx.w, P1, 0, "DeleteJunk", "pending", None, NOW).await;
    action(&fx.w, P1, 1, "DeleteJunk", "pending", None, NOW).await;

    let detail = |id: char, detail: bool| {
        let r = fx.r.clone();
        async move {
            r.issues(&IssueQuery {
                ids: vec![id.to_string().repeat(32)],
                include_detail: detail,
                ..IssueQuery::default()
            })
            .await
            .unwrap()
            .rows
            .remove(0)
        }
    };
    let row = detail('b', true).await;
    assert_eq!(row.torrents.len(), 1);
    assert_eq!(
        (
            row.torrents[0].name.as_str(),
            row.torrents[0].state.as_str()
        ),
        ("Alpha.S01", "stalledUP")
    );
    assert_eq!(row.annexes, ["completed:Alpha/info.nfo"]);
    assert_eq!(row.planned.len(), 1);
    let p = &row.planned[0];
    assert_eq!(
        (
            p.plan_id.as_str(),
            p.plan_status.as_str(),
            p.mode.as_str(),
            p.index
        ),
        (P1, "draft", "live", 0)
    );
    assert_eq!(
        (p.expected_logical, p.expected_attributable, p.torrents),
        (100, 90, 1)
    );
    assert_eq!(p.kind, "DeleteJunk");
    // an issue no plan targets has nothing planned; list rows skip the detail queries
    assert!(detail('c', true).await.planned.is_empty());
    let plain = detail('b', false).await;
    assert!(plain.torrents.is_empty() && plain.annexes.is_empty() && plain.planned.is_empty());
}

#[tokio::test]
async fn plan_listing_counts_actions_and_gains_newest_first() {
    let fx = fixture().await;
    session(&fx.w, S1, NOW - 5 * HOUR, Some(NOW - 4 * HOUR), "complete").await;
    let body = r#"{"actions":[{"index":0,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"a"}}},"expected_gain":{"logical":10,"attributable":10,"added":0}},{"index":1,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"b"}}},"expected_gain":{"logical":5,"attributable":5,"added":2}}],"gains":{"logical":15,"attributable":15,"added":2,"net":13}}"#;
    plan(&fx.w, P1, '1', S1, "live", "user", body, NOW - 3 * HOUR).await;
    plan(
        &fx.w,
        P2,
        '2',
        S1,
        "dry_run",
        "auto_policy",
        body,
        NOW - HOUR,
    )
    .await;
    action(
        &fx.w,
        P1,
        0,
        "DeleteJunk",
        "verified",
        Some("\"Verified\""),
        NOW,
    )
    .await;
    action(&fx.w, P1, 1, "DeleteJunk", "failed", None, NOW).await;
    gain(
        &fx.w,
        &"1".repeat(32),
        P1,
        0,
        "attributable_deletion",
        10,
        NOW,
    )
    .await;

    let l = fx.r.plans(&PlanListQuery::default()).await.unwrap();
    assert_eq!(l.rows.len(), 2);
    assert!(!l.has_more);
    assert_eq!(l.rows[0].id, P2, "newest first");
    assert_eq!(
        (l.rows[0].mode.as_str(), l.rows[0].origin.as_str()),
        ("dry_run", "auto_policy")
    );
    let r = &l.rows[1];
    assert_eq!(
        (
            r.status.as_str(),
            r.actions,
            r.actions_done,
            r.actions_failed
        ),
        ("draft", 2, 1, 1)
    );
    assert_eq!(
        (
            r.planned_attributable,
            r.planned_added,
            r.recorded_attributable,
            r.recorded_added
        ),
        (15, 2, 10, 0)
    );
    let page =
        fx.r.plans(&PlanListQuery {
            offset: 0,
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!((page.rows.len(), page.has_more), (1, true));
    let none =
        fx.r.plans(&PlanListQuery {
            offset: 5,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(none.rows.is_empty());
}

#[tokio::test]
async fn stats_range_keeps_only_recent_events() {
    let fx = fixture().await;
    session(&fx.w, S1, NOW - 100 * DAY, Some(NOW - 99 * DAY), "complete").await;
    let body = r#"{"actions":[{"index":0,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"a"}}},"expected_gain":{"logical":10,"attributable":10,"added":0}},{"index":1,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"b"}}},"expected_gain":{"logical":5,"attributable":5,"added":0}}],"gains":{"logical":15,"attributable":15,"added":0,"net":15}}"#;
    plan(&fx.w, P1, '1', S1, "live", "user", body, NOW - 60 * DAY).await;
    action(&fx.w, P1, 0, "DeleteJunk", "verified", None, NOW).await;
    action(&fx.w, P1, 1, "DeleteJunk", "verified", None, NOW).await;
    gain(
        &fx.w,
        &"1".repeat(32),
        P1,
        0,
        "attributable_deletion",
        10,
        NOW - 60 * DAY,
    )
    .await;
    gain(
        &fx.w,
        &"2".repeat(32),
        P1,
        1,
        "attributable_deletion",
        5,
        NOW - 3 * DAY,
    )
    .await;

    // Series follow the window; the gain totals stay all-time.
    let attributable = |st: &removarr_reporting::StatsVm| {
        st.series
            .iter()
            .find(|s| s.kind == removarr_reporting::SeriesKind::Attributable)
            .unwrap()
            .total
    };
    let stats = |range, since| {
        let r = fx.r.clone();
        async move {
            r.stats(&StatsQuery {
                since,
                range,
                ..StatsQuery::default()
            })
            .await
            .unwrap()
        }
    };
    let all = stats(StatsRange::All, None).await;
    assert_eq!(attributable(&all), 15);
    let r30 = stats(StatsRange::Days30, None).await;
    assert_eq!(attributable(&r30), 5);
    assert_eq!(r30.totals.attributable_bytes, 15);
    assert_eq!(attributable(&stats(StatsRange::Days90, None).await), 15);
    // a later explicit `since` wins over a wider window
    let both = stats(StatsRange::Days365, Some(NOW - 2 * DAY)).await;
    assert_eq!(attributable(&both), 0);
}
