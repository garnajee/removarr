//! Loopback fake Jellyfin server (agent 10). Binds `127.0.0.1:0` only. Serves the subset of
//! the Jellyfin REST API used by `removarr-jellyfin`, records every request, and can inject
//! failures (status codes, delays, malformed or oversized bodies). Item JSON uses the real
//! Jellyfin wire shape (`Id`, `Name`, `Type`, `Path`, `SeriesName`, `ParentIndexNumber`,
//! `IndexNumber`, `IndexNumberEnd`, `ProviderIds`).

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

/// The token the fake accepts (a dummy value, not a secret).
pub const FAKE_TOKEN: &str = "fake-jellyfin-token-0000";

/// One request as seen by the server. The token value is never stored, only whether it matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub token_ok: bool,
}

/// Failure injection for one endpoint.
#[derive(Debug, Clone, Default)]
pub struct Fault {
    /// Respond with this status instead of the normal one.
    pub status: Option<u16>,
    /// Sleep before responding (to provoke client timeouts).
    pub delay: Option<Duration>,
    /// Respond 200 with this raw body instead of the normal one.
    pub raw_body: Option<String>,
}

#[derive(Debug)]
struct Inner {
    items: Vec<Value>,
    libraries: Vec<Value>,
    version: String,
    /// Successive answers of the library-scan task state. The last one repeats.
    task_states: VecDeque<bool>,
    task_present: bool,
    refresh_status: u16,
    faults: BTreeMap<&'static str, Fault>,
    recorded: Vec<Recorded>,
}

