//! Loopback fake Sonarr/Radarr v3 server (agent 09). Binds `127.0.0.1:0` only, serves the
//! subset of the v3 API used by `removarr-arr` from synthetic JSON, records every request and
//! can inject failures.
//!
//! Safety assertions built into the fake:
//! - the API key must arrive in the `X-Api-Key` header (a key in the query is a violation);
//! - a `ManualImport` command whose import mode is not `copy` is refused with HTTP 400 and
//!   recorded as a violation (architecture 6.8(d));
//! - a queue removal without `removeFromClient=false&blocklist=false` is refused and recorded.
//!
//! V10 behaviors (architecture 15.3): a "move" switch ([`ImportEffect::Move`]) that makes the
//! source disappear on import, an "upgrade deletes the old file" behavior (old files go to the
//! recycle bin when one is configured, else they are removed), and media-management presets.
//! Filesystem effects happen only when an [`FsBridge`] maps service paths to a local temp dir.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// The API key the fake accepts (a dummy value, not a secret).
pub const FAKE_API_KEY: &str = "fake-arr-api-key-0123456789abcdef";

/// Synthetic qBittorrent download ids (upper case, as ARR stores them).
pub const HASH_SEASON: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
pub const HASH_AMBIGUOUS: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
pub const HASH_UNKNOWN: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
pub const HASH_IMPORTED: &str = "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
pub const HASH_MOVIE: &str = "EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";

/// A tracker passkey planted in fields the connector must never read (download URLs,
/// history `data`). Tests assert it never surfaces.
pub const PLANTED_PASSKEY: &str = "PLANTEDPASSKEY0123456789";

/// One request as seen by the server. The key value is never stored, only whether it matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub body: Option<Value>,
    pub key_ok: bool,
}

/// Failure injection. Keys are the first path segment after `/api/v3/` (`series`, `queue`,
/// `release`, `command`, `system`, `config`, ...) or `*` for every request.
#[derive(Debug, Clone, Default)]
pub struct Fault {
    pub status: Option<u16>,
    pub delay: Option<Duration>,
    pub raw_body: Option<String>,
    /// Respond `302 Found` with this `Location`.
    pub redirect_to: Option<String>,
}

/// What a manual import does to the source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportEffect {
    /// Honors the copy request: hardlink when `copyUsingHardlinks` is on, else a byte copy.
    #[default]
    Copy,
    /// Simulates ARR moving the file despite the copy request: the source disappears.
    Move,
}

/// Maps service paths under `service_prefix` to `local_dir` for filesystem effects.
#[derive(Debug, Clone)]
pub struct FsBridge {
    pub service_prefix: String,
    pub local_dir: PathBuf,
}

impl FsBridge {
    fn local(&self, service_path: &str) -> Option<PathBuf> {
        let rest = service_path.strip_prefix(&self.service_prefix)?;
        Some(self.local_dir.join(rest.trim_start_matches('/')))
    }
}

/// The synthetic ARR state, in v3 wire shape.
#[derive(Debug, Clone, Default)]
pub struct ArrData {
    pub app_name: String,
    pub version: String,
    pub series: Vec<Value>,
    pub episodes: Vec<Value>,
    pub episode_files: Vec<Value>,
    pub movies: Vec<Value>,
    /// Movie files carry a `movieId`; `GET /movie` embeds the first one as `movieFile`.
    pub movie_files: Vec<Value>,
    pub quality_profiles: Vec<Value>,
    pub custom_formats: Vec<Value>,
    pub media_management: Value,
    pub queue: Vec<Value>,
    pub history: Vec<Value>,
    /// Manual-import preview rows per service-side folder.
    pub manual_import: BTreeMap<String, Vec<Value>>,
    /// Results of every interactive search.
    pub releases: Vec<Value>,
}

#[derive(Debug)]
struct Inner {
    data: ArrData,
    faults: BTreeMap<String, Fault>,
    recorded: Vec<Recorded>,
    violations: Vec<String>,
    commands: BTreeMap<i64, Value>,
    grabs: Vec<Value>,
    next_id: i64,
    command_status: String,
    import_effect: ImportEffect,
    fs: Option<FsBridge>,
    destinations: BTreeMap<String, String>,
}

type Shared = Arc<Mutex<Inner>>;

