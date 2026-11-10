//! Loopback fake qBittorrent Web API (4.x and 5.x). Binds `127.0.0.1` on an ephemeral port only.
//!
//! The fake records every request, serves the documented endpoints used by `removarr-qbit`, and
//! can be scripted: login style, forced statuses, delays, oversize bodies, aborted connections
//! and recheck behavior. `parity_preset` builds the torrents of `docs/legacy-parity.md` 7.3.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub const FAKE_USERNAME: &str = "admin";
pub const FAKE_PASSWORD: &str = "s3cret-fake-pass";
pub const FAKE_SID: &str = "fakesid0123456789";

/// How `/api/v2/auth/login` answers valid credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    /// 200 with body `Ok.` (4.x and 5.x classic).
    Ok200,
    /// 200 with an empty body.
    Empty200,
    /// 204 No Content (documented for newer 5.x builds).
    NoContent204,
    /// 200 with body `Fails.` even for valid credentials.
    Fails200,
    /// 403 (IP banned).
    Forbidden403,
    /// 200 with an unrecognized body.
    Garbage200,
}

#[derive(Debug, Clone)]
pub struct FakeFile {
    pub name: String,
    pub size: u64,
    pub priority: i32,
    pub progress: f64,
}

impl FakeFile {
    #[must_use]
    pub fn new(name: &str, size: u64) -> Self {
        Self {
            name: name.to_string(),
            size,
            priority: 1,
            progress: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeTorrent {
    pub hash: String,
    pub name: String,
    pub state: String,
    pub progress: f64,
    pub save_path: String,
    pub download_path: String,
    pub content_path: String,
    pub category: String,
    pub added_on: i64,
    pub completion_on: i64,
    pub files: Vec<FakeFile>,
    check_left: u32,
    check_final: Option<(String, f64)>,
}

impl FakeTorrent {
    #[must_use]
    pub fn new(hash: &str, name: &str, state: &str, progress: f64) -> Self {
        Self {
            hash: hash.to_string(),
            name: name.to_string(),
            state: state.to_string(),
            progress,
            save_path: String::new(),
            download_path: String::new(),
            content_path: String::new(),
            category: String::new(),
            added_on: 1_700_000_000,
            completion_on: if progress >= 1.0 { 1_700_000_100 } else { -1 },
            files: Vec::new(),
            check_left: 0,
            check_final: None,
        }
    }

    #[must_use]
    pub fn paths(mut self, save: &str, content: &str) -> Self {
        self.save_path = save.to_string();
        self.content_path = content.to_string();
        self
    }

    #[must_use]
    pub fn download_path(mut self, p: &str) -> Self {
        self.download_path = p.to_string();
        self
    }

    #[must_use]
    pub fn file(mut self, f: FakeFile) -> Self {
        self.files.push(f);
        self
    }

    #[must_use]
    pub fn added_on(mut self, secs: i64) -> Self {
        self.added_on = secs;
        self
    }
}

/// What a recheck does: report `checking*` for `checking_polls` listings, then settle on `result`
/// (or the pre-recheck state when `None`).
#[derive(Debug, Clone)]
pub struct RecheckScript {
    pub checking_polls: u32,
    pub result: Option<(String, f64)>,
}

impl Default for RecheckScript {
    fn default() -> Self {
        Self {
            checking_polls: 1,
            result: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: String,
    pub has_sid: bool,
}

#[derive(Debug)]
struct FakeState {
    version: String,
    login: LoginMode,
    require_auth: bool,
    torrents: Vec<FakeTorrent>,
    requests: Vec<Recorded>,
    delay: Duration,
    oversize: Option<usize>,
    forced: BTreeMap<String, (u16, String)>,
    sid_valid: bool,
    aborted: Vec<String>,
    recheck: RecheckScript,
}

type Shared = Arc<Mutex<FakeState>>;

fn lock(s: &Shared) -> MutexGuard<'_, FakeState> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
pub struct FakeQbit {
    addr: SocketAddr,
    state: Shared,
    task: JoinHandle<()>,
}

impl Drop for FakeQbit {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeQbit {
    /// Starts the fake. `version` is the `app/version` text, for example `v5.0.4` or `v4.6.7`.
    ///
    /// # Panics
    /// Panics when loopback sockets are unavailable (test environment failure).
    #[allow(clippy::expect_used)]
    pub async fn spawn(version: &str) -> Self {
        let state: Shared = Arc::new(Mutex::new(FakeState {
            version: version.to_string(),
            login: LoginMode::Ok200,
            require_auth: true,
            torrents: Vec::new(),
            requests: Vec::new(),
            delay: Duration::ZERO,
            oversize: None,
            forced: BTreeMap::new(),
            sid_valid: false,
            aborted: Vec::new(),
            recheck: RecheckScript::default(),
        }));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let app = Router::new().fallback(handle).with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, state, task }
    }

    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_version(&self, v: &str) {
        lock(&self.state).version = v.to_string();
    }

    pub fn set_login(&self, mode: LoginMode) {
        lock(&self.state).login = mode;
    }

    pub fn set_require_auth(&self, on: bool) {
        lock(&self.state).require_auth = on;
    }

    pub fn set_torrents(&self, t: Vec<FakeTorrent>) {
        lock(&self.state).torrents = t;
    }

    pub fn add_torrent(&self, t: FakeTorrent) {
        lock(&self.state).torrents.push(t);
    }

    /// Delay applied to every non-login response.
    pub fn set_delay(&self, d: Duration) {
        lock(&self.state).delay = d;
    }

    /// Makes `torrents/info` answer a valid JSON array padded to about `bytes` bytes.
    pub fn set_oversize(&self, bytes: Option<usize>) {
        lock(&self.state).oversize = bytes;
    }

    /// Forces a status for a path (suffix match), for example `/torrents/info`.
    pub fn force_status(&self, path_suffix: &str, status: u16) {
        self.force_response(path_suffix, status, "forced");
    }

    /// Like `force_status` with a chosen body (for error-body redaction tests).
    pub fn force_response(&self, path_suffix: &str, status: u16, body: &str) {
        lock(&self.state)
            .forced
            .insert(path_suffix.to_string(), (status, body.to_string()));
    }

    /// Invalidates the current session cookie: the next authenticated call gets 403 until a
    /// new login happens.
    pub fn expire_sessions(&self) {
        lock(&self.state).sid_valid = false;
    }

    /// The response for this path suffix promises more bytes than it sends, then the connection
    /// closes (an interrupted transfer). The request is still recorded and applied.
    pub fn abort_response(&self, path_suffix: &str) {
        lock(&self.state).aborted.push(path_suffix.to_string());
    }

    pub fn set_recheck(&self, script: RecheckScript) {
        lock(&self.state).recheck = script;
    }

    #[must_use]
    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.state).requests.clone()
    }

    /// Recorded requests whose path ends with `suffix`.
    #[must_use]
    pub fn requests_to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.path.ends_with(suffix))
            .collect()
    }

    /// Number of recorded requests that are not reads (`POST`) excluding login.
    #[must_use]
    pub fn mutation_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == "POST" && !r.path.ends_with("/auth/login"))
            .count()
    }

    #[must_use]
    pub fn states(&self) -> BTreeMap<String, String> {
        lock(&self.state)
            .torrents
            .iter()
            .map(|t| (t.hash.clone(), t.state.clone()))
            .collect()
    }

    #[must_use]
    pub fn has_torrent(&self, hash: &str) -> bool {
        lock(&self.state).torrents.iter().any(|t| t.hash == hash)
    }
}

