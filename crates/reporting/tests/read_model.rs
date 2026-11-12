#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use common::*;
use removarr_domain::evidence::Completeness;
use removarr_reporting::export::{self, ExportFormat};
use removarr_reporting::{DashboardInputs, HistoryQuery, IssueQuery, StatsQuery, Urgency};

const COMPLETE: Completeness = Completeness::Complete;

fn inputs() -> DashboardInputs {
    DashboardInputs {
        scan_interval_minutes: 60,
        auto_enabled: true,
        ..Default::default()
    }
}

async fn base(fx: &Fx) {
    session(
        &fx.w,
        S0,
        NOW - 50 * HOUR,
        Some(NOW - 49 * HOUR),
        "cancelled",
    )
    .await;
    session(&fx.w, S1, NOW - 2 * HOUR, Some(NOW - HOUR), "complete").await;
    exec(
        &fx.w,
        &format!("INSERT INTO session_roots VALUES ('{S1}', 'completed', 1, 0, 0, NULL, 1, 1)"),
    )
    .await;
    exec(&fx.w, &format!("INSERT INTO session_services VALUES ('{S1}', 'sonarr', '{{\"Ok\":{{\"version\":\"4.0.1\"}}}}', {})", NOW - 2 * HOUR)).await;
    exec(
        &fx.w,
        &format!("INSERT INTO meta VALUES ('issues_session', '{S1}')"),
    )
    .await;
    // one hardlinked file (two paths, one inode) and one lone file
    file_entry(&fx.w, "completed", "a/x.mkv", 1, 10, 1000, 2, 4096, S1).await;
    file_entry(&fx.w, "media-aaaaaaaa", "m/x.mkv", 1, 10, 1000, 2, 4096, S1).await;
    file_entry(&fx.w, "completed", "b/y.mkv", 1, 11, 500, 1, 4096, S1).await;
}

#[tokio::test]
async fn dashboard_reports_four_space_quantities_apart_and_dedupes_potential() {
    let fx = fixture().await;
    base(&fx).await;
    let ev = [note(S1, "no_inode_shared_with_media", COMPLETE)];
    // two issues claim inode 1:10 (potential must be counted once), one distinct, one ignored
    issue(
        &fx.w,
        &"a".repeat(32),
        "orphan_completed",
        "warning",
        "p:completed:a/x.mkv",
        &file_unit("a/x.mkv"),
        &ev,
        1000,
        4096,
        &[(1, 10)],
    )
    .await;
    issue(
        &fx.w,
        &"b".repeat(32),
        "duplicate",
        "info",
        "d:1:10-1:11",
        &file_unit("a/x.mkv"),
        &ev,
        1000,
        4000,
        &[(1, 10)],
    )
    .await;
    issue(
        &fx.w,
        &"c".repeat(32),
        "junk",
        "info",
        "p:completed:b/y.mkv",
        &file_unit("b/y.mkv"),
        &ev,
        500,
        4096,
        &[(1, 11)],
    )
    .await;
    issue(
        &fx.w,
        &"d".repeat(32),
        "junk",
        "critical",
        "p:completed:ignored",
        &file_unit("b/z.mkv"),
        &ev,
        9,
        9,
        &[(1, 12)],
    )
    .await;
    exec(&fx.w, "INSERT INTO ignores (subject_key, reason, created_at) VALUES ('p:completed:ignored', 'because', 1)").await;

    let d = fx.r.dashboard(&inputs()).await.unwrap();
    let s = &d.space;
    assert_eq!(s.logical_bytes, 2500, "every path counts");
    assert_eq!(s.unique_allocated_bytes, 8192, "each inode once");
    assert_eq!(s.shared_bytes, 4096, "only the inode with two paths");
    assert_eq!(s.potential_bytes, 4096 + 4096, "inode 1:10 counted once");
    assert_eq!(s.potential_overlap_excluded_bytes, 4000);
    assert_eq!(s.potential_label_key, "space.potential_not_guaranteed");
    assert_eq!(d.ignored_issues, 1);
    let cat = |c: &str| {
        d.categories
            .iter()
            .find(|x| serde_json::to_value(x.category).unwrap() == c)
            .unwrap()
    };
    assert_eq!(cat("junk").count, 1);
    assert_eq!(cat("orphan_completed").warning, 1);
    assert_eq!(d.categories.len(), 13);
    assert_eq!(
        d.urgency.level,
        Urgency::Orange,
        "the critical issue is ignored; only a warning remains"
    );
    assert!(!d.urgency.icon.is_empty());
    let total_by_category: u64 = d.categories.iter().map(|c| c.potential_bytes).sum();
    assert_eq!(total_by_category, s.potential_bytes);
    assert!(
        d.services[0].ok && !d.services[0].current,
        "recorded state is historical"
    );
    assert_eq!(d.services[0].observed_at, NOW - 2 * HOUR);
}

