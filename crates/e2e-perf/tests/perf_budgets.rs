//! Resource budgets B1-B11 (architecture section 14) against the real release binary over the
//! reference synthetic fixture (20k+ files with hardlinks, 3,000 torrents on a loopback
//! qBittorrent fake, 2,000 media items on a loopback Radarr fake). Everything is `#[ignore]`d:
//!
//! ```sh
//! cargo build --release -p removarr
//! cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `PERF_IDLE_SECS` (default 60) is the idle observation window (the budget text asks for
//! 5 minutes of CPU average; use 300 for the final figure). Results are printed as `BUDGET`
//! lines and appended to `$PERF_RESULTS` when set. A miss fails the test (it blocks the release).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::Path;
use std::time::{Duration, Instant};

use argon2::Params;
use removarr_e2e_perf::perf::*;
use removarr_e2e_perf::*;
use removarr_testkit::fake_arr::FakeArr;
use removarr_testkit::fake_qbit::FakeQbit;

const MIB: f64 = 1024.0 * 1024.0;

struct Report {
    rows: Vec<(String, String, String, bool)>,
}

impl Report {
    fn check(&mut self, id: &str, what: &str, measured: f64, limit: f64, unit: &str) {
        let ok = measured <= limit;
        let line = format!(
            "BUDGET {id} {what}: measured {measured:.2} {unit} (limit {limit:.2} {unit}) {}",
            if ok { "PASS" } else { "MISS" }
        );
        eprintln!("{line}");
        if let Ok(f) = std::env::var("PERF_RESULTS") {
            record(Path::new(&f), &line);
        }
        self.rows
            .push((id.into(), what.into(), format!("{measured:.2} {unit}"), ok));
    }
    fn info(&self, line: &str) {
        eprintln!("INFO {line}");
        if let Ok(f) = std::env::var("PERF_RESULTS") {
            record(Path::new(&f), &format!("INFO {line}"));
        }
    }
}