fn lock(s: &Shared) -> MutexGuard<'_, Inner> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug)]
pub struct FakeArr {
    addr: SocketAddr,
    inner: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeArr {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeArr {
    /// Starts a server with `data`.
    ///
    /// # Panics
    /// Panics when no loopback port can be bound (test environment failure).
    #[allow(clippy::expect_used)]
    pub async fn start(data: ArrData) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            data,
            faults: BTreeMap::new(),
            recorded: Vec::new(),
            violations: Vec::new(),
            commands: BTreeMap::new(),
            grabs: Vec::new(),
            next_id: 5000,
            command_status: "completed".into(),
            import_effect: ImportEffect::Copy,
            fs: None,
            destinations: BTreeMap::new(),
        }));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, inner, task }
    }

    /// Sonarr with the V10 preset.
    pub async fn sonarr() -> Self {
        Self::start(sonarr_v10()).await
    }

    /// Radarr with the basic movie preset.
    pub async fn radarr() -> Self {
        Self::start(radarr_basic()).await
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Edits the synthetic state in place.
    pub fn with_data<R>(&self, f: impl FnOnce(&mut ArrData) -> R) -> R {
        f(&mut lock(&self.inner).data)
    }

    #[must_use]
    pub fn data(&self) -> ArrData {
        lock(&self.inner).data.clone()
    }

    pub fn set_fault(&self, key: &str, fault: Fault) {
        lock(&self.inner).faults.insert(key.to_string(), fault);
    }

    pub fn clear_faults(&self) {
        lock(&self.inner).faults.clear();
    }

    /// Status reported by `GET /command/{id}` (`completed` by default).
    pub fn set_command_status(&self, status: &str) {
        lock(&self.inner).command_status = status.to_string();
    }

    pub fn set_import_effect(&self, e: ImportEffect) {
        lock(&self.inner).import_effect = e;
    }

    pub fn set_fs_bridge(&self, bridge: FsBridge) {
        lock(&self.inner).fs = Some(bridge);
    }

    /// Library path ARR gives to an imported source file (service paths).
    pub fn set_import_destination(&self, source: &str, dest: &str) {
        lock(&self.inner)
            .destinations
            .insert(source.to_string(), dest.to_string());
    }

    #[must_use]
    pub fn recorded(&self) -> Vec<Recorded> {
        lock(&self.inner).recorded.clone()
    }

    #[must_use]
    pub fn violations(&self) -> Vec<String> {
        lock(&self.inner).violations.clone()
    }

    /// Bodies of accepted grabs (`POST /api/v3/release`).
    #[must_use]
    pub fn grabs(&self) -> Vec<Value> {
        lock(&self.inner).grabs.clone()
    }

    /// Accepted commands by id (bodies as received).
    #[must_use]
    pub fn commands(&self) -> BTreeMap<i64, Value> {
        lock(&self.inner).commands.clone()
    }

    #[must_use]
    pub fn count(&self, method: &str, path: &str) -> usize {
        lock(&self.inner)
            .recorded
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .count()
    }

    /// Requests other than GET (every mutating request, accepted or not).
    #[must_use]
    pub fn mutating_count(&self) -> usize {
        lock(&self.inner)
            .recorded
            .iter()
            .filter(|r| r.method != "GET")
            .count()
    }
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn ok(v: &Value) -> Response {
    json_response(StatusCode::OK, v)
}

fn status(code: StatusCode, msg: &str) -> Response {
    json_response(code, &json!({ "message": msg }))
}

fn id_of(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn q_i64(q: &BTreeMap<String, String>, k: &str) -> Option<i64> {
    q.get(k).and_then(|v| v.parse().ok())
}

fn page(records: &[Value], q: &BTreeMap<String, String>) -> Value {
    let page = q_i64(q, "page").unwrap_or(1).max(1) as usize;
    let size = q_i64(q, "pageSize").unwrap_or(10).max(1) as usize;
    let slice: Vec<Value> = records
        .iter()
        .skip((page - 1) * size)
        .take(size)
        .cloned()
        .collect();
    json!({ "page": page, "pageSize": size, "totalRecords": records.len(), "records": slice })
}

async fn handle(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == FAKE_API_KEY);
    let body_json: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };
    let rel = path.strip_prefix("/api/v3/").unwrap_or("").to_string();
    let segment = rel.split('/').next().unwrap_or("").to_string();
    let fault = {
        let mut g = lock(&s);
        g.recorded.push(Recorded {
            method: method.to_string(),
            path: path.clone(),
            query: q.clone(),
            body: body_json.clone(),
            key_ok,
        });
        if q.keys().any(|k| k.eq_ignore_ascii_case("apikey")) {
            g.violations
                .push(format!("api key sent in the query of {path}"));
        }
        g.faults
            .get(&segment)
            .or_else(|| g.faults.get("*"))
            .cloned()
    };
    if let Some(f) = fault {
        if let Some(d) = f.delay {
            tokio::time::sleep(d).await;
        }
        if let Some(loc) = f.redirect_to {
            return (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response();
        }
        if let Some(code) = f.status {
            return status(
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                "injected failure",
            );
        }
        if let Some(raw) = f.raw_body {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                raw,
            )
                .into_response();
        }
    }
    if !key_ok {
        return status(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let mut g = lock(&s);
    route(&mut g, &method, &rel, &q, body_json)
}

fn route(
    g: &mut Inner,
    method: &Method,
    rel: &str,
    q: &BTreeMap<String, String>,
    body: Option<Value>,
) -> Response {
    let parts: Vec<&str> = rel.split('/').collect();
    let d = &g.data;
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["system", "status"]) => {
            ok(&json!({ "appName": d.app_name, "version": d.version }))
        }
        ("GET", ["series"]) => ok(&Value::Array(d.series.clone())),
        ("GET", ["series", id]) => find_by_id(&d.series, id),
        ("GET", ["episode"]) => {
            let sid = q_i64(q, "seriesId");
            let season = q_i64(q, "seasonNumber");
            let v: Vec<Value> = d
                .episodes
                .iter()
                .filter(|e| id_of(e, "seriesId") == sid)
                .filter(|e| season.is_none() || id_of(e, "seasonNumber") == season)
                .cloned()
                .collect();
            ok(&Value::Array(v))
        }
        ("GET", ["episodefile"]) => {
            let sid = q_i64(q, "seriesId");
            let v: Vec<Value> = d
                .episode_files
                .iter()
                .filter(|f| id_of(f, "seriesId") == sid)
                .cloned()
                .collect();
            ok(&Value::Array(v))
        }
        ("GET", ["movie"]) => {
            let v: Vec<Value> = d.movies.iter().map(|m| with_movie_file(d, m)).collect();
            ok(&Value::Array(v))
        }
        ("GET", ["movie", id]) => match d
            .movies
            .iter()
            .find(|m| id_of(m, "id").map(|x| x.to_string()).as_deref() == Some(*id))
        {
            Some(m) => ok(&with_movie_file(d, m)),
            None => status(StatusCode::NOT_FOUND, "NotFound"),
        },
        ("GET", ["moviefile"]) => {
            let mid = q_i64(q, "movieId");
            let v: Vec<Value> = d
                .movie_files
                .iter()
                .filter(|f| id_of(f, "movieId") == mid)
                .cloned()
                .collect();
            ok(&Value::Array(v))
        }
        ("GET", ["qualityprofile"]) => ok(&Value::Array(d.quality_profiles.clone())),
        ("GET", ["qualityprofile", id]) => find_by_id(&d.quality_profiles, id),
        ("GET", ["customformat"]) => ok(&Value::Array(d.custom_formats.clone())),
        ("GET", ["config", "mediamanagement"]) => ok(&d.media_management),
        ("GET", ["queue"]) => ok(&page(&d.queue, q)),
        ("GET", ["history"]) => {
            let want = q.get("downloadId");
            let v: Vec<Value> = d
                .history
                .iter()
                .filter(|h| {
                    want.is_none()
                        || h.get("downloadId").and_then(Value::as_str) == want.map(String::as_str)
                })
                .cloned()
                .collect();
            ok(&page(&v, q))
        }
        ("GET", ["manualimport"]) => {
            let folder = q.get("folder").cloned().unwrap_or_default();
            ok(&Value::Array(
                d.manual_import.get(&folder).cloned().unwrap_or_default(),
            ))
        }
        ("GET", ["release"]) => ok(&Value::Array(d.releases.clone())),
        ("GET", ["command", id]) => match id.parse::<i64>().ok().and_then(|i| g.commands.get(&i)) {
            Some(c) => ok(&json!({
                "id": id.parse::<i64>().unwrap_or(0),
                "name": c.get("name").cloned().unwrap_or(Value::Null),
                "status": g.command_status,
                "result": if g.command_status == "completed" { "successful" } else { "unknown" },
            })),
            None => status(StatusCode::NOT_FOUND, "NotFound"),
        },
        ("POST", ["release"]) => grab(g, body),
        ("POST", ["command"]) => command(g, body),
        ("PUT", ["episode", "monitor"]) => set_monitored(g, body, "episodeIds", Target::Episodes),
        ("PUT", ["series", "editor"]) => set_monitored(g, body, "seriesIds", Target::Series),
        ("PUT", ["movie", "editor"]) => set_monitored(g, body, "movieIds", Target::Movies),
        ("DELETE", ["queue", id]) => {
            if q.get("removeFromClient").map(String::as_str) != Some("false")
                || q.get("blocklist").map(String::as_str) != Some("false")
            {
                g.violations
                    .push("queue removal without removeFromClient=false&blocklist=false".into());
                return status(StatusCode::BAD_REQUEST, "unsafe queue removal refused");
            }
            let before = g.data.queue.len();
            g.data
                .queue
                .retain(|i| id_of(i, "id").map(|x| x.to_string()).as_deref() != Some(*id));
            if g.data.queue.len() == before {
                status(StatusCode::NOT_FOUND, "NotFound")
            } else {
                ok(&json!({}))
            }
        }
        _ => status(StatusCode::NOT_FOUND, "NotFound"),
    }
}