#[tokio::test]
async fn incomplete_or_stale_sessions_turn_urgency_red_and_are_explained() {
    let fx = fixture().await;
    session(
        &fx.w,
        S1,
        NOW - 10 * HOUR,
        Some(NOW - 9 * HOUR),
        "cancelled",
    )
    .await;
    exec(
        &fx.w,
        &format!("INSERT INTO session_roots VALUES ('{S1}', 'completed', 0, 3, 0, NULL, 0, 0)"),
    )
    .await;
    exec(&fx.w, &format!("INSERT INTO session_services VALUES ('{S1}', 'qbittorrent', '{{\"Unavailable\":{{\"reason\":\"GET http://qbit.lan:8080/api?apikey=SUPERSECRET failed\"}}}}', {NOW})")).await;
    exec(
        &fx.w,
        &format!("INSERT INTO meta VALUES ('issues_session', '{S0}')"),
    )
    .await;
    let ev = [note(
        S0,
        "x",
        Completeness::Incomplete {
            missing: vec!["arr snapshot".into()],
        },
    )];
    issue(
        &fx.w,
        &"e".repeat(32),
        "junk",
        "info",
        "p:completed:q",
        &file_unit("q"),
        &ev,
        1,
        1,
        &[(1, 1)],
    )
    .await;
    issue(
        &fx.w,
        &"f".repeat(32),
        "junk",
        "info",
        "p:completed:r",
        &file_unit("r"),
        &[],
        1,
        0,
        &[],
    )
    .await;

    let d = fx.r.dashboard(&inputs()).await.unwrap();
    assert_eq!(d.urgency.level, Urgency::Red);
    let keys: Vec<_> = d.urgency.reasons.iter().map(|r| r.key).collect();
    for k in ["scan_incomplete", "scan_stale", "service_not_ok"] {
        assert!(keys.contains(&k), "{k} in {keys:?}");
    }
    let codes: Vec<_> = d.explanations.iter().map(|e| e.code).collect();
    for c in [
        "scan.not_complete",
        "scan.root_not_scanned",
        "scan.root_errors",
        "scan.stale",
        "issues.from_older_session",
        "service.not_ok",
        "issues.incomplete_evidence",
    ] {
        assert!(codes.contains(&c), "{c} in {codes:?}");
    }
    let all = serde_json::to_string(&d).unwrap();
    assert!(
        !all.contains("SUPERSECRET") && !all.contains("qbit.lan"),
        "{all}"
    );

    let list = fx.r.issues(&IssueQuery::default()).await.unwrap();
    assert_eq!(list.total_count, 2);
    let row = list.rows.iter().find(|r| r.id.starts_with('e')).unwrap();
    assert_eq!(row.completeness.state, "incomplete");
    assert_eq!(row.completeness.missing, ["arr snapshot"]);
    assert!(
        row.evidence.iter().all(|e| !e.current),
        "evidence of an older session is never current"
    );
    assert!(row.evidence[0].observed_at_iso.ends_with('Z'));
    let unknown = list.rows.iter().find(|r| r.id.starts_with('f')).unwrap();
    assert_eq!(
        unknown.completeness.state, "unknown",
        "no evidence is unknown, never complete"
    );
}

#[tokio::test]
async fn an_empty_database_is_red_with_no_scan_and_no_numbers_invented() {
    let fx = fixture().await;
    let d = fx.r.dashboard(&inputs()).await.unwrap();
    assert_eq!(d.urgency.level, Urgency::Red);
    assert!(d.last_scan.is_none() && d.space.observed_at.is_none());
    assert_eq!(d.explanations[0].code, "scan.none");
    assert_eq!(d.space.potential_bytes, 0);
}