async fn job_done(b: &Browser, location: &str, limit: Duration) -> (Duration, String) {
    let t0 = Instant::now();
    let id = location.trim_start_matches("/jobs/").to_string();
    loop {
        let p = b.get(&format!("/jobs/{id}/progress")).await;
        if !p.body.contains("hx-trigger=\"every 2s\"") {
            return (t0.elapsed(), p.body);
        }
        assert!(t0.elapsed() < limit, "job did not finish in {limit:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn latencies(b: &Browser, path: &str, n: usize) -> Vec<Duration> {
    let mut v = Vec::new();
    for _ in 0..n {
        let t = Instant::now();
        let p = b.get(path).await;
        v.push(t.elapsed());
        assert!(p.status.is_success(), "{path}: {}", p.status);
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: reference fixture budgets B1-B11, needs target/release/removarr"]
async fn perf_reference_fixture_budgets_b1_to_b11() {
    let mut rep = Report { rows: Vec::new() };
    // `PERF_WORK=<dir>` keeps the working tree (database, logs) for inspection after the run.
    let work = tempfile::tempdir().unwrap();
    let work_dir = match std::env::var("PERF_WORK") {
        Ok(d) => {
            std::fs::create_dir_all(&d).unwrap();
            std::path::PathBuf::from(d).canonicalize().unwrap()
        }
        Err(_) => work.path().canonicalize().unwrap(),
    };
    let tree = work_dir.join("tree");
    std::fs::create_dir(&tree).unwrap();
    let syn = generate(&tree, Spec::REFERENCE);
    rep.info(&format!(
        "fixture: {} files, {} dirs, {} hardlinks, {} torrents, {} radarr movies, {:.1} MiB apparent, generated in {:.1}s",
        syn.counts.files,
        syn.counts.dirs,
        syn.counts.hardlinks,
        syn.torrents.len(),
        syn.arr.movies.len(),
        syn.counts.bytes as f64 / MIB,
        syn.gen_time.as_secs_f64()
    ));
    assert!(syn.counts.files >= 20_000 && syn.torrents.len() == 3000);
    let data = work_dir.join("config");
    std::fs::create_dir(&data).unwrap();

    let qbit = FakeQbit::spawn("v5.0.4").await;
    qbit.set_torrents(syn.torrents.clone());
    let radarr = FakeArr::start(syn.arr.clone()).await;
    let prod_hash = removarr_web::auth::hash_password(PASSWORD).unwrap();
    let env = app_env(
        &syn,
        &data,
        &qbit.url(),
        &radarr.base_url(),
        &prod_hash,
        &[],
    );
    let before_tree = tree_signature(&tree);

    // ---- B9 startup ------------------------------------------------------------------------
    let mut app = Proc::spawn(&work_dir, &env, free_port());
    let Some(ready) = app.wait_ready(Duration::from_secs(30)).await else {
        panic!(
            "no /readyz\nstdout:\n{}\nstderr:\n{}",
            std::fs::read_to_string(&app.stdout_path).unwrap_or_default(),
            std::fs::read_to_string(work_dir.join("stderr.log")).unwrap_or_default()
        );
    };
    rep.check("B9", "startup to /readyz", ready.as_secs_f64(), 3.0, "s");
    let b = Browser::new(&app.base);
    let t = Instant::now();
    let r = b.login(USER, PASSWORD).await;
    let login_ms = t.elapsed();
    assert_eq!(r.status, reqwest::StatusCode::SEE_OTHER, "{}", r.body);
    rep.info(&format!(
        "login with production Argon2id parameters (2 requests): {:.0} ms",
        login_ms.as_secs_f64() * 1000.0
    ));
    let token = b.token().await;
    let db0 = db_bytes(&data);
    let lines0 = app.stdout_lines();
    rep.info(&format!(
        "RSS after start {} KiB, db {} bytes",
        rss_kib(app.pid).unwrap_or(0),
        db0
    ));

    // ---- B2 / B4 / B5 / B10: first full scan -------------------------------------------------
    app.reset_peak();
    let t = Instant::now();
    let r = b.post("/scan", "", &token).await;
    assert_eq!(r.status, reqwest::StatusCode::SEE_OTHER, "{}", r.body);
    let (scan_time, body) = job_done(&b, &r.location(), Duration::from_secs(600)).await;
    assert!(
        body.contains("badge ok") || body.contains("Succeeded") || body.contains("check-circle"),
        "scan did not succeed: {body}"
    );
    rep.check(
        "B4",
        "full scan of the fixture (scan + evaluation)",
        t.elapsed().as_secs_f64(),
        60.0,
        "s",
    );
    let _ = scan_time;
    rep.check(
        "B2",
        "RSS peak during the full scan (sampled 100 ms)",
        app.peak_kib() as f64 / 1024.0,
        160.0,
        "MiB",
    );
    let wal_db1 = db_bytes(&data);
    let truncated = checkpoint(&data);
    let db1 = db_bytes(&data);
    rep.info(&format!("db after the scan: {:.1} MiB with WAL, {:.1} MiB after wal_checkpoint(TRUNCATE) (sqlite3 CLI: {truncated})", wal_db1 as f64 / MIB, db1 as f64 / MIB));
    rep.info(&format!(
        "issues: {:?}; fs_entries: {:?}; torrents: {:?}",
        sqlite_scalar(
            &data,
            "select count(*) from issues where resolved_at is null"
        ),
        sqlite_scalar(&data, "select count(*) from fs_entries"),
        sqlite_scalar(&data, "select count(*) from torrents"),
    ));
    rep.check(
        "B5",
        "DB writes, initial scan (db size after checkpoint, minus before)",
        (db1 - db0) as f64 / MIB,
        64.0,
        "MiB",
    );
    let first_lines = app.stdout_lines() - lines0;
    rep.check(
        "B10",
        "INFO lines for the scan",
        first_lines as f64,
        200.0,
        "lines",
    );
    rep.info(&format!(
        "{} requests to the qBittorrent fake, {} to Radarr during the scan",
        qbit.requests().len(),
        radarr.recorded().len()
    ));

    // ---- unchanged rescan ----------------------------------------------------------------------
    let lines1 = app.stdout_lines();
    app.reset_peak();
    let t = Instant::now();
    let r = b.post("/scan", "", &token).await;
    let (_, body) = job_done(&b, &r.location(), Duration::from_secs(600)).await;
    assert!(!body.contains("badge crit"), "{body}");
    rep.check(
        "B4",
        "unchanged rescan",
        t.elapsed().as_secs_f64(),
        30.0,
        "s",
    );
    rep.check(
        "B2",
        "RSS peak during the unchanged rescan",
        app.peak_kib() as f64 / 1024.0,
        160.0,
        "MiB",
    );
    let _ = checkpoint(&data);
    let db2 = db_bytes(&data);
    rep.check(
        "B5",
        "DB writes, unchanged rescan (db size growth after checkpoint)",
        db2.saturating_sub(db1) as f64 / MIB,
        4.0,
        "MiB",
    );
    rep.check(
        "B10",
        "INFO lines for the unchanged rescan",
        (app.stdout_lines() - lines1) as f64,
        200.0,
        "lines",
    );
    rep.info(&format!("db after scans: {:.1} MiB", db2 as f64 / MIB));

    // ---- B1 / B3: roots renamed away, no jobs, no HTTP traffic ---------------------------------------
    for d in ["completed", "incomplete", "medias"] {
        std::fs::rename(tree.join(d), tree.join(format!("{d}.away"))).unwrap();
    }
    let idle = std::env::var("PERF_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    let rss_after_scan = rss_kib(app.pid).unwrap_or(0) as f64 / 1024.0;
    let cpu0 = cpu_seconds(app.pid).unwrap_or(0.0);
    let lines_idle0 = app.stdout_lines();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(idle) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let cpu = cpu_seconds(app.pid).unwrap_or(0.0) - cpu0;
    rep.check(
        "B1",
        &format!("RSS idle {idle}s after the scan (no jobs, no traffic)"),
        rss_kib(app.pid).unwrap_or(0) as f64 / 1024.0,
        40.0,
        "MiB",
    );
    rep.info(&format!(
        "RSS right after the scan: {rss_after_scan:.1} MiB"
    ));
    rep.check(
        "B3",
        &format!("CPU idle over {idle}s (no traffic)"),
        100.0 * cpu / t.elapsed().as_secs_f64(),
        1.0,
        "% of one core",
    );
    rep.check(
        "B3",
        "log lines while idle",
        (app.stdout_lines() - lines_idle0) as f64,
        0.0,
        "lines",
    );

    // ---- B7 latencies with the roots renamed away ----------------------------------------------
    for (name, path, n, limit_ms) in [
        ("dashboard", "/", 200, 100.0),
        (
            "issue list, 50 rows",
            "/issues/junk?per_page=50",
            200,
            100.0,
        ),
        (
            "issue list, page 40 of junk",
            "/issues/junk?per_page=50&page=40",
            200,
            100.0,
        ),
        (
            "issue list with search",
            "/issues/junk?per_page=50&q=junk-004",
            200,
            100.0,
        ),
        (
            "issue list, 200 rows (max page size) of 1000 read",
            "/issues/junk?per_page=200",
            50,
            100.0,
        ),
        ("jobs", "/jobs", 200, 100.0),
        ("history", "/history", 200, 100.0),
    ] {
        let v = latencies(&b, path, n).await;
        rep.check(
            "B7",
            &format!("{name} p95"),
            quantile(&v, 0.95).as_secs_f64() * 1000.0,
            limit_ms,
            "ms",
        );
    }
    // HTMX fragment: the job progress partial.
    let jobs = b.get("/jobs").await;
    let first_job = regex::Regex::new(r#"href="/jobs/([^"]+)""#)
        .unwrap()
        .captures(&jobs.body)
        .unwrap()[1]
        .to_string();
    let v = latencies(&b, &format!("/jobs/{first_job}/progress"), 200).await;
    rep.check(
        "B7",
        "HTMX fragment (job progress) p95",
        quantile(&v, 0.95).as_secs_f64() * 1000.0,
        50.0,
        "ms",
    );
    let mut v = Vec::new();
    let listing = b.get("/issues/junk?per_page=50").await;
    let re = regex::Regex::new(r#"/issues/junk/([^"]+)""#).unwrap();
    let ids: Vec<String> = re
        .captures_iter(&listing.body)
        .map(|c| format!("issue_ids={}", &c[1]))
        .collect();
    assert!(!ids.is_empty());
    for _ in 0..100 {
        let t = Instant::now();
        let p = b
            .post_raw(
                "/issues/selection-summary",
                &format!("category=junk&{}", ids.join("&")),
                &[("X-CSRF-Token", &token), ("HX-Request", "true")],
            )
            .await;
        v.push(t.elapsed());
        assert!(p.status.is_success(), "{}", p.status);
    }
    rep.check(
        "B7",
        "HTMX fragment (selection summary, 50 ids) p95",
        quantile(&v, 0.95).as_secs_f64() * 1000.0,
        50.0,
        "ms",
    );

    // ---- B3 / B6: every GET page and both refreshes with the roots away -------------------------------
    let warn0 = std::fs::read_to_string(&app.stdout_path)
        .unwrap()
        .lines()
        .count();
    // Manual refreshes read from the databases and the fakes only.
    for svc in ["qbittorrent", "radarr"] {
        let r = b
            .post(&format!("/services/{svc}/refresh"), "", &token)
            .await;
        assert!(
            r.status.is_redirection() || r.status.is_success(),
            "{svc}: {}",
            r.status
        );
        if r.status.is_redirection() {
            job_done(&b, &r.location(), Duration::from_secs(120)).await;
        }
    }
    let mut pages = vec![
        "/",
        "/jobs",
        "/history",
        "/stats",
        "/logs",
        "/auto",
        "/plans",
        "/ignored",
        "/issues/junk?per_page=200",
    ];
    let cats = [
        "junk",
        "duplicates",
        "orphans",
        "hardlink",
        "incomplete",
        "media-only",
    ];
    let cat_pages: Vec<String> = cats.iter().map(|c| format!("/issues/{c}")).collect();
    pages.extend(cat_pages.iter().map(String::as_str));
    for p in &pages {
        let r = b.get(p).await;
        assert!(
            r.status.is_success() || r.status == reqwest::StatusCode::NOT_FOUND,
            "{p}: {}",
            r.status
        );
    }
    let links: Vec<String> = regex::Regex::new(r#"href="(/issues/junk/[^"]+)""#)
        .unwrap()
        .captures_iter(&b.get("/issues/junk?per_page=20").await.body)
        .map(|c| c[1].to_string())
        .collect();
    for l in &links {
        assert!(b.get(l).await.status.is_success(), "{l}");
    }
    rep.info(&format!(
        "RSS after all UI traffic: {:.1} MiB (informational)",
        rss_kib(app.pid).unwrap_or(0) as f64 / 1024.0
    ));
    let new_lines: Vec<String> = std::fs::read_to_string(&app.stdout_path)
        .unwrap()
        .lines()
        .skip(warn0)
        .map(String::from)
        .collect();
    let fs_errors: Vec<&String> = new_lines
        .iter()
        .filter(|l| {
            let u = l.to_lowercase();
            (u.contains("\"level\":\"error\"") || u.contains("\"level\":\"warn\""))
                && (u.contains("no such file")
                    || u.contains("not found")
                    || u.contains("enoent")
                    || u.contains("scan")
                    || u.contains("root"))
        })
        .collect();
    rep.check(
        "B3",
        "filesystem errors logged while the roots are away",
        fs_errors.len() as f64,
        0.0,
        "lines",
    );
    for l in fs_errors.iter().take(3) {
        rep.info(&format!("  {l}"));
    }
    for d in ["completed", "incomplete", "medias"] {
        assert!(
            !tree.join(d).exists(),
            "{d} was recreated by the application"
        );
        std::fs::rename(tree.join(format!("{d}.away")), tree.join(d)).unwrap();
    }
    rep.check(
        "B3/B6",
        "root changes caused by idle and UI/refresh traffic",
        f64::from(u8::from(tree_signature(&tree) != before_tree)),
        0.0,
        "diffs",
    );

    // ---- B11: dry-run plan + simulation on the big fixture ------------------------------------------
    let listing = b.get("/issues/junk?per_page=50").await;
    let ids: Vec<String> = re
        .captures_iter(&listing.body)
        .map(|c| format!("issue_ids={}", &c[1]))
        .collect();
    let c = b
        .post(
            "/plans",
            &format!("category=junk&action=delete&{}", ids.join("&")),
            &token,
        )
        .await;
    assert_eq!(c.status, reqwest::StatusCode::SEE_OTHER, "{}", c.body);
    let plan = c.location();
    let plan_id = plan.rsplit('/').next().unwrap().to_string();
    let confirm = b.get(&format!("{plan}/confirm")).await;
    let hash = regex::Regex::new(r#"name="plan_hash" value="([0-9a-f]{64})""#)
        .unwrap()
        .captures(&confirm.body)
        .expect("hash")[1]
        .to_string();
    let ap = b
        .post(
            &format!("/plans/{plan_id}/approve"),
            &format!("plan_hash={hash}&confirmed_count={}", ids.len()),
            &token,
        )
        .await;
    assert_eq!(ap.status, reqwest::StatusCode::SEE_OTHER, "{}", ap.body);
    let ex = b
        .post(&format!("/plans/{plan_id}/execute"), "", &token)
        .await;
    assert!(
        ex.status.is_client_error(),
        "live execute must be refused in dry run: {}",
        ex.status
    );
    let sim = b
        .post(&format!("/plans/{plan_id}/simulate"), "", &token)
        .await;
    job_done(&b, &sim.location(), Duration::from_secs(120)).await;
    let after_tree = tree_signature(&tree);
    rep.check(
        "B11",
        "entries changed in the roots (dry run plan, simulation, scans)",
        before_tree
            .iter()
            .zip(after_tree.iter())
            .filter(|(a, b)| a != b)
            .count() as f64
            + before_tree.len().abs_diff(after_tree.len()) as f64,
        0.0,
        "entries",
    );
    assert_eq!(qbit.mutation_count(), 0, "no qBittorrent mutation");
    assert_eq!(radarr.mutating_count(), 0, "no Radarr mutation");

    let totals = app.terminate();
    rep.info(&format!(
        "process totals (time -l): max RSS {:.1} MiB, block in {}, block out {}",
        totals.max_rss_kib as f64 / 1024.0,
        totals.block_in,
        totals.block_out
    ));
    rep.check(
        "B8",
        "release binary (host target, not musl)",
        std::fs::metadata(release_binary()).unwrap().len() as f64 / MIB,
        30.0,
        "MiB",
    );
    let misses: Vec<_> = rep.rows.iter().filter(|r| !r.3).collect();
    assert!(misses.is_empty(), "budget misses: {misses:#?}");
}

#[test]
#[ignore = "benchmark: Argon2id with the production parameters (m=19456 KiB, t=2, p=1)"]
fn argon2_verify_time_with_production_parameters() {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let phc = removarr_web::auth::hash_password(PASSWORD).unwrap();
    assert!(phc.contains("m=19456,t=2,p=1"), "{phc}");
    let parsed = PasswordHash::new(&phc).unwrap();
    let a = argon2::Argon2::default();
    let mut v = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        assert!(a.verify_password(PASSWORD.as_bytes(), &parsed).is_ok());
        v.push(t.elapsed());
    }
    let _ = Params::DEFAULT_M_COST;
    let p50 = quantile(&v, 0.5).as_secs_f64() * 1000.0;
    let p95 = quantile(&v, 0.95).as_secs_f64() * 1000.0;
    eprintln!("INFO argon2id verify (m=19456,t=2,p=1): p50 {p50:.1} ms, p95 {p95:.1} ms");
    // A hash must cost something (>= 5 ms) yet never make a login feel slow (< 1 s).
    assert!(p50 >= 5.0, "suspiciously cheap hash: {p50} ms");
    assert!(p95 < 1000.0, "login verification too slow: {p95} ms");
}