fn major(version: &str) -> u32 {
    version
        .trim_start_matches(['v', 'V'])
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn text(status: u16, body: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body.to_string(),
    )
        .into_response()
}

fn torrent_json(t: &FakeTorrent) -> Value {
    let size: u64 = t.files.iter().map(|f| f.size).sum();
    json!({
        "hash": t.hash,
        "infohash_v1": t.hash,
        "name": t.name,
        "state": t.state,
        "progress": t.progress,
        "save_path": t.save_path,
        "content_path": t.content_path,
        "category": t.category,
        "added_on": t.added_on,
        "completion_on": t.completion_on,
        "size": size,
        "total_size": size,
        "download_path": t.download_path,
    })
}

fn checking_state(t: &FakeTorrent) -> &'static str {
    if t.progress >= 1.0 {
        "checkingUP"
    } else {
        "checkingDL"
    }
}

fn stopped_state(t: &FakeTorrent, major: u32) -> String {
    match (t.progress >= 1.0, major >= 5) {
        (true, true) => "stoppedUP",
        (true, false) => "pausedUP",
        (false, true) => "stoppedDL",
        (false, false) => "pausedDL",
    }
    .to_string()
}

fn hashes_param(body: &str) -> Vec<String> {
    form_value(body, "hashes")
        .map(|h| h.split('|').map(str::to_string).collect())
        .unwrap_or_default()
}

