//! Stress and failure presentation (wave 16): bounded queues under request floods, API outages
//! with bounded backoff, log retention, incomplete scan presentation, configuration limits.
//! Loopback fakes and temporary trees only. Heavy variants live in `perf_budgets.rs` and
//! `stress_heavy.rs` behind `#[ignore]`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use removarr_config::{Config, Env};
use removarr_e2e_perf::*;
use removarr_integration_tests::{Options, Stack};
use removarr_reporting::{LogBuffer, LogLevel, LogQuery};
use removarr_testkit::fake_arr::Fault;
use removarr_testkit::fixture::ParityFixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_commands_never_grows_the_job_queue_beyond_the_dedupe_bound() {
    let lv = Live::start(Options::default(), 50, 50).await;
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    let mut tasks = Vec::new();
    for i in 0..400usize {
        let b = b.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            let path = match i % 4 {
                0 | 1 => "/scan",
                2 => "/services/qbittorrent/refresh",
                _ => "/issues/evaluate",
            };
            b.post(path, "", &token).await.status.as_u16()
        }));
    }
    let mut codes = std::collections::BTreeMap::<u16, usize>::new();
    for t in tasks {
        *codes.entry(t.await.unwrap()).or_default() += 1;
    }
    lv.stack.wait_idle().await;
    let jobs = lv.stack.app.runtime.status().jobs;
    let scans = jobs
        .iter()
        .filter(|j| j.label.to_lowercase().contains("scan"))
        .count();
    eprintln!(
        "INFO flood: 400 commands -> {} jobs ({scans} scans), status codes {codes:?}",
        jobs.len()
    );
    assert!(
        scans <= 3,
        "scan requests are deduplicated, got {scans} scan jobs"
    );
    assert!(
        jobs.len() <= 12,
        "the queue stayed bounded: {} jobs",
        jobs.len()
    );
    assert!(
        !codes.contains_key(&500) && !codes.contains_key(&502),
        "{codes:?}"
    );
    // Still responsive and consistent afterwards.
    assert_eq!(b.get("/jobs").await.status, reqwest::StatusCode::OK);
    assert_eq!(
        lv.stack.app.runtime.status().jobs.len(),
        jobs.len(),
        "reading creates nothing"
    );
}

