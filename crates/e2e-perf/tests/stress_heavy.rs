//! Heavy stress over the real release binary and the reference fixture (`#[ignore]`):
//! cancellation, SIGTERM and SIGKILL during a scan, restart over the same database, and a start
//! with unavailable qBittorrent. Run explicitly:
//!
//! ```sh
//! cargo build --release -p removarr
//! cargo test -p removarr-e2e-perf --test stress_heavy -- --ignored --nocapture --test-threads=1
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use removarr_e2e_perf::perf::*;
use removarr_e2e_perf::*;
use removarr_testkit::fake_arr::FakeArr;
use removarr_testkit::fake_qbit::FakeQbit;

struct Env {
    _work: tempfile::TempDir,
    dir: PathBuf,
    tree: PathBuf,
    data: PathBuf,
    syn: Synth,
    hash: String,
    qbit: FakeQbit,
    radarr: FakeArr,
}

async fn setup() -> Env {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().canonicalize().unwrap();
    let tree = dir.join("tree");
    std::fs::create_dir(&tree).unwrap();
    let data = dir.join("config");
    std::fs::create_dir(&data).unwrap();
    let syn = generate(&tree, Spec::REFERENCE);
    let qbit = FakeQbit::spawn("v5.0.4").await;
    qbit.set_torrents(syn.torrents.clone());
    let radarr = FakeArr::start(syn.arr.clone()).await;
    Env {
        _work: work,
        dir,
        tree,
        data,
        syn,
        hash: hash_with(argon2::Params::new(256, 1, 1, None).unwrap(), PASSWORD),
        qbit,
        radarr,
    }
}

impl Env {
    fn start(&self, extra: &[(&str, &str)]) -> Proc {
        let env = app_env(
            &self.syn,
            &self.data,
            &self.qbit.url(),
            &self.radarr.base_url(),
            &self.hash,
            extra,
        );
        Proc::spawn(&self.dir, &env, free_port())
    }
}

async fn signed_in(p: &Proc) -> (Browser, String) {
    p.wait_ready(Duration::from_secs(30)).await.expect("ready");
    let b = Browser::new(&p.base);
    assert!(b.login(USER, PASSWORD).await.status.is_redirection());
    let t = b.token().await;
    (b, t)
}

async fn status_of(b: &Browser, loc: &str) -> String {
    let p = b.get(&format!("{loc}/progress")).await;
    let t = html::text(&p.body);
    for s in ["Succeeded", "Cancelled", "Failed", "Interrupted"] {
        if t.contains(s) {
            return s.into();
        }
    }
    "Active".into()
}