#[tokio::test]
async fn auto_pause_reason_is_shown_and_sanitized() {
    let fx = fixture().await;
    base(&fx).await;
    exec(&fx.w, &format!("INSERT INTO app_state VALUES ('auto_paused', 'service sonarr unavailable at http://sonarr.lan/api?apikey=SUPERSECRET', {})", NOW - HOUR)).await;
    exec(
        &fx.w,
        &format!(
            "INSERT INTO app_state VALUES ('auto_cycle_running', '{S1}', {})",
            NOW - HOUR
        ),
    )
    .await;
    let d = fx.r.dashboard(&inputs()).await.unwrap();
    assert!(d.auto.paused && d.auto.enabled);
    let reason = d.auto.paused_reason.clone().unwrap();
    assert!(
        reason.contains("unavailable")
            && !reason.contains("SUPERSECRET")
            && !reason.contains("sonarr.lan")
    );
    assert_eq!(d.auto.paused_at, Some(NOW - HOUR));
    assert_eq!(d.auto.cycle_marker_session.as_deref(), Some(S1));
    assert!(d.urgency.reasons.iter().any(|r| r.key == "auto_paused"));
    assert!(d.explanations.iter().any(|e| e.code == "auto.paused"));
    exec(
        &fx.w,
        "UPDATE app_state SET value = '' WHERE key = 'auto_paused'",
    )
    .await;
    assert!(!fx.r.dashboard(&inputs()).await.unwrap().auto.paused);
}

const BODY: &str = r#"{"actions":[{"index":0,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"junk/a.nfo"}}},"expected_gain":{"logical":100,"attributable":100,"added":0}},{"index":1,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"junk/b.nfo"}}},"expected_gain":{"logical":50,"attributable":50,"added":0}},{"index":2,"kind":"DeleteJunk","target":{"Path":{"path":{"root":"completed","rel":"junk/c.nfo"}}},"expected_gain":{"logical":10,"attributable":10,"added":0}}],"gains":{"logical":160,"attributable":160,"added":0,"net":160}}"#;