fn find_by_id(list: &[Value], id: &str) -> Response {
    match list
        .iter()
        .find(|v| id_of(v, "id").map(|x| x.to_string()).as_deref() == Some(id))
    {
        Some(v) => ok(v),
        None => status(StatusCode::NOT_FOUND, "NotFound"),
    }
}

fn with_movie_file(d: &ArrData, m: &Value) -> Value {
    let mut m = m.clone();
    let mid = id_of(&m, "id");
    let file = d.movie_files.iter().find(|f| id_of(f, "movieId") == mid);
    if let Some(obj) = m.as_object_mut() {
        obj.insert("hasFile".into(), json!(file.is_some()));
        obj.insert("movieFile".into(), file.cloned().unwrap_or(Value::Null));
    }
    m
}

fn grab(g: &mut Inner, body: Option<Value>) -> Response {
    let Some(b) = body else {
        return status(StatusCode::BAD_REQUEST, "missing body");
    };
    let guid = b.get("guid").and_then(Value::as_str).unwrap_or_default();
    let Some(rel) = g
        .data
        .releases
        .iter()
        .find(|r| r.get("guid").and_then(Value::as_str) == Some(guid))
        .cloned()
    else {
        return status(
            StatusCode::NOT_FOUND,
            "Couldn't find requested release in cache",
        );
    };
    g.grabs.push(b);
    g.next_id += 1;
    let id = g.next_id;
    g.data.queue.push(json!({
        "id": id,
        "title": rel.get("title").cloned().unwrap_or(Value::Null),
        "trackedDownloadStatus": "ok",
        "trackedDownloadState": "downloading",
        "statusMessages": [],
    }));
    ok(&rel)
}