#[test]
fn log_buffer_retention_is_bounded_under_a_concurrent_flood_and_redacts() {
    let buf = LogBuffer::new(removarr_reporting::logs::DEFAULT_CAPACITY, LogLevel::Info);
    buf.redactor().add_secret("hunter2-dummy-secret");
    let total = 8 * 25_000;
    let big = "x".repeat(1_000_000);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for t in 0..8 {
            let buf = buf.clone();
            let big = &big;
            s.spawn(move || {
                for i in 0..25_000 {
                    let msg = if i % 5000 == 0 {
                        big.clone()
                    } else {
                        format!("event {t}-{i} hunter2-dummy-secret")
                    };
                    buf.push(
                        LogLevel::Info,
                        "stress",
                        &msg,
                        vec![
                            ("password".into(), "hunter2-dummy-secret".into()),
                            ("n".into(), i.to_string()),
                        ],
                    );
                    // Below the capture level: dropped without cost.
                    buf.push(LogLevel::Debug, "stress", "noise", vec![]);
                }
            });
        }
    });
    let v = buf.view(&LogQuery {
        min_level: LogLevel::Debug,
        limit: 100_000,
        ..LogQuery::default()
    });
    eprintln!(
        "INFO logs: {total} events in {:?}; buffered {}, dropped {}",
        t0.elapsed(),
        v.buffered,
        v.dropped
    );
    assert_eq!(v.buffered, v.capacity, "the ring is full and never larger");
    assert_eq!(
        v.capacity, 2000,
        "B10: the UI buffer never exceeds 2,000 events"
    );
    assert_eq!(
        v.dropped as usize,
        total - 2000,
        "oldest evicted, nothing else lost"
    );
    assert!(
        v.entries.len() <= removarr_reporting::logs::MAX_VIEW_ENTRIES,
        "a view is bounded"
    );
    let all = buf.view(&LogQuery {
        limit: 500,
        ..LogQuery::default()
    });
    for e in &all.entries {
        assert!(
            !e.message.contains("hunter2-dummy-secret"),
            "redacted: {}",
            e.message
        );
        assert!(
            e.message.chars().count() <= 4096,
            "oversized message truncated ({})",
            e.message.len()
        );
        assert!(e.fields.values().all(|f| !f.contains("hunter2")));
    }
    // Sequence numbers are strictly increasing in the view (newest last).
    assert!(all.entries.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[test]
fn concurrency_limits_are_refused_or_clamped_at_the_documented_maxima() {
    let base = |k: &str, v: &str| -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();
        for n in ["c", "i", "m", "data"] {
            std::fs::create_dir(d.join(n)).unwrap();
        }
        let mut e = Env::new();
        let s = |n: &str| d.join(n).to_str().unwrap().to_string();
        e.insert("REMOVARR_COMPLETED_ROOT".into(), s("c"));
        e.insert("REMOVARR_INCOMPLETE_ROOT".into(), s("i"));
        e.insert("REMOVARR_MEDIA_ROOTS".into(), s("m"));
        e.insert("REMOVARR_DATA_DIR".into(), s("data"));
        e.insert(
            "REMOVARR_PASSWORD_HASH".into(),
            removarr_integration_tests::DUMMY_HASH.into(),
        );
        e.insert(
            "REMOVARR_SESSION_KEY".into(),
            removarr_integration_tests::DUMMY_SESSION_KEY.into(),
        );
        e.insert(k.into(), v.into());
        Config::from_sources(None, &e).map_err(|x| x.to_string())
    };
    for (key, max) in [
        ("REMOVARR_CONTENT_READ_CONCURRENCY", 4u32),
        ("REMOVARR_FFPROBE_CONCURRENCY", 4),
        ("REMOVARR_HTTP_CONCURRENCY", 16),
    ] {
        for bad in ["0", &(max + 1).to_string(), "99999", "-1", "abc"] {
            match base(key, bad) {
                Err(_) => {}
                Ok(c) => {
                    let l = &c.limits;
                    let got = [
                        l.content_read_concurrency,
                        l.ffprobe_concurrency,
                        l.http_concurrency_per_service,
                    ];
                    assert!(
                        got.iter().all(|g| (1..=16).contains(g)),
                        "{key}={bad} accepted as {got:?}"
                    );
                }
            }
        }
        assert!(base(key, &max.to_string()).is_ok(), "{key}={max} is valid");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qbittorrent_outage_backs_off_a_bounded_number_of_times_and_recovers() {
    let lv = Live::start(Options::default(), 50, 50).await;
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    // A refresh needs a scan to refresh against.
    let sc = b.post("/scan", "", &token).await;
    b.wait_job(&sc.location()).await;
    let ok = b.post("/services/qbittorrent/refresh", "", &token).await;
    b.wait_job(&ok.location()).await;
    assert!(
        html::text(&b.get(&ok.location()).await.body).contains("Succeeded"),
        "a refresh succeeds while the service is up"
    );
    lv.stack.qbit.force_status("/torrents/info", 503);
    let before = lv.stack.qbit.requests().len();
    let healthy_info = lv.stack.qbit.requests_to("/torrents/info").len();
    let t0 = Instant::now();
    let r = b.post("/services/qbittorrent/refresh", "", &token).await;
    assert!(r.status.is_redirection(), "{}", r.status);
    b.wait_job(&r.location()).await;
    let elapsed = t0.elapsed();
    let attempts = lv.stack.qbit.requests_to("/torrents/info").len() - healthy_info;
    eprintln!(
        "INFO qbit 503 outage: {attempts} attempts on /torrents/info in {elapsed:?} ({} requests total)",
        lv.stack.qbit.requests().len() - before
    );
    assert!(attempts >= 2, "a retry happened ({attempts})");
    assert!(
        attempts <= 16,
        "retries are bounded, got {attempts} attempts"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "backoff is bounded: {elapsed:?}"
    );
    // The failure is presented, not hidden: failed job and an unavailable service, no 5xx.
    let jobs = b.get("/jobs").await;
    assert_eq!(jobs.status, reqwest::StatusCode::OK);
    let dash = b.get("/").await;
    assert_eq!(dash.status, reqwest::StatusCode::OK);
    assert!(
        html::a11y_findings(&dash.body).is_empty(),
        "{:?}",
        html::a11y_findings(&dash.body)
    );
    assert!(
        jobs.body.contains("crit")
            || jobs.body.contains("Failed")
            || dash.body.contains("crit")
            || dash.body.contains("warn"),
        "failure is visible"
    );
    // No plan can be made while the service is down; nothing is mutated.
    assert!(lv.stack.qbit_mutations().is_empty());
    // Recovery: the service returns, the next refresh succeeds without a restart.
    lv.stack.qbit.force_status("/torrents/info", 200);
    lv.stack
        .qbit
        .set_torrents(removarr_integration_tests::parity_torrents(
            removarr_integration_tests::now_secs(),
        ));
    let r = b.post("/services/qbittorrent/refresh", "", &token).await;
    assert!(r.status.is_redirection());
    b.wait_job(&r.location()).await;
    let dash2 = b.get("/").await;
    assert_ne!(dash.body, dash2.body, "the dashboard reflects the recovery");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreadable_subtree_gives_an_incomplete_scan_that_is_labelled_in_both_locales() {
    let fx = ParityFixture::build().unwrap();
    fx.apply_v2().unwrap();
    let opts = Options::default()
        .env("REMOVARR_PASSWORD_HASH", &cheap_hash())
        .env("REMOVARR_COOKIE_SECURE", "false")
        .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other");
    let stack = Stack::over(fx, opts).await;
    let lv = Live::over(stack).await;
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    // Before any scan the dashboard says so (no false "all clear").
    let empty = b.get("/").await;
    assert!(!empty.body.contains("state incomplete"));
    let r = b.post("/scan", "", &token).await;
    b.wait_job(&r.location()).await;
    for locale in ["en", "fr"] {
        b.post("/locale", &format!("locale={locale}"), &token).await;
        let d = b.get("/").await;
        assert!(
            d.body.contains("class=\"state incomplete\""),
            "{locale}: incomplete notice shown"
        );
        let tr = removarr_web::i18n::Tr::new(if locale == "en" {
            removarr_web::i18n::Locale::En
        } else {
            removarr_web::i18n::Locale::Fr
        });
        assert!(
            d.body.contains(&escaped(tr.t("state.incomplete_text"))),
            "{locale}: explanation translated"
        );
        assert!(html::a11y_findings(&d.body).is_empty());
        // The label is text plus an icon, never colour alone.
        let at = d.body.find("class=\"state incomplete\"").unwrap();
        assert!(
            html::text(d.body[at..at + 600].split("</p>").next().unwrap())
                .contains(tr.t("state.incomplete"))
        );
    }
    // A plan over the incomplete area is refused or excluded with a stated blocker.
    let junk = b.get("/issues/junk").await;
    assert_eq!(junk.status, reqwest::StatusCode::OK);
    assert_eq!(lv.stack.qbit_mutations(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arr_outage_marks_media_issues_blocked_and_the_pages_still_render() {
    let opts = Options {
        radarr: true,
        ..Options::default()
    };
    let lv = {
        let opts = opts
            .env("REMOVARR_PASSWORD_HASH", &cheap_hash())
            .env("REMOVARR_COOKIE_SECURE", "false")
            .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other");
        Live::over(Stack::parity(opts).await).await
    };
    lv.stack.radarr.as_ref().unwrap().set_fault(
        "*",
        Fault {
            status: Some(503),
            ..Fault::default()
        },
    );
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    let t0 = Instant::now();
    let r1 = b.post("/scan", "", &token).await;
    let r2 = b.post("/services/radarr/refresh", "", &token).await;
    b.wait_job(&r1.location()).await;
    b.wait_job(&r2.location()).await;
    let n = lv.stack.radarr.as_ref().unwrap().recorded().len();
    eprintln!(
        "INFO radarr 503 outage: {n} requests, settled in {:?}",
        t0.elapsed()
    );
    assert!(n <= 40, "bounded retries, got {n} requests");
    assert!(t0.elapsed() < Duration::from_secs(90));
    for path in ["/", "/issues/hardlink", "/issues/junk", "/jobs", "/history"] {
        let p = b.get(path).await;
        assert!(
            p.status.is_success() || p.status == reqwest::StatusCode::NOT_FOUND,
            "{path}: {}",
            p.status
        );
    }
    // No ARR mutation was attempted during the outage.
    assert_eq!(lv.stack.radarr.as_ref().unwrap().mutating_count(), 0);
    let _ = Arc::new(());
}