async fn executed_plan(fx: &Fx) {
    plan(
        &fx.w,
        P1,
        '1',
        S1,
        "live",
        "auto_policy",
        BODY,
        NOW - 3 * HOUR,
    )
    .await;
    approve(&fx.w, P1, '1', "auto_policy", NOW - 3 * HOUR + 1000).await;
    set_status(&fx.w, P1, "executing").await;
    action(
        &fx.w,
        P1,
        0,
        "DeleteJunk",
        "verified",
        Some("\"Verified\""),
        NOW - 2 * HOUR,
    )
    .await;
    action(&fx.w, P1, 1, "DeleteJunk", "executed_unverified", Some(r#"{"ExecutedUnverified":{"reason":"recheck pending, see http://t.example/announce?passkey=PASSKEYVALUE"}}"#), NOW - 2 * HOUR + 5).await;
    action(
        &fx.w,
        P1,
        2,
        "DeleteJunk",
        "pending",
        None,
        NOW - 2 * HOUR + 6,
    )
    .await;
    exec(&fx.w, &format!("INSERT INTO journal_intents (id, plan_id, action_idx, step, mode, target, observed, expected_gain, hashes, created_at) VALUES ('i1', '{P1}', 0, 'FsRemoveFile', 'live', '{{}}', '{{}}', '{{}}', '[]', {})", NOW - 2 * HOUR - 50)).await;
    exec(
        &fx.w,
        &format!(
            "INSERT INTO journal_results VALUES ('i1', '\"Simulated\"', {})",
            NOW - 2 * HOUR
        ),
    )
    .await;
    exec(&fx.w, &format!("INSERT INTO journal_intents (id, plan_id, action_idx, step, mode, target, observed, expected_gain, hashes, created_at) VALUES ('i2', '{P1}', 1, 'FsRemoveFile', 'live', '{{}}', '{{}}', '{{}}', '[]', {})", NOW - 2 * HOUR)).await;
    gain(
        &fx.w,
        &"1".repeat(32),
        P1,
        0,
        "attributable_deletion",
        80,
        NOW - 2 * HOUR,
    )
    .await;
    set_status(&fx.w, P1, "partially_executed").await;
    exec(&fx.w, &format!("INSERT INTO observed_deltas (plan_id, root, free_before, free_after_local, free_after_services, recorded_at) VALUES ('{P1}', 'completed', 1000, 1030, 1040, {})", NOW - 2 * HOUR + 9)).await;
}

#[tokio::test]
async fn history_rows_carry_exact_time_target_reason_outcome_and_gain_ids() {
    let fx = fixture().await;
    base(&fx).await;
    executed_plan(&fx).await;
    // a dry run never shows up by default
    plan(&fx.w, P_DRY, '2', S1, "dry_run", "user", BODY, NOW).await;
    action(
        &fx.w,
        P_DRY,
        0,
        "DeleteJunk",
        "simulated",
        Some("\"Simulated\""),
        NOW,
    )
    .await;

    let h = fx.r.history(&HistoryQuery::default()).await.unwrap();
    assert_eq!(
        h.rows.len(),
        2,
        "pending and dry-run actions are not history"
    );
    assert_eq!(h.unfinished_intents, 1);
    let verified = h.rows.iter().find(|r| r.action_index == 0).unwrap();
    assert_eq!(verified.at, NOW - 2 * HOUR);
    assert_eq!(verified.at_iso, "2023-11-14T20:13:20.000Z");
    assert_eq!(verified.started_at, Some(NOW - 2 * HOUR - 50));
    assert_eq!(verified.target, "completed:junk/a.nfo");
    assert_eq!(verified.reason.origin, "auto_policy");
    assert_eq!(verified.reason.approved_by.as_deref(), Some("auto_policy"));
    assert!(verified.reason.approved_at.is_some());
    assert!(verified.outcome.success);
    assert_eq!(
        (verified.attributable_bytes, verified.gain_ids.len()),
        (80, 1)
    );
    let unver = h.rows.iter().find(|r| r.action_index == 1).unwrap();
    assert_eq!(unver.outcome.key, "executed_unverified");
    assert!(unver.outcome.needs_attention);
    let all = serde_json::to_string(&h).unwrap();
    assert!(
        !all.contains("PASSKEYVALUE") && !all.contains("t.example"),
        "{all}"
    );
    assert!(h.rows[0].at >= h.rows[1].at, "newest first");

    let page =
        fx.r.history(&HistoryQuery {
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.has_more && page.rows.len() == 1);
    let dry =
        fx.r.history(&HistoryQuery {
            include_dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(dry.rows.len(), 3);
    let auto =
        fx.r.history(&HistoryQuery {
            origin: Some("user".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(auto.rows.is_empty());
}

#[tokio::test]
async fn stats_keep_the_five_notions_apart_and_sum_each_gain_id_once() {
    let fx = fixture().await;
    base(&fx).await;
    executed_plan(&fx).await;
    gain(
        &fx.w,
        &"2".repeat(32),
        P1,
        0,
        "attributable_consolidation",
        20,
        NOW - 2 * HOUR + 3,
    )
    .await;
    gain(
        &fx.w,
        &"3".repeat(32),
        P1,
        1,
        "added_cost",
        7,
        NOW - 2 * HOUR + 4,
    )
    .await;
    let ev = [note(S1, "n", COMPLETE)];
    issue(
        &fx.w,
        &"a".repeat(32),
        "junk",
        "info",
        "s",
        &file_unit("a/x.mkv"),
        &ev,
        1000,
        300,
        &[(1, 10)],
    )
    .await;

    let st = fx.r.stats(&StatsQuery::default()).await.unwrap();
    assert_eq!(
        (
            st.totals.attributable_bytes,
            st.totals.added_bytes,
            st.totals.net_bytes
        ),
        (100, 7, 93)
    );
    assert_eq!(
        (
            st.totals.unique_gain_ids,
            st.totals.duplicate_gain_ids_ignored
        ),
        (3, 0)
    );
    let by = |k: &str| {
        st.series
            .iter()
            .find(|s| serde_json::to_value(s.kind).unwrap() == k)
            .unwrap()
    };
    assert_eq!(by("attributable").total, 100);
    assert!(by("attributable").guaranteed);
    assert_eq!(by("added_cost").total, 7);
    assert_eq!(by("expected").total, 160, "plan estimate, not recorded");
    assert!(!by("expected").guaranteed);
    assert_eq!(by("observed").total, 30, "free_after_local - free_before");
    assert_eq!(by("potential").total, 300);
    assert!(!by("potential").guaranteed);
    assert_eq!(by("potential").points[0].at_iso.len(), 24);
    assert_eq!(
        st.caveats[0].code,
        "caveat.btrfs_snapshots_may_retain_space"
    );
    assert_eq!(st.observed[0].free_after_services, Some(1040));
}

#[tokio::test]
async fn plan_report_explains_differences_and_remaining_actions() {
    let fx = fixture().await;
    base(&fx).await;
    executed_plan(&fx).await;
    let r =
        fx.r.plan_report(removarr_domain::ids::PlanId::new(
            removarr_domain::ids::RawUuid::parse_str(P1).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        (r.planned.attributable, r.recorded.attributable_bytes),
        (160, 80)
    );
    let codes: Vec<_> = r.differences.iter().map(|d| d.code).collect();
    for c in [
        "plan.unfinished_intents",
        "action.not_verified",
        "gain.differs_from_plan",
        "gain.total_differs_from_plan",
        "observed.differs_from_attributable",
    ] {
        assert!(codes.contains(&c), "{c} in {codes:?}");
    }
    let rem: Vec<_> = r.remaining.iter().map(|x| (x.index, x.next_step)).collect();
    assert_eq!(rem, [(1, "verify_manually"), (2, "run_or_discard")]);
    assert!(r.actions[0].outcome.success);
    assert!(!serde_json::to_string(&r).unwrap().contains("PASSKEYVALUE"));
    assert!(
        fx.r.plan_report(removarr_domain::ids::PlanId::new(
            removarr_domain::ids::RawUuid::nil()
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn exports_neutralize_hostile_content_and_never_contain_secrets() {
    let fx = fixture().await;
    base(&fx).await;
    executed_plan(&fx).await;
    exec(&fx.w, &format!("INSERT INTO session_services VALUES ('{S1}', 'qbittorrent', '{{\"Unavailable\":{{\"reason\":\"password=SUPERSECRET at http://private.tracker/announce/PASSKEYVALUE\"}}}}', {NOW})")).await;
    exec(
        &fx.w,
        &format!("INSERT INTO app_state VALUES ('auto_paused', 'Bearer SUPERSECRET stop', {NOW})"),
    )
    .await;
    exec(&fx.w, &format!("INSERT INTO jobs (id, kind, dedupe_key, lane, status, enqueued_at, stage) VALUES ('j1', '\"Scan\"', 'scan', 'observe', 'running', {NOW}, 'token=SUPERSECRET')")).await;
    let ev = [note(S1, "n", COMPLETE)];
    issue(
        &fx.w,
        &"1".repeat(32),
        "junk",
        "warning",
        "p:completed:=cmd|calc",
        &file_unit("=cmd|calc.mkv"),
        &ev,
        5,
        5,
        &[(1, 90)],
    )
    .await;
    issue(
        &fx.w,
        &"2".repeat(32),
        "junk",
        "info",
        "p:completed:<script>",
        &file_unit("<img src=x onerror=alert(1)>.mkv"),
        &ev,
        5,
        5,
        &[(1, 91)],
    )
    .await;
    issue(
        &fx.w,
        &"3".repeat(32),
        "junk",
        "info",
        "p:completed:pipe",
        &file_unit("x/a|b,\"c\"\n.mkv"),
        &ev,
        5,
        5,
        &[(1, 92)],
    )
    .await;

    let bundle = fx.r.report(None, &inputs()).await.unwrap();
    assert!(
        bundle
            .explanations
            .iter()
            .any(|e| e.code == "caveat.btrfs_snapshots_may_retain_space")
    );
    for fmt in [
        ExportFormat::Json,
        ExportFormat::CsvIssues,
        ExportFormat::CsvHistory,
        ExportFormat::CsvSeries,
        ExportFormat::Markdown,
        ExportFormat::Html,
    ] {
        let f = export::render(&bundle, fmt).unwrap();
        let text = String::from_utf8(f.bytes).unwrap();
        for bad in [
            "SUPERSECRET",
            "PASSKEYVALUE",
            "private.tracker",
            "t.example",
        ] {
            assert!(!text.contains(bad), "{fmt:?} leaks {bad}");
        }
        assert!(f.file_name.starts_with("removarr-") && !f.file_name.contains(['/', ' ']));
    }
    let csv = export::issues_csv(&bundle);
    assert!(csv.lines().all(|l| !l.starts_with('=')));
    assert!(csv.contains("'=cmd"), "formula neutralized: {csv}");
    let html = export::to_html(&bundle);
    assert!(!html.contains("<img") && html.contains("&lt;img"));
    let md = export::to_markdown(&bundle);
    assert!(!md.contains("<img"));
    assert!(md.contains("Potential gain") && md.contains("not guaranteed"));
    assert!(!md.to_lowercase().contains("recovered space"));
    let json: serde_json::Value = serde_json::from_str(&export::to_json(&bundle).unwrap()).unwrap();
    assert!(json["dashboard"]["space"]["potential_label_key"].is_string());
    assert!(
        json["issues"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["potential_label_key"].is_string())
    );
}