enum Target {
    Episodes,
    Series,
    Movies,
}

fn set_monitored(g: &mut Inner, body: Option<Value>, ids_key: &str, t: Target) -> Response {
    let Some(b) = body else {
        return status(StatusCode::BAD_REQUEST, "missing body");
    };
    let monitored = b.get("monitored").and_then(Value::as_bool);
    let ids: Vec<i64> = b
        .get(ids_key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let (Some(monitored), false) = (monitored, ids.is_empty()) else {
        return status(StatusCode::BAD_REQUEST, "invalid monitor body");
    };
    let list = match t {
        Target::Episodes => &mut g.data.episodes,
        Target::Series => &mut g.data.series,
        Target::Movies => &mut g.data.movies,
    };
    for item in list.iter_mut() {
        if id_of(item, "id").is_some_and(|i| ids.contains(&i))
            && let Some(o) = item.as_object_mut()
        {
            o.insert("monitored".into(), json!(monitored));
        }
    }
    ok(&json!({}))
}

fn command(g: &mut Inner, body: Option<Value>) -> Response {
    let Some(b) = body else {
        return status(StatusCode::BAD_REQUEST, "missing body");
    };
    let name = b
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name == "ManualImport" {
        let mode = b
            .get("importMode")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if mode != "copy" {
            g.violations
                .push(format!("manual import with import mode {mode:?} refused"));
            return status(StatusCode::BAD_REQUEST, "only copy imports are accepted");
        }
        apply_import(g, &b);
    }
    g.next_id += 1;
    let id = g.next_id;
    g.commands.insert(id, b);
    ok(&json!({ "id": id, "name": name, "status": "queued" }))
}

/// Applies a manual import to the state and, with an `FsBridge`, to the filesystem.
fn apply_import(g: &mut Inner, b: &Value) {
    let files = b
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let hardlinks = g
        .data
        .media_management
        .get("copyUsingHardlinks")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let recycle = g
        .data
        .media_management
        .get("recycleBin")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sonarr = g.data.app_name.eq_ignore_ascii_case("sonarr");
    for f in files {
        let src = f
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(dest) = g.destinations.get(&src).cloned() else {
            g.violations
                .push(format!("no import destination configured for {src}"));
            continue;
        };
        let preview = g
            .data
            .manual_import
            .values()
            .flatten()
            .find(|r| r.get("path").and_then(Value::as_str) == Some(src.as_str()))
            .cloned()
            .unwrap_or(Value::Null);
        g.next_id += 1;
        let new_id = g.next_id;
        let mut new_file = json!({
            "id": new_id,
            "path": dest,
            "size": preview.get("size").cloned().unwrap_or(json!(0)),
            "quality": preview.get("quality").cloned().unwrap_or(Value::Null),
            "customFormats": preview.get("customFormats").cloned().unwrap_or(json!([])),
            "customFormatScore": preview.get("customFormatScore").cloned().unwrap_or(json!(0)),
            "qualityCutoffNotMet": false,
            "languages": preview.get("languages").cloned().unwrap_or(json!([])),
        });
        let mut old_paths = Vec::new();
        if sonarr {
            let sid = id_of(&f, "seriesId");
            let eps: Vec<i64> = f
                .get("episodeIds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            let mut old_ids = Vec::new();
            for e in g.data.episodes.iter_mut() {
                if id_of(e, "id").is_some_and(|i| eps.contains(&i)) {
                    if let Some(old) = id_of(e, "episodeFileId").filter(|i| *i > 0) {
                        old_ids.push(old);
                    }
                    if let Some(o) = e.as_object_mut() {
                        o.insert("episodeFileId".into(), json!(new_id));
                        o.insert("hasFile".into(), json!(true));
                    }
                }
            }
            g.data.episode_files.retain(|ef| {
                let old = id_of(ef, "id").is_some_and(|i| old_ids.contains(&i));
                if old && let Some(p) = ef.get("path").and_then(Value::as_str) {
                    old_paths.push(p.to_string());
                }
                !old
            });
            if let (Some(o), Some(sid)) = (new_file.as_object_mut(), sid) {
                o.insert("seriesId".into(), json!(sid));
            }
            g.data.episode_files.push(new_file);
        } else {
            let mid = id_of(&f, "movieId");
            g.data.movie_files.retain(|mf| {
                let old = id_of(mf, "movieId") == mid;
                if old && let Some(p) = mf.get("path").and_then(Value::as_str) {
                    old_paths.push(p.to_string());
                }
                !old
            });
            if let (Some(o), Some(mid)) = (new_file.as_object_mut(), mid) {
                o.insert("movieId".into(), json!(mid));
            }
            g.data.movie_files.push(new_file);
        }
        if let Some(bridge) = g.fs.clone() {
            fs_effects(g, &bridge, &src, &dest, &old_paths, hardlinks, &recycle);
        }
    }
}

fn fs_effects(
    g: &mut Inner,
    bridge: &FsBridge,
    src: &str,
    dest: &str,
    old_paths: &[String],
    hardlinks: bool,
    recycle: &str,
) {
    let mut fail = |what: String| g.violations.push(what);
    // The upgrade removes the old file first (inside ARR's import, before any proof).
    for old in old_paths {
        let Some(old_local) = bridge.local(old) else {
            continue;
        };
        let res = match bridge.local(recycle).filter(|_| !recycle.is_empty()) {
            Some(bin) => std::fs::create_dir_all(&bin).and_then(|()| {
                let name = old_local.file_name().map(PathBuf::from).unwrap_or_default();
                std::fs::rename(&old_local, bin.join(name))
            }),
            None => std::fs::remove_file(&old_local),
        };
        if res.is_err() {
            fail(format!("fake could not retire old file {old}"));
        }
    }
    let (Some(s), Some(d)) = (bridge.local(src), bridge.local(dest)) else {
        fail(format!("fs bridge does not cover {src} or {dest}"));
        return;
    };
    if let Some(parent) = d.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        fail(format!("fake could not create the parent of {dest}"));
        return;
    }
    let effect = g.import_effect;
    let res = match effect {
        ImportEffect::Move => std::fs::rename(&s, &d),
        ImportEffect::Copy if hardlinks => std::fs::hard_link(&s, &d),
        ImportEffect::Copy => std::fs::copy(&s, &d).map(|_| ()),
    };
    if res.is_err() {
        g.violations
            .push(format!("fake import of {src} to {dest} failed"));
    }
}

// ---- Presets (synthetic data only) ---------------------------------------------------

fn quality(id: i64, name: &str) -> Value {
    json!({ "quality": { "id": id, "name": name, "source": "synthetic", "resolution": 1080 },
            "revision": { "version": 1, "real": 0, "isRepack": false } })
}

fn profile(id: i64, name: &str, scores: &[(i64, &str, i64)]) -> Value {
    let items: Vec<Value> = scores
        .iter()
        .map(|(fid, n, s)| json!({ "format": fid, "name": n, "score": s }))
        .collect();
    json!({
        "id": id, "name": name, "upgradeAllowed": true, "cutoff": 1001,
        "items": [
            { "quality": { "id": 4, "name": "HDTV-720p" }, "items": [], "allowed": true },
            { "id": 1001, "name": "Group-HD", "allowed": true, "items": [
                { "quality": { "id": 3, "name": "WEBDL-1080p" }, "items": [], "allowed": true },
                { "quality": { "id": 7, "name": "Bluray-1080p" }, "items": [], "allowed": true }
            ]}
        ],
        "minFormatScore": 0, "cutoffFormatScore": 100, "formatItems": items
    })
}

/// Media-management presets for V10 (recycle bin set or unset, hardlinks on or off).
#[must_use]
pub fn media_management(recycle_bin: Option<&str>, cleanup_days: i64, hardlinks: bool) -> Value {
    json!({
        "recycleBin": recycle_bin.unwrap_or(""),
        "recycleBinCleanupDays": cleanup_days,
        "copyUsingHardlinks": hardlinks,
        "importExtraFiles": false,
        "deleteEmptyFolders": false
    })
}

/// Service-side folder of the V10 season pack in the download client.
pub const V10_FOLDER: &str = "/downloads/Synthetic.Show.S01.1080p.BluRay-SYN";

/// Sonarr V10: series 1 (anime numbering) with S01E01 (file, absolute 1), S01E02 (no file),
/// S01E03 (file, absolute 3, scene S02E01 / absolute 4), S01E04 (not aired), special S00E01
/// (file, unmonitored); profile with Custom Format scores; a season-pack preview where E03 is
/// rejected as not an upgrade; queue items covering merge, ambiguity and an unknown item;
/// history with a planted passkey in `data`; two releases (approved, rejected).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn sonarr_v10() -> ArrData {
    let ep = |id: i64, s: i64, e: i64, abs: Option<i64>, file: i64, mon: bool, air: &str| {
        json!({ "id": id, "seriesId": 1, "seasonNumber": s, "episodeNumber": e,
                "absoluteEpisodeNumber": abs, "hasFile": file > 0, "episodeFileId": file,
                "monitored": mon, "airDateUtc": air, "title": format!("Episode {e}") })
    };
    let mut e3 = ep(103, 1, 3, Some(3), 1003, true, "2020-01-15T00:00:00Z");
    if let Some(o) = e3.as_object_mut() {
        o.insert("sceneAbsoluteEpisodeNumber".into(), json!(4));
        o.insert("sceneSeasonNumber".into(), json!(2));
        o.insert("sceneEpisodeNumber".into(), json!(1));
    }
    let file = |id: i64, name: &str, q: Value, cfs: &[&str], score: i64, cutoff_not_met: bool| {
        let cf: Vec<Value> = cfs
            .iter()
            .enumerate()
            .map(|(i, n)| json!({ "id": i + 1, "name": n }))
            .collect();
        json!({ "id": id, "seriesId": 1, "seasonNumber": 1,
                "path": format!("/tv/Synthetic Show/Season 01/{name}"),
                "relativePath": format!("Season 01/{name}"), "size": 1_000_000 + id,
                "quality": q, "customFormats": cf, "customFormatScore": score,
                "qualityCutoffNotMet": cutoff_not_met, "languages": [{ "id": 1, "name": "English" }] })
    };
    let preview = |name: &str, eps: &[i64], rejections: Value| {
        let episodes: Vec<Value> = eps.iter().map(|i| json!({ "id": i })).collect();
        json!({ "path": format!("{V10_FOLDER}/{name}"), "relativePath": name,
                "folderName": "Synthetic.Show.S01.1080p.BluRay-SYN", "name": name, "size": 2_000_000,
                "series": { "id": 1, "title": "Synthetic Show" }, "seasonNumber": 1,
                "episodes": episodes, "quality": quality(7, "Bluray-1080p"),
                "languages": [{ "id": 1, "name": "English" }], "releaseGroup": "SYN",
                "downloadId": HASH_SEASON, "customFormats": [{ "id": 2, "name": "CF-Beta" }],
                "customFormatScore": 50, "rejections": rejections })
    };
    let q = |id: i64,
             hash: Option<&str>,
             series: Option<i64>,
             ep: Option<i64>,
             st: &str,
             state: &str,
             msg: &str| {
        json!({ "id": id, "downloadId": hash, "seriesId": series, "episodeId": ep,
                "title": "Synthetic.Show.S01.1080p.BluRay-SYN", "status": "completed",
                "trackedDownloadStatus": st, "trackedDownloadState": state,
                "statusMessages": [{ "title": "Synthetic.Show.S01E01.mkv", "messages": [msg] }],
                "protocol": "torrent", "downloadClient": "qBittorrent",
                "outputPath": V10_FOLDER })
    };
    let rel = |guid: &str, title: &str, approved: bool, rejections: Value, score: i64| {
        json!({ "guid": guid, "indexerId": 1, "indexer": "SynthIndexer", "title": title,
                "size": 2_000_000_000u64, "seeders": 12, "leechers": 3, "age": 4, "ageHours": 96.0,
                "protocol": "torrent", "quality": quality(7, "Bluray-1080p"),
                "customFormats": [{ "id": 2, "name": "CF-Beta" }], "customFormatScore": score,
                "approved": approved, "rejected": !approved, "rejections": rejections,
                "downloadAllowed": true, "temporarilyRejected": false,
                "downloadUrl": format!("https://tracker.invalid/dl/1?passkey={PLANTED_PASSKEY}"),
                "magnetUrl": format!("magnet:?xt=urn:btih:{HASH_SEASON}&tr=https://tracker.invalid/{PLANTED_PASSKEY}/announce"),
                "infoUrl": "https://tracker.invalid/t/1" })
    };
    ArrData {
        app_name: "Sonarr".into(),
        version: "4.0.0.0-synthetic".into(),
        series: vec![
            json!({ "id": 1, "title": "Synthetic Show", "year": 2020, "tvdbId": 100_001,
                    "imdbId": "tt9000001", "tvMazeId": 300_001, "seriesType": "anime",
                    "path": "/tv/Synthetic Show", "monitored": true, "qualityProfileId": 1 }),
            json!({ "id": 2, "title": "Other Show", "year": 2021, "tvdbId": 100_002,
                    "seriesType": "standard", "path": "/tv/Other Show", "monitored": true,
                    "qualityProfileId": 1 }),
        ],
        episodes: vec![
            ep(100, 0, 1, None, 1000, false, "2020-02-01T00:00:00Z"),
            ep(101, 1, 1, Some(1), 1001, true, "2020-01-01T00:00:00Z"),
            ep(102, 1, 2, Some(2), 0, true, "2020-01-08T00:00:00Z"),
            e3,
            ep(104, 1, 4, Some(5), 0, true, "2099-01-01T00:00:00Z"),
        ],
        episode_files: vec![
            json!({ "id": 1000, "seriesId": 1, "seasonNumber": 0,
                    "path": "/tv/Synthetic Show/Specials/Synthetic Show - S00E01.mkv",
                    "size": 500_000, "quality": quality(3, "WEBDL-1080p"), "customFormats": [],
                    "customFormatScore": 0, "qualityCutoffNotMet": true,
                    "languages": [{ "id": 1, "name": "English" }] }),
            file(
                1001,
                "Synthetic Show - S01E01.mkv",
                quality(3, "WEBDL-1080p"),
                &["CF-Alpha"],
                10,
                true,
            ),
            file(
                1003,
                "Synthetic Show - S01E03.mkv",
                quality(7, "Bluray-1080p"),
                &["CF-Beta"],
                50,
                false,
            ),
        ],
        quality_profiles: vec![profile(
            1,
            "Synthetic-HD",
            &[
                (1, "CF-Alpha", 10),
                (2, "CF-Beta", 50),
                (3, "CF-Penalty", -100),
            ],
        )],
        custom_formats: vec![
            json!({ "id": 1, "name": "CF-Alpha" }),
            json!({ "id": 2, "name": "CF-Beta" }),
            json!({ "id": 3, "name": "CF-Penalty" }),
        ],
        media_management: media_management(Some("/recycle"), 7, true),
        queue: vec![
            q(
                1,
                Some(HASH_SEASON),
                Some(1),
                Some(101),
                "warning",
                "importPending",
                "No files found are eligible for import",
            ),
            q(
                2,
                Some(HASH_SEASON),
                Some(1),
                Some(103),
                "warning",
                "importPending",
                "No files found are eligible for import",
            ),
            q(
                3,
                Some(HASH_AMBIGUOUS),
                Some(1),
                Some(102),
                "ok",
                "downloading",
                "ok",
            ),
            q(
                4,
                Some(HASH_AMBIGUOUS),
                Some(2),
                Some(201),
                "ok",
                "downloading",
                "ok",
            ),
            q(
                5,
                Some(HASH_UNKNOWN),
                None,
                None,
                "error",
                "importBlocked",
                "Unknown Series",
            ),
        ],
        history: vec![
            json!({ "id": 1, "eventType": "grabbed", "downloadId": HASH_SEASON, "seriesId": 1,
                    "episodeId": 101, "sourceTitle": "Synthetic.Show.S01.1080p.BluRay-SYN",
                    "date": "2024-05-01T10:20:30.1234567Z",
                    "data": { "downloadUrl": format!("https://tracker.invalid/dl?passkey={PLANTED_PASSKEY}"),
                              "indexer": "SynthIndexer" } }),
            json!({ "id": 2, "eventType": "downloadFolderImported", "downloadId": HASH_IMPORTED,
                    "seriesId": 1, "episodeId": 103, "sourceTitle": "Synthetic.Show.S01E03.WEB-SYN",
                    "date": "2024-04-01T00:00:00Z",
                    "data": { "importedPath": "/tv/Synthetic Show/Season 01/Synthetic Show - S01E03.mkv" } }),
        ],
        manual_import: BTreeMap::from([(
            V10_FOLDER.to_string(),
            vec![
                preview(
                    "Synthetic.Show.S01E01.1080p.BluRay-SYN.mkv",
                    &[101],
                    json!([]),
                ),
                preview(
                    "Synthetic.Show.S01E02.1080p.BluRay-SYN.mkv",
                    &[102],
                    json!([]),
                ),
                preview(
                    "Synthetic.Show.S01E03.1080p.BluRay-SYN.mkv",
                    &[103],
                    json!([{ "reason": "Not an upgrade for existing episode file(s)", "type": "permanent" }]),
                ),
            ],
        )]),
        releases: vec![
            rel(
                "synthetic-guid-approved",
                "Synthetic.Show.S01E01.1080p.BluRay-SYN",
                true,
                json!([]),
                50,
            ),
            rel(
                "https://tracker.invalid/details/2?passkey=PLANTEDPASSKEY0123456789",
                "Synthetic.Show.S01E01.720p.HDTV-BAD",
                false,
                json!([
                    "Not an upgrade for existing episode file(s)",
                    "Custom Formats CF-Penalty have score -100 below minimum 0"
                ]),
                -100,
            ),
        ],
        ..ArrData::default()
    }
}