async fn handle(State(shared): State<Shared>, req: Request) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let has_sid = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains(&format!("SID={FAKE_SID}")));
    let body_bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    let (delay, forced, aborted, oversize) = {
        let mut st = lock(&shared);
        st.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            body: body.clone(),
            has_sid,
        });
        let forced = st
            .forced
            .iter()
            .find(|(k, _)| path.ends_with(k.as_str()))
            .map(|(_, v)| v.clone());
        let aborted = st.aborted.iter().any(|k| path.ends_with(k.as_str()));
        (st.delay, forced, aborted, st.oversize)
    };

    if path == "/api/v2/auth/login" {
        return login(&shared, &body);
    }
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if let Some((code, body)) = forced {
        return text(code, &body);
    }
    let sid_ok = has_sid && lock(&shared).sid_valid;
    if lock(&shared).require_auth && !sid_ok {
        return text(403, "Forbidden");
    }

    let resp = route(&shared, &method, &path, &query, &body, oversize);
    if aborted {
        // Promise more than we send: the client sees a truncated transfer.
        let mut r = Response::new(Body::from("partial"));
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from_static("4096"));
        return r;
    }
    resp
}

fn login(shared: &Shared, body: &str) -> Response {
    let mut st = lock(shared);
    let user_ok = form_value(body, "username").as_deref() == Some(FAKE_USERNAME);
    let pass_ok = form_value(body, "password").as_deref() == Some(FAKE_PASSWORD);
    let cookie = format!("SID={FAKE_SID}; HttpOnly; path=/");
    let with_cookie = |mut r: Response| {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            r.headers_mut().insert(header::SET_COOKIE, v);
        }
        r
    };
    if !(user_ok && pass_ok) {
        return text(200, "Fails.");
    }
    if matches!(
        st.login,
        LoginMode::Ok200 | LoginMode::Empty200 | LoginMode::NoContent204
    ) {
        st.sid_valid = true;
    }
    match st.login {
        LoginMode::Ok200 => with_cookie(text(200, "Ok.")),
        LoginMode::Empty200 => with_cookie(text(200, "")),
        LoginMode::NoContent204 => with_cookie(StatusCode::NO_CONTENT.into_response()),
        LoginMode::Fails200 => text(200, "Fails."),
        LoginMode::Forbidden403 => text(403, "Your IP address has been banned"),
        LoginMode::Garbage200 => text(200, "<html>proxy login page</html>"),
    }
}