/// Fault keys: `health`, `info`, `libraries`, `items`, `tasks`, `refresh_library`, `refresh_item`.
#[derive(Debug)]
pub struct FakeJellyfin {
    addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeJellyfin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Shared = Arc<Mutex<Inner>>;

fn lock(s: &Shared) -> MutexGuard<'_, Inner> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeJellyfin {
    /// Starts an empty server.
    ///
    /// # Panics
    /// Panics when no loopback port can be bound (test environment failure).
    #[allow(clippy::expect_used)]
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            items: Vec::new(),
            libraries: Vec::new(),
            version: "10.10.3".to_string(),
            task_states: VecDeque::from([false]),
            task_present: true,
            refresh_status: 204,
            faults: BTreeMap::new(),
            recorded: Vec::new(),
        }));
        let app = Router::new()
            .route("/health", get(health))
            .route("/System/Info", get(info))
            .route("/Library/VirtualFolders", get(libraries))
            .route("/Items", get(items))
            .route("/ScheduledTasks", get(tasks))
            .route("/Library/Refresh", post(refresh_library))
            .route("/Items/{id}/Refresh", post(refresh_item))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, inner, task }
    }

    /// Starts a server preloaded with [`common_library`].
    pub async fn with_common_library() -> Self {
        let s = Self::start().await;
        s.set_libraries(common_libraries());
        s.set_items(common_library());
        s
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_items(&self, items: Vec<Value>) {
        lock(&self.inner).items = items;
    }

    pub fn set_libraries(&self, libs: Vec<Value>) {
        lock(&self.inner).libraries = libs;
    }

    pub fn set_version(&self, v: &str) {
        lock(&self.inner).version = v.to_string();
    }

    /// Successive "is the library scan running" answers; the last repeats forever.
    pub fn set_task_states(&self, states: &[bool]) {
        lock(&self.inner).task_states = states.iter().copied().collect();
    }

    /// When false, `/ScheduledTasks` has no library scan task.
    pub fn set_task_present(&self, present: bool) {
        lock(&self.inner).task_present = present;
    }

    /// Status of both refresh endpoints (default 204).
    pub fn set_refresh_status(&self, status: u16) {
        lock(&self.inner).refresh_status = status;
    }

    pub fn set_fault(&self, endpoint: &'static str, fault: Fault) {
        lock(&self.inner).faults.insert(endpoint, fault);
    }

    pub fn clear_faults(&self) {
        lock(&self.inner).faults.clear();
    }

    #[must_use]
    pub fn recorded(&self) -> Vec<Recorded> {
        lock(&self.inner).recorded.clone()
    }

    /// Number of recorded requests with this method and path.
    #[must_use]
    pub fn count(&self, method: &str, path: &str) -> usize {
        lock(&self.inner)
            .recorded
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .count()
    }

    /// Number of POST requests of any kind (every mutating call).
    #[must_use]
    pub fn post_count(&self) -> usize {
        lock(&self.inner)
            .recorded
            .iter()
            .filter(|r| r.method == "POST")
            .count()
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures

fn movie(id: &str, name: &str, path: &str, tmdb: &str) -> Value {
    json!({"Id": id, "Name": name, "Type": "Movie", "Path": path,
           "ProviderIds": {"Tmdb": tmdb, "Imdb": format!("tt{tmdb}")}})
}

fn episode(id: &str, series: &str, season: Option<i32>, ep: Option<i32>, path: &str) -> Value {
    let mut v = json!({"Id": id, "Name": format!("{series} episode"), "Type": "Episode",
        "Path": path, "SeriesName": series, "ProviderIds": {"Tvdb": format!("9{id}")}});
    if let Some(s) = season {
        v["ParentIndexNumber"] = json!(s);
    }
    if let Some(e) = ep {
        v["IndexNumber"] = json!(e);
    }
    v
}

/// Library roots as the fake Jellyfin sees them (`/media/...`).
#[must_use]
pub fn common_libraries() -> Vec<Value> {
    vec![
        json!({"Name": "Movies", "Locations": ["/media/movies"], "CollectionType": "movies", "ItemId": "L1"}),
        json!({"Name": "Shows", "Locations": ["/media/shows"], "CollectionType": "tvshows", "ItemId": "L2"}),
    ]
}

/// A common healthy layout: two films, regular episodes, a special (season 0) and a
/// multi-episode file.
#[must_use]
pub fn common_library() -> Vec<Value> {
    let mut multi = episode(
        "e3",
        "Show A",
        Some(1),
        Some(3),
        "/media/shows/Show A/Season 01/Show A - S01E03-E04.mkv",
    );
    multi["IndexNumberEnd"] = json!(4);
    vec![
        movie(
            "m1",
            "Film One",
            "/media/movies/Film One (2020)/Film One (2020).mkv",
            "100",
        ),
        movie(
            "m2",
            "Film Two",
            "/media/movies/Film Two (2018)/Film Two (2018).mkv",
            "200",
        ),
        episode(
            "e1",
            "Show A",
            Some(1),
            Some(1),
            "/media/shows/Show A/Season 01/Show A - S01E01.mkv",
        ),
        episode(
            "e2",
            "Show A",
            Some(1),
            Some(2),
            "/media/shows/Show A/Season 01/Show A - S01E02.mkv",
        ),
        multi,
        episode(
            "e0",
            "Show A",
            Some(0),
            Some(1),
            "/media/shows/Show A/Specials/Show A - S00E01.mkv",
        ),
    ]
}

/// Ambiguous layouts: an unrecognized `Video`, an episode without numbering, an episode
/// placed in a different season than expected, two items claiming the same path, and
/// an absolute-numbered anime file.
#[must_use]
pub fn ambiguous_library() -> Vec<Value> {
    vec![
        json!({"Id": "v1", "Name": "Random Clip", "Type": "Video",
               "Path": "/media/shows/Show B/Show B - 05.mkv"}),
        episode(
            "x1",
            "Show B",
            None,
            None,
            "/media/shows/Show B/Season 01/Show B - S01E09.mkv",
        ),
        episode(
            "x2",
            "Show C",
            Some(2),
            Some(1),
            "/media/shows/Show C/Season 01/Show C - S01E13.mkv",
        ),
        episode(
            "d1",
            "Show D",
            Some(1),
            Some(1),
            "/media/shows/Show D/dup.mkv",
        ),
        episode(
            "d2",
            "Show D",
            Some(1),
            Some(2),
            "/media/shows/Show D/dup.mkv",
        ),
        episode(
            "a1",
            "Anime E",
            Some(1),
            Some(25),
            "/media/shows/Anime E/Anime E - 025.mkv",
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Handlers

fn record(s: &Shared, method: &str, path: &str, q: &BTreeMap<String, String>, h: &HeaderMap) {
    let token_ok = authed(h);
    lock(s).recorded.push(Recorded {
        method: method.to_string(),
        path: path.to_string(),
        query: q.clone(),
        token_ok,
    });
}

fn authed(h: &HeaderMap) -> bool {
    h.get("x-emby-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t == FAKE_TOKEN)
}

/// Applies the fault of `key`. `Some(response)` means the fault short-circuits the handler.
async fn fault(s: &Shared, key: &'static str) -> Option<Response> {
    let f = lock(s).faults.get(key).cloned()?;
    if let Some(d) = f.delay {
        tokio::time::sleep(d).await;
    }
    if let Some(raw) = f.raw_body {
        return Some(([(header::CONTENT_TYPE, "application/json")], raw).into_response());
    }
    f.status.map(|c| {
        StatusCode::from_u16(c)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response()
    })
}

async fn health(State(s): State<Shared>, h: HeaderMap) -> Response {
    record(&s, "GET", "/health", &BTreeMap::new(), &h);
    if let Some(r) = fault(&s, "health").await {
        return r;
    }
    "Healthy".into_response()
}

async fn info(State(s): State<Shared>, h: HeaderMap) -> Response {
    record(&s, "GET", "/System/Info", &BTreeMap::new(), &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "info").await {
        return r;
    }
    let v = lock(&s).version.clone();
    axum::Json(json!({"Version": v, "ServerName": "fake", "ProductName": "Jellyfin Server"}))
        .into_response()
}

async fn libraries(State(s): State<Shared>, h: HeaderMap) -> Response {
    record(&s, "GET", "/Library/VirtualFolders", &BTreeMap::new(), &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "libraries").await {
        return r;
    }
    let libs = lock(&s).libraries.clone();
    axum::Json(libs).into_response()
}

async fn items(
    State(s): State<Shared>,
    h: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    record(&s, "GET", "/Items", &q, &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "items").await {
        return r;
    }
    let all = lock(&s).items.clone();
    let start: usize = q
        .get("StartIndex")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = q
        .get("Limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let page: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
    axum::Json(json!({"Items": page, "TotalRecordCount": all.len(), "StartIndex": start}))
        .into_response()
}

async fn tasks(State(s): State<Shared>, h: HeaderMap) -> Response {
    record(&s, "GET", "/ScheduledTasks", &BTreeMap::new(), &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "tasks").await {
        return r;
    }
    let (present, running) = {
        let mut g = lock(&s);
        let running = if g.task_states.len() > 1 {
            g.task_states.pop_front().unwrap_or(false)
        } else {
            g.task_states.front().copied().unwrap_or(false)
        };
        (g.task_present, running)
    };
    let mut out = vec![
        json!({"Name": "Clean Cache Directory", "Key": "DeleteCacheFileTask", "State": "Idle"}),
    ];
    if present {
        out.push(
            json!({"Name": "Scan Media Library", "Key": "RefreshLibrary",
                        "State": if running { "Running" } else { "Idle" }}),
        );
    }
    axum::Json(out).into_response()
}

async fn refresh_library(State(s): State<Shared>, h: HeaderMap) -> Response {
    record(&s, "POST", "/Library/Refresh", &BTreeMap::new(), &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "refresh_library").await {
        return r;
    }
    let code = lock(&s).refresh_status;
    StatusCode::from_u16(code)
        .unwrap_or(StatusCode::NO_CONTENT)
        .into_response()
}

async fn refresh_item(
    State(s): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    record(&s, "POST", &format!("/Items/{id}/Refresh"), &q, &h);
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(r) = fault(&s, "refresh_item").await {
        return r;
    }
    let code = lock(&s).refresh_status;
    StatusCode::from_u16(code)
        .unwrap_or(StatusCode::NO_CONTENT)
        .into_response()
}