/// Radarr: movie 7 with a file, movie 8 without; profile 2; queue and history for one
/// download; a preview folder; two releases (approved, rejected).
#[must_use]
pub fn radarr_basic() -> ArrData {
    let rel = |guid: &str,
               title: &str,
               approved: bool,
               rejections: Value,
               score: i64,
               size: u64| {
        json!({ "guid": guid, "indexerId": 2, "indexer": "SynthIndexer", "title": title,
                "size": size, "seeders": 40, "leechers": 1, "age": 10, "protocol": "torrent",
                "quality": quality(7, "Bluray-1080p"), "customFormats": [{ "id": 2, "name": "CF-Beta" }],
                "customFormatScore": score, "approved": approved, "rejections": rejections,
                "downloadAllowed": true,
                "downloadUrl": format!("https://tracker.invalid/dl/7?passkey={PLANTED_PASSKEY}") })
    };
    ArrData {
        app_name: "Radarr".into(),
        version: "5.0.0.0-synthetic".into(),
        movies: vec![
            json!({ "id": 7, "title": "Synthetic Movie", "year": 2021, "tmdbId": 200_001,
                    "imdbId": "tt0000007", "path": "/movies/Synthetic Movie (2021)",
                    "monitored": true, "qualityProfileId": 2 }),
            json!({ "id": 8, "title": "Missing Movie", "year": 2022, "tmdbId": 200_002,
                    "path": "/movies/Missing Movie (2022)", "monitored": false, "qualityProfileId": 2 }),
        ],
        movie_files: vec![json!({
            "id": 70, "movieId": 7,
            "path": "/movies/Synthetic Movie (2021)/Synthetic Movie (2021).mkv",
            "relativePath": "Synthetic Movie (2021).mkv", "size": 5_000_000,
            "quality": quality(3, "WEBDL-1080p"), "customFormats": [{ "id": 1, "name": "CF-Alpha" }],
            "customFormatScore": 10, "qualityCutoffNotMet": true,
            "languages": [{ "id": 1, "name": "English" }]
        })],
        quality_profiles: vec![profile(
            2,
            "Synthetic-Movies",
            &[(1, "CF-Alpha", 10), (2, "CF-Beta", 50)],
        )],
        custom_formats: vec![
            json!({ "id": 1, "name": "CF-Alpha" }),
            json!({ "id": 2, "name": "CF-Beta" }),
        ],
        media_management: media_management(None, 0, true),
        queue: vec![
            json!({ "id": 11, "downloadId": HASH_MOVIE, "movieId": 7, "status": "completed",
                            "trackedDownloadStatus": "warning", "trackedDownloadState": "importBlocked",
                            "statusMessages": [{ "title": "Synthetic.Movie.2021.mkv",
                                                 "messages": ["Not an upgrade for existing movie file"] }] }),
        ],
        history: vec![
            json!({ "id": 1, "eventType": "grabbed", "downloadId": HASH_MOVIE, "movieId": 7,
                              "sourceTitle": "Synthetic.Movie.2021.1080p.BluRay-SYN",
                              "date": "2024-05-02T00:00:00Z", "data": {} }),
        ],
        manual_import: BTreeMap::from([(
            "/downloads/Synthetic.Movie.2021.1080p.BluRay-SYN".to_string(),
            vec![
                json!({ "path": "/downloads/Synthetic.Movie.2021.1080p.BluRay-SYN/Synthetic.Movie.2021.mkv",
                         "size": 6_000_000, "movie": { "id": 7 }, "quality": quality(7, "Bluray-1080p"),
                         "languages": [{ "id": 1, "name": "English" }], "releaseGroup": "SYN",
                         "downloadId": HASH_MOVIE, "customFormats": [{ "id": 2, "name": "CF-Beta" }],
                         "customFormatScore": 50, "rejections": [] }),
            ],
        )]),
        releases: vec![
            rel(
                "movie-guid-approved",
                "Synthetic.Movie.2021.1080p.BluRay-SYN",
                true,
                json!([]),
                50,
                4_000_000,
            ),
            rel(
                "movie-guid-rejected",
                "Synthetic.Movie.2021.CAM-BAD",
                false,
                json!([{ "reason": "Quality CAM is not wanted in profile", "type": "permanent" }]),
                0,
                1_000_000,
            ),
        ],
        ..ArrData::default()
    }
}