fn route(
    shared: &Shared,
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    oversize: Option<usize>,
) -> Response {
    let mut st = lock(shared);
    let version = st.version.clone();
    let maj = major(&version);
    match (method, path) {
        ("GET", "/api/v2/app/version") => text(200, &version),
        ("GET", "/api/v2/torrents/info") => {
            if let Some(n) = oversize {
                let pad = "x".repeat(n);
                return json_response(&json!([{ "hash": "0".repeat(40), "name": pad }]));
            }
            let wanted = hashes_param(query);
            let mut out = Vec::new();
            for t in &mut st.torrents {
                if !wanted.is_empty() && !wanted.contains(&t.hash) {
                    continue;
                }
                let mut view = t.clone();
                if t.check_left > 0 {
                    t.check_left -= 1;
                    view.state = checking_state(t).to_string();
                } else if let Some((s, p)) = t.check_final.take() {
                    t.state = s;
                    t.progress = p;
                    view = t.clone();
                }
                out.push(torrent_json(&view));
            }
            json_response(&Value::Array(out))
        }
        ("GET", "/api/v2/torrents/files") => {
            let hash = form_value(query, "hash").unwrap_or_default();
            match st.torrents.iter().find(|t| t.hash == hash) {
                Some(t) => {
                    let files: Vec<Value> = t
                        .files
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            json!({"index": i, "name": f.name, "size": f.size,
                                   "progress": f.progress, "priority": f.priority})
                        })
                        .collect();
                    json_response(&Value::Array(files))
                }
                None => text(404, ""),
            }
        }
        ("POST", "/api/v2/torrents/pause") if maj < 5 => {
            set_stopped(&mut st, body, maj);
            text(200, "")
        }
        ("POST", "/api/v2/torrents/stop") if maj >= 5 => {
            set_stopped(&mut st, body, maj);
            text(200, "")
        }
        ("POST", "/api/v2/torrents/resume") if maj < 5 => {
            set_started(&mut st, body);
            text(200, "")
        }
        ("POST", "/api/v2/torrents/start") if maj >= 5 => {
            set_started(&mut st, body);
            text(200, "")
        }
        ("POST", "/api/v2/torrents/recheck") => {
            let script = st.recheck.clone();
            let hashes = hashes_param(body);
            for t in &mut st.torrents {
                if hashes.contains(&t.hash) {
                    t.check_left = script.checking_polls;
                    t.check_final = Some(
                        script
                            .result
                            .clone()
                            .unwrap_or_else(|| (t.state.clone(), t.progress)),
                    );
                }
            }
            text(200, "")
        }
        ("POST", "/api/v2/torrents/delete") => {
            let hashes = hashes_param(body);
            st.torrents.retain(|t| !hashes.contains(&t.hash));
            text(200, "")
        }
        _ => text(404, "Not Found"),
    }
}