async fn settle(b: &Browser, loc: &str) -> String {
    let t0 = Instant::now();
    loop {
        let s = status_of(b, loc).await;
        if s != "Active" {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(300));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn junk_count(body: &str) -> usize {
    let mut v: Vec<&str> = body
        .match_indices("href=\"/issues/junk/")
        .map(|(i, _)| &body[i..i + 40])
        .collect();
    v.dedup();
    v.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "heavy: cancellation, SIGTERM and SIGKILL under load on the reference fixture"]
async fn cancel_sigterm_and_sigkill_during_a_scan_then_restart_converge() {
    let e = setup().await;
    let sig0 = tree_signature(&e.tree);
    let mut app = e.start(&[("REMOVARR_SHUTDOWN_GRACE_SECONDS", "10")]);
    let (b, token) = signed_in(&app).await;

    // 1. Cancel a scan in flight (the first scan, so there is no earlier complete one).
    let mut cancelled = false;
    for delay_ms in [150u64, 60, 20, 5] {
        let r = b.post("/scan", "", &token).await;
        let loc = r.location();
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let c = b.post(&format!("{loc}/cancel"), "", &token).await;
        assert!(
            c.status.is_redirection() || c.status.is_success() || c.status.is_client_error(),
            "{}",
            c.status
        );
        let end = settle(&b, &loc).await;
        eprintln!("INFO cancel after {delay_ms} ms -> {end}");
        assert!(["Cancelled", "Succeeded"].contains(&end.as_str()), "{end}");
        if end == "Cancelled" {
            cancelled = true;
            break;
        }
    }
    assert!(cancelled, "a scan could be cancelled mid-flight");
    // A cancelled first scan must not be presented as a complete one: no false all-clear.
    let dash = b.get("/").await;
    assert_eq!(dash.status, reqwest::StatusCode::OK);
    assert!(
        dash.body.contains("state incomplete")
            || dash.body.contains("state cancelled")
            || dash.body.contains("class=\"state\""),
        "the dashboard says the data is incomplete or absent"
    );
    assert!(html::a11y_findings(&dash.body).is_empty());
    assert_eq!(e.qbit.mutation_count(), 0);

    // 2. A full scan afterwards completes and fills the lists.
    let r = b.post("/scan", "", &token).await;
    assert_eq!(settle(&b, &r.location()).await, "Succeeded");
    let full = junk_count(&b.get("/issues/junk?per_page=200").await.body);
    assert!(full >= 100, "{full}");

    // 3. SIGKILL in the middle of a rescan: no shutdown path at all.
    let r = b.post("/scan", "", &token).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    app.kill9();
    let before_restart = Instant::now();
    let mut app = e.start(&[("REMOVARR_SHUTDOWN_GRACE_SECONDS", "10")]);
    let ready = app
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready after SIGKILL");
    eprintln!(
        "INFO ready {:?} after SIGKILL restart over a {:.0} MiB database",
        ready,
        db_bytes(&e.data) as f64 / 1048576.0
    );
    assert!(before_restart.elapsed() < Duration::from_secs(10));
    let b = Browser::new(&app.base);
    assert!(b.login(USER, PASSWORD).await.status.is_redirection());
    let token = b.token().await;
    let jobs_after = html::text(&b.get("/jobs").await.body);
    let dash_after = html::text(&b.get("/").await.body);
    eprintln!(
        "INFO after SIGKILL: GET {} -> {}",
        r.location(),
        b.get(&r.location()).await.status
    );
    eprintln!(
        "INFO /jobs after restart mentions Interrupted: {}",
        jobs_after.contains("Interrupted")
    );
    eprintln!(
        "INFO dashboard after restart says incomplete: {}",
        dash_after.contains("Some data could not be read")
    );
    let log = std::fs::read_to_string(&app.stdout_path).unwrap();
    let started = log
        .lines()
        .find(|l| l.contains("job runtime started"))
        .unwrap_or_default()
        .to_string();
    eprintln!(
        "INFO restart log: {}",
        started
            .split("\"fields\":")
            .nth(1)
            .unwrap_or("")
            .split('}')
            .next()
            .unwrap_or("")
    );
    assert!(
        started.contains("\"interrupted\":\"1\"") && started.contains("\"requeued\":\"1\""),
        "the killed scan is recorded as interrupted and re-queued once: {started}"
    );
    // The re-queued scan is visible and completes by itself.
    assert!(
        jobs_after.to_lowercase().contains("scan"),
        "the re-queued scan is listed"
    );
    // The observe job is re-queued once; wait for the lists to be whole again.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let r = b.post("/scan", "", &token).await;
    assert_eq!(settle(&b, &r.location()).await, "Succeeded");
    assert_eq!(
        junk_count(&b.get("/issues/junk?per_page=200").await.body),
        full,
        "same inventory after the crash"
    );

    // 4. SIGTERM during a scan: graceful, exit 0 within the grace period.
    let r = b.post("/scan", "", &token).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = Instant::now();
    let _ = r;
    let totals = app.terminate();
    eprintln!(
        "INFO SIGTERM during a scan: stopped in {:?}; I/O {totals:?}",
        t0.elapsed()
    );
    assert!(t0.elapsed() < Duration::from_secs(15));
    let mut app = e.start(&[]);
    let (b, token) = signed_in(&app).await;
    let r = b.post("/scan", "", &token).await;
    assert_eq!(settle(&b, &r.location()).await, "Succeeded");
    app.terminate();

    // B11/B3: through all of that, nothing in the roots changed.
    assert!(
        sig0 == tree_signature(&e.tree),
        "the synthetic tree changed"
    );
    assert_eq!(e.qbit.mutation_count(), 0);
    assert_eq!(e.radarr.mutating_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "heavy: start and scan with qBittorrent and Radarr unreachable, then recover"]
async fn unreachable_apis_at_start_do_not_block_readiness_or_the_scan_and_recovery_needs_no_restart()
 {
    let e = setup().await;
    let dead = free_port();
    let dead_url = format!("http://127.0.0.1:{dead}");
    let env = app_env(&e.syn, &e.data, &dead_url, &dead_url, &e.hash, &[]);
    let mut app = Proc::spawn(&e.dir, &env, free_port());
    let ready = app
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");
    eprintln!("INFO ready with unreachable APIs after {ready:?}");
    assert!(
        ready < Duration::from_secs(3),
        "B9 even when the services are down"
    );
    let b = Browser::new(&app.base);
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    let t0 = Instant::now();
    let r = b.post("/scan", "", &token).await;
    let end = settle(&b, &r.location()).await;
    eprintln!("INFO scan with APIs down: {end} in {:?}", t0.elapsed());
    assert!(["Succeeded", "Failed"].contains(&end.as_str()));
    assert!(t0.elapsed() < Duration::from_secs(120), "bounded retries");
    for p in [
        "/",
        "/issues/hardlink",
        "/issues/junk",
        "/jobs",
        "/history",
        "/auto",
        "/logs",
    ] {
        let page = b.get(p).await;
        assert!(
            page.status.is_success() || page.status == reqwest::StatusCode::NOT_FOUND,
            "{p}: {}",
            page.status
        );
    }
    let created = b
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={}", "x"),
            &token,
        )
        .await;
    assert!(
        created.status.is_client_error(),
        "no plan from nothing: {}",
        created.status
    );
    assert!(app.alive());
    app.terminate();
    assert_eq!(e.qbit.mutation_count(), 0);
}
