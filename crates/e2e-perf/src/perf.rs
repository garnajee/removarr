//! Synthetic inventory generator and out-of-process measurement for the budget benchmarks
//! (architecture section 14). The application runs as the real release binary so that RSS, CPU
//! and disk I/O are its own and not the test harness's.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use removarr_testkit::fake_arr::{ArrData, FAKE_API_KEY};
use removarr_testkit::fake_qbit::{FAKE_PASSWORD, FakeFile, FakeTorrent};
use removarr_testkit::fixture::bytes;
use serde_json::{Value, json};

/// Shape of the reference fixture (architecture section 14: 20,000 regular files across the
/// three roots, 60 % of completed videos hardlinked into media, 3,000 torrents, 2,000 media).
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub completed_torrents: u32,
    pub incomplete_torrents: u32,
    pub junk: u32,
    pub orphans: u32,
    pub media_only: u32,
    /// Completed videos that also exist as an exact, non-linked copy in media (comparison work).
    pub copies: u32,
    /// Hardlinked share of completed videos, in percent.
    pub linked_percent: u32,
}

impl Spec {
    pub const REFERENCE: Spec = Spec {
        completed_torrents: 2800,
        incomplete_torrents: 200,
        junk: 1000,
        orphans: 400,
        media_only: 1000,
        copies: 300,
        linked_percent: 60,
    };
}