fn json_response(v: &Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

fn set_stopped(st: &mut FakeState, body: &str, maj: u32) {
    let hashes = hashes_param(body);
    for t in &mut st.torrents {
        if hashes.contains(&t.hash) && t.state != "missingFiles" {
            t.state = stopped_state(t, maj);
        }
    }
}

fn set_started(st: &mut FakeState, body: &str) {
    let hashes = hashes_param(body);
    for t in &mut st.torrents {
        if hashes.contains(&t.hash) && t.state != "missingFiles" {
            t.state = if t.progress >= 1.0 {
                "uploading"
            } else {
                "downloading"
            }
            .to_string();
        }
    }
}

/// Hash `H<n>` of the parity fixture: 40 repeated hex digits (`H1` = `1111...1`).
#[must_use]
pub fn parity_hash(n: char) -> String {
    n.to_string().repeat(40)
}

/// The torrents of `docs/legacy-parity.md` 7.3 (T_A .. T_O) with container paths `/data/...`.
/// `now_secs` anchors `added_on` (now - 10 days, T_O now - 30 days). `v4_names` replaces the 5.x
/// `stoppedUP` of T_D with `pausedUP` and keeps T_B2 `pausedUP` (V8). The state names of T_B2 and
/// T_D follow the table: T_B2 `pausedUP`, T_D `stoppedUP` unless `v4_names`.
#[must_use]
pub fn parity_preset(now_secs: i64, v4_names: bool) -> Vec<FakeTorrent> {
    const DAY: i64 = 86_400;
    let c = "/data/completed";
    let mk = |h: char, name: &str, state: &str, progress: f64, content: &str, files: &[&str]| {
        let mut t = FakeTorrent::new(&parity_hash(h), name, state, progress)
            .paths(c, &format!("{c}/{content}"))
            .added_on(now_secs - 10 * DAY);
        for f in files {
            t = t.file(FakeFile::new(f, 1_000));
        }
        t
    };
    let alpha = "Alpha.Movie.2020.1080p.WEB.x264-GRP";
    let bravo = "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv";
    let charlie = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
    let delta = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
    let foxtrot = "Foxtrot.Show.S01.1080p.WEB.x264-GRP";
    let juliet = "Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv";
    let kilo = "Kilo.Movie.2012.1080p.BluRay.x264-GRP";
    let november = "November.Movie.2010.1080p.WEB.x264-GRP.mkv";
    let tango = "Tango.Movie.2007.1080p.WEB.x264-GRP.mkv";
    let papa = "Papa.Movie.2008.1080p.WEB.x264-GRP.mkv";
    let a_mkv = format!("{alpha}/{alpha}.mkv");
    let a_nfo = format!("{alpha}/{alpha}.nfo");
    let c_main = format!("{charlie}/{charlie}.mkv");
    let c_sample = format!("{charlie}/Sample/{charlie}-sample.mkv");
    let c_nfo = format!("{charlie}/{charlie}.nfo");
    let f1 = format!("{foxtrot}/{foxtrot}E01.mkv");
    let f2 = format!("{foxtrot}/{foxtrot}E02.mkv");
    let k_main = format!("{kilo}/{kilo}.mkv");
    let k_sample = format!("{kilo}/{kilo}-sample.mkv");
    let d_state = if v4_names { "pausedUP" } else { "stoppedUP" };

    let mut out = vec![
        mk('1', alpha, "uploading", 1.0, alpha, &[&a_mkv, &a_nfo]),
        mk('2', bravo, "stalledUP", 1.0, bravo, &[bravo]),
        mk('3', bravo, "pausedUP", 1.0, bravo, &[bravo]),
        mk(
            '4',
            charlie,
            "uploading",
            1.0,
            charlie,
            &[&c_main, &c_sample, &c_nfo],
        ),
        mk('5', delta, d_state, 1.0, delta, &[delta]),
        mk('6', foxtrot, "queuedUP", 1.0, foxtrot, &[&f1, &f2]),
        mk('7', juliet, "checkingUP", 1.0, juliet, &[juliet]),
        mk('8', kilo, "forcedUP", 1.0, kilo, &[&k_main, &k_sample]),
        mk('9', november, "uploading", 1.0, november, &[november]),
        mk('a', tango, "uploading", 1.0, tango, &[tango]),
        mk('b', papa, "missingFiles", 1.0, papa, &[papa]),
    ];
    out.push(
        FakeTorrent::new(&parity_hash('c'), "Quebec.Movie.2006.mkv", "uploading", 1.0)
            .paths("/srv/other", "/srv/other/Quebec.Movie.2006.mkv")
            .added_on(now_secs - 10 * DAY)
            .file(FakeFile::new("Quebec.Movie.2006.mkv", 1_000)),
    );
    let oscar = "Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv";
    out.push(
        FakeTorrent::new(&parity_hash('d'), oscar, "stalledDL", 0.4)
            .paths("/data/incomplete", &format!("/data/incomplete/{oscar}"))
            .added_on(now_secs - 30 * DAY)
            .file(FakeFile::new(oscar, 1_000)),
    );
    out
}

/// Variant V4: torrent T_M whose `save_path` escapes the container root.
#[must_use]
pub fn parity_escape_torrent(now_secs: i64) -> FakeTorrent {
    FakeTorrent::new(&parity_hash('e'), "Mike.Movie.mkv", "uploading", 1.0)
        .paths(
            "/data/completed/../../etc",
            "/data/completed/../../etc/Mike.mkv",
        )
        .added_on(now_secs - 10 * 86_400)
        .file(FakeFile::new("Mike.mkv", 1_000))
}

/// Variant V9: T_R, a `stalledDL` torrent at 0.2 whose data lives under `download_path`.
#[must_use]
pub fn parity_download_path_torrent(now_secs: i64) -> FakeTorrent {
    let name = "Romeo.Movie";
    FakeTorrent::new(&parity_hash('f'), name, "stalledDL", 0.2)
        .paths("/data/completed", &format!("/data/incomplete/{name}"))
        .download_path("/data/incomplete")
        .added_on(now_secs - 30 * 86_400)
        .file(FakeFile::new(&format!("{name}/{name}.mkv"), 1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_and_major() {
        assert_eq!(percent_decode("a%2Bb+c"), "a+b c");
        assert_eq!(percent_decode("x%"), "x%");
        assert_eq!(major("v5.0.4"), 5);
        assert_eq!(major("4.6.7"), 4);
    }

    #[test]
    fn parity_preset_has_thirteen_torrents() {
        assert_eq!(parity_preset(1_700_000_000, false).len(), 13);
    }
}