#[derive(Debug, Default, Clone)]
pub struct Counts {
    pub files: u64,
    pub dirs: u64,
    pub hardlinks: u64,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct Synth {
    pub completed: PathBuf,
    pub incomplete: PathBuf,
    pub medias: PathBuf,
    pub counts: Counts,
    pub torrents: Vec<FakeTorrent>,
    pub arr: ArrData,
    pub gen_time: Duration,
}

fn write(p: &Path, label: &str, n: usize, c: &mut Counts) {
    fs::write(p, bytes(label, n)).unwrap();
    c.files += 1;
    c.bytes += n as u64;
}

fn mkdir(p: &Path, c: &mut Counts) {
    fs::create_dir_all(p).unwrap();
    c.dirs += 1;
}

fn hex40(n: u32) -> String {
    format!("{n:040x}")
}

/// Builds the tree under `base` (`completed`, `incomplete`, `medias`) and the matching fake
/// qBittorrent and Radarr data.
#[must_use]
pub fn generate(base: &Path, s: Spec) -> Synth {
    let t0 = Instant::now();
    let (completed, incomplete, medias) = (
        base.join("completed"),
        base.join("incomplete"),
        base.join("medias"),
    );
    let mut c = Counts::default();
    for d in [&completed, &incomplete, &medias.join("Movies")] {
        mkdir(d, &mut c);
    }
    let mut torrents = Vec::new();
    let mut movies = Vec::new();
    let mut movie_files = Vec::new();
    let mut movie_id = 0i64;
    let mut arr_movie = |folder: &str, file: &str, size: u64| {
        movie_id += 1;
        movies.push(
            json!({ "id": movie_id, "title": folder, "year": 2020, "tmdbId": 300_000 + movie_id,
            "path": format!("/media/Movies/{folder}"), "monitored": true, "qualityProfileId": 1 }),
        );
        movie_files.push(json!({ "id": movie_id, "movieId": movie_id,
            "path": format!("/media/Movies/{folder}/{file}"), "relativePath": file, "size": size,
            "quality": { "quality": { "id": 3, "name": "WEBDL-1080p", "source": "synthetic", "resolution": 1080 },
                         "revision": { "version": 1, "real": 0, "isRepack": false } },
            "customFormats": [], "customFormatScore": 0, "qualityCutoffNotMet": false,
            "languages": [{ "id": 1, "name": "English" }] }));
    };
    let linked = s.completed_torrents * s.linked_percent / 100;
    for i in 0..s.completed_torrents {
        let name = format!("Movie.{i:05}.2020.1080p.WEB.x264-GRP");
        let dir = completed.join(&name);
        mkdir(&dir, &mut c);
        let size = 4096 + (i as usize % 7) * 512;
        let video = format!("{name}.mkv");
        write(&dir.join(&video), &name, size, &mut c);
        write(
            &dir.join(format!("{name}.nfo")),
            &format!("nfo{i}"),
            128,
            &mut c,
        );
        let mut files = vec![(video.clone(), size as u64), (format!("{name}.nfo"), 128)];
        mkdir(&dir.join("Subs"), &mut c);
        for lang in ["en", "fr", "de", "es"] {
            write(
                &dir.join("Subs").join(format!("{name}.{lang}.srt")),
                &format!("srt{i}{lang}"),
                256,
                &mut c,
            );
            files.push((format!("Subs/{name}.{lang}.srt"), 256));
        }
        let folder = format!("{name} (2020)");
        if i < linked {
            mkdir(&medias.join("Movies").join(&folder), &mut c);
            fs::hard_link(
                dir.join(&video),
                medias.join("Movies").join(&folder).join(&video),
            )
            .unwrap();
            c.files += 1;
            c.hardlinks += 1;
            write(
                &medias.join("Movies").join(&folder).join("poster.jpg"),
                &format!("poster{i}"),
                64,
                &mut c,
            );
            arr_movie(&folder, &video, size as u64);
        } else if i - linked < s.copies {
            mkdir(&medias.join("Movies").join(&folder), &mut c);
            write(
                &medias.join("Movies").join(&folder).join(&video),
                &name,
                size,
                &mut c,
            );
            write(
                &medias.join("Movies").join(&folder).join("poster.jpg"),
                &format!("poster{i}"),
                64,
                &mut c,
            );
            arr_movie(&folder, &video, size as u64);
        }
        let mut t = FakeTorrent::new(&hex40(i + 1), &name, "stalledUP", 1.0)
            .paths("/data/completed", &format!("/data/completed/{name}"))
            .added_on(1_700_000_000);
        for (f, n) in files {
            t = t.file(FakeFile::new(&format!("{name}/{f}"), n));
        }
        torrents.push(t);
    }
    for i in 0..s.media_only {
        let folder = format!("Media.Only.{i:05}.2019 (2019)");
        let file = format!("Media.Only.{i:05}.2019.mkv");
        mkdir(&medias.join("Movies").join(&folder), &mut c);
        write(
            &medias.join("Movies").join(&folder).join(&file),
            &folder,
            3000,
            &mut c,
        );
        write(
            &medias.join("Movies").join(&folder).join("poster.jpg"),
            &format!("mp{i}"),
            64,
            &mut c,
        );
        if i < 20 {
            arr_movie(&folder, &file, 3000);
        }
    }
    mkdir(&completed.join("stray"), &mut c);
    for i in 0..s.junk {
        write(
            &completed.join("stray").join(format!("junk-{i:05}.txt")),
            &format!("junk{i}"),
            32,
            &mut c,
        );
    }
    mkdir(&completed.join("orphans"), &mut c);
    for i in 0..s.orphans {
        write(
            &completed.join("orphans").join(format!("Orphan.{i:04}.mkv")),
            &format!("orphan{i}"),
            2048,
            &mut c,
        );
    }
    for i in 0..s.incomplete_torrents {
        let name = format!("Partial.{i:04}.2021.mkv");
        write(&incomplete.join(&name), &name, 1500, &mut c);
        torrents.push(
            FakeTorrent::new(&hex40(100_000 + i), &name, "stalledDL", 0.5)
                .paths("/data/incomplete", &format!("/data/incomplete/{name}"))
                .added_on(1_700_000_000)
                .file(FakeFile::new(&name, 3000)),
        );
    }
    for root in [&completed, &incomplete, &medias] {
        age_tree(root, 30);
    }
    let mut arr = removarr_testkit::fake_arr::radarr_basic();
    arr.movies = movies;
    arr.movie_files = movie_files;
    arr.queue.clear();
    arr.history.clear();
    arr.manual_import.clear();
    Synth {
        completed,
        incomplete,
        medias,
        counts: c,
        torrents,
        arr,
        gen_time: t0.elapsed(),
    }
}

/// Sets the mtime of every regular file under `p` to `days` ago (files are not "recent").
pub fn age_tree(p: &Path, days: u64) {
    for e in fs::read_dir(p).unwrap().filter_map(Result::ok) {
        let m = e.metadata().unwrap();
        if m.is_dir() {
            age_tree(&e.path(), days);
        } else {
            removarr_integration_tests::age_hours(&e.path(), days * 24);
        }
    }
}

/// Bytes of every regular file under `p` (apparent size).
#[must_use]
pub fn dir_size(p: &Path) -> u64 {
    fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| {
                    let m = e.metadata().unwrap();
                    if m.is_dir() {
                        dir_size(&e.path())
                    } else {
                        m.len()
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// `PRAGMA wal_checkpoint(TRUNCATE)` through the `sqlite3` CLI when it is installed (so the
/// database file alone carries the committed data). Returns false when the CLI is missing.
#[must_use]
pub fn checkpoint(data_dir: &Path) -> bool {
    Command::new("sqlite3")
        .arg(data_dir.join("removarr.db"))
        .arg("PRAGMA busy_timeout=5000; PRAGMA wal_checkpoint(TRUNCATE);")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// One scalar from the database through the `sqlite3` CLI (read-only), if installed.
#[must_use]
pub fn sqlite_scalar(data_dir: &Path, sql: &str) -> Option<String> {
    let o = Command::new("sqlite3")
        .arg("-readonly")
        .arg(data_dir.join("removarr.db"))
        .arg(sql)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Sum of the sizes of the database file and its WAL.
#[must_use]
pub fn db_bytes(data_dir: &Path) -> u64 {
    fs::read_dir(data_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("removarr.db"))
                .map(|e| e.metadata().unwrap().len())
                .sum()
        })
        .unwrap_or(0)
}

/// `(path, dev, ino, size, mtime_ns, nlink)` of every entry: B11 proves nothing changed.
#[must_use]
pub fn tree_signature(root: &Path) -> Vec<(String, u64, u64, u64, i64, u64)> {
    fn rec(root: &Path, dir: &Path, out: &mut Vec<(String, u64, u64, u64, i64, u64)>) {
        let mut ents: Vec<_> = fs::read_dir(dir).unwrap().filter_map(Result::ok).collect();
        ents.sort_by_key(std::fs::DirEntry::file_name);
        for e in ents {
            let p = e.path();
            let m = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let (size, mtime) = if m.is_dir() {
                (0, 0)
            } else {
                (m.len(), m.mtime() * 1_000_000_000 + m.mtime_nsec())
            };
            out.push((rel, m.dev(), m.ino(), size, mtime, m.nlink()));
            if m.is_dir() {
                rec(root, &p, out);
            }
        }
    }
    let mut v = Vec::new();
    rec(root, root, &mut v);
    v
}

/// A free loopback port (bound and released immediately).
#[must_use]
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[must_use]
pub fn release_binary() -> PathBuf {
    // `PERF_BIN` points at another build of the binary (for example one with symbols).
    if let Ok(b) = std::env::var("PERF_BIN") {
        return PathBuf::from(b);
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/removarr");
    assert!(
        p.exists(),
        "build it first: cargo build --release -p removarr ({})",
        p.display()
    );
    p
}

/// Resident set size of a pid in KiB (`ps`, works on macOS and Linux).
#[must_use]
pub fn rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// CPU seconds (user + system) of a pid, from `ps -o time=` (`[H:]MM:SS.ss`).
#[must_use]
pub fn cpu_seconds(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut secs = 0.0;
    for part in t.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(secs)
}

/// What the I/O accounting reports for a finished process.
#[derive(Debug, Default, Clone)]
pub struct Totals {
    pub max_rss_kib: u64,
    pub block_in: u64,
    pub block_out: u64,
}

/// The release binary running as a child process with an empty environment but the given
/// variables, sampled for RSS every 100 ms.
pub struct Proc {
    child: Child,
    pub pid: u32,
    pub port: u16,
    pub base: String,
    pub stdout_path: PathBuf,
    stderr_path: PathBuf,
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    sampler: Option<JoinHandle<()>>,
    pub started: Instant,
}

impl std::fmt::Debug for Proc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proc").field("pid", &self.pid).finish()
    }
}

impl Proc {
    /// `env` is the complete environment (nothing inherited except a minimal `PATH`).
    #[must_use]
    pub fn spawn(work: &Path, env: &[(String, String)], port: u16) -> Self {
        let bin = release_binary();
        let stdout_path = work.join("stdout.log");
        let stderr_path = work.join("stderr.log");
        let mut cmd = if cfg!(target_os = "macos") {
            // `time -l` reports block I/O and the maximum RSS of the child at exit.
            let mut c = Command::new("/usr/bin/time");
            c.arg("-l").arg(&bin).arg("serve");
            c
        } else {
            let mut c = Command::new(&bin);
            c.arg("serve");
            c
        };
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", work)
            .env("REMOVARR_BIND", format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout_path).unwrap())
            .stderr(fs::File::create(&stderr_path).unwrap());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let started = Instant::now();
        let child = cmd.spawn().unwrap();
        let lead = child.id();
        // Under `time` the application is the child of the wrapper.
        let pid = if cfg!(target_os = "macos") {
            let mut found = None;
            for _ in 0..100 {
                let o = Command::new("pgrep")
                    .args(["-P", &lead.to_string()])
                    .output()
                    .unwrap();
                if let Some(p) = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .and_then(|l| l.trim().parse::<u32>().ok())
                {
                    found = Some(p);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            found.unwrap_or_else(|| {
                panic!(
                    "application did not start\nstdout: {}\nstderr: {}",
                    fs::read_to_string(&stdout_path).unwrap_or_default(),
                    fs::read_to_string(&stderr_path).unwrap_or_default()
                )
            })
        } else {
            lead
        };
        let peak = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p2, s2) = (peak.clone(), stop.clone());
        let sampler = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                if let Some(r) = rss_kib(pid) {
                    p2.fetch_max(r, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            child,
            pid,
            port,
            base: format!("http://127.0.0.1:{port}"),
            stdout_path,
            stderr_path,
            peak,
            stop,
            sampler: Some(sampler),
            started,
        }
    }

    /// Peak sampled RSS since the last [`Proc::reset_peak`] (KiB).
    #[must_use]
    pub fn peak_kib(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn reset_peak(&self) {
        self.peak.store(0, Ordering::Relaxed);
    }

    #[must_use]
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Number of non-empty stdout lines so far.
    #[must_use]
    pub fn stdout_lines(&self) -> usize {
        fs::read_to_string(&self.stdout_path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    /// SIGKILL: a crash, no shutdown path.
    pub fn kill9(&mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", &self.pid.to_string()])
            .status();
        self.finish();
    }

    /// SIGTERM, wait for the exit and return the I/O totals.
    pub fn terminate(&mut self) -> Totals {
        let _ = Command::new("kill")
            .args(["-TERM", &self.pid.to_string()])
            .status();
        self.finish()
    }

    fn finish(&mut self) -> Totals {
        let t0 = Instant::now();
        while self.alive() && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.alive() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.sampler.take() {
            let _ = h.join();
        }
        let err = fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let grab = |label: &str| -> u64 {
            err.lines()
                .find(|l| l.contains(label))
                .and_then(|l| l.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        };
        Totals {
            max_rss_kib: grab("maximum resident set size") / 1024,
            block_in: grab("block input operations"),
            block_out: grab("block output operations"),
        }
    }

    /// Waits until `/readyz` answers 200; returns the elapsed time since spawn.
    pub async fn wait_ready(&self, limit: Duration) -> Option<Duration> {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        while self.started.elapsed() < limit {
            if let Ok(r) = c.get(format!("{}/readyz", self.base)).send().await
                && r.status().is_success()
            {
                return Some(self.started.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Under `time -l` the application is a grandchild: kill it explicitly.
        let _ = Command::new("kill")
            .args(["-KILL", &self.pid.to_string()])
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The environment of the application over a synthetic tree and loopback fakes.
#[must_use]
pub fn app_env(
    syn: &Synth,
    data_dir: &Path,
    qbit_url: &str,
    radarr_url: &str,
    password_hash: &str,
    extra: &[(&str, &str)],
) -> Vec<(String, String)> {
    let s = |p: &Path| p.to_str().unwrap().to_string();
    let md = fs::metadata(&syn.completed).unwrap();
    let mut v = vec![
        ("REMOVARR_COMPLETED_ROOT".into(), s(&syn.completed)),
        ("REMOVARR_INCOMPLETE_ROOT".into(), s(&syn.incomplete)),
        ("REMOVARR_MEDIA_ROOTS".into(), s(&syn.medias)),
        ("REMOVARR_DATA_DIR".into(), s(data_dir)),
        ("REMOVARR_PASSWORD_HASH".into(), password_hash.into()),
        (
            "REMOVARR_SESSION_KEY".into(),
            "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef".into(),
        ),
        ("REMOVARR_COOKIE_SECURE".into(), "false".into()),
        ("REMOVARR_DRY_RUN".into(), "true".into()),
        ("REMOVARR_VIDEO_EXTENSIONS".into(), ".mkv".into()),
        ("REMOVARR_SCAN_INTERVAL_MINUTES".into(), "0".into()),
        ("REMOVARR_QBIT_REFRESH_MINUTES".into(), "0".into()),
        (
            "REMOVARR_EXPECTED_OWNER".into(),
            format!("{}:{}", md.uid(), md.gid()),
        ),
        ("REMOVARR_QBIT_URL".into(), qbit_url.into()),
        ("REMOVARR_QBIT_USERNAME".into(), "admin".into()),
        ("REMOVARR_QBIT_PASSWORD".into(), FAKE_PASSWORD.into()),
        (
            "REMOVARR_QBIT_PATH_MAP".into(),
            format!(
                "/data/completed={},/data/incomplete={}",
                s(&syn.completed),
                s(&syn.incomplete)
            ),
        ),
        ("REMOVARR_RADARR_URL".into(), radarr_url.into()),
        ("REMOVARR_RADARR_API_KEY".into(), FAKE_API_KEY.into()),
        (
            "REMOVARR_RADARR_PATH_MAP".into(),
            format!("/media={}", s(&syn.medias)),
        ),
    ];
    for (k, val) in extra {
        v.retain(|(x, _)| x != k);
        v.push(((*k).into(), (*val).into()));
    }
    v
}

/// Quantile of a sorted copy (`q` in 0..=1).
#[must_use]
pub fn quantile(samples: &[Duration], q: f64) -> Duration {
    let mut v = samples.to_vec();
    v.sort();
    v[(((v.len() - 1) as f64) * q).round() as usize]
}

#[must_use]
pub fn json_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Appends a line to a results file (the benchmark tests print the same lines).
pub fn record(file: &Path, line: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}
