//! HTTP session: login (200 `Ok.`, 200 empty, 204), version cache, bounded bodies, error mapping.
//!
//! Errors are redacted: the password and anything `redact_text` recognizes never leave this module.

use std::collections::BTreeSet;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use removarr_config::{PathMapper, Secret};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::torrent::{TorrentHash, TorrentInfo};
use reqwest::{Client, Method, Url, redirect};

use crate::parse::{VersionInfo, parse_torrents, parse_version};
use crate::settings::{QbitSettings, RetryPolicy};

#[derive(Debug, Default)]
struct Session {
    logged_in: bool,
    rejected: Option<(Instant, PortError)>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    client: Client,
    base: String,
    username: String,
    password: Secret,
    pub(crate) paths: PathMapper,
    max_bytes: u64,
    retry: RetryPolicy,
    cooldown: Duration,
    pub(crate) concurrency: usize,
    session: tokio::sync::Mutex<Session>,
    version: StdMutex<Option<VersionInfo>>,
    /// Torrents whose mapping was uncertain in the last listing.
    info_blocked: StdMutex<BTreeSet<TorrentHash>>,
    /// Torrents with an uncertain file path (from `files`).
    file_blocked: StdMutex<BTreeSet<TorrentHash>>,
}

fn err(class: ErrorClass, detail: impl Into<String>) -> PortError {
    PortError::new(class, detail)
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Inner {
    pub(crate) fn new(s: QbitSettings) -> Result<Self, PortError> {
        let client = Client::builder()
            .cookie_store(true)
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(s.connect_timeout)
            .timeout(s.request_timeout)
            .build()
            .map_err(|_| err(ErrorClass::Internal, "cannot build the HTTP client"))?;
        Ok(Self {
            client,
            base: s.url.as_str().trim_end_matches('/').to_string(),
            username: s.username,
            password: s.password,
            paths: s.paths,
            max_bytes: s.max_response_bytes,
            retry: s.retry,
            cooldown: s.login_cooldown,
            concurrency: s.concurrency.max(1),
            session: tokio::sync::Mutex::new(Session::default()),
            version: StdMutex::new(None),
            info_blocked: StdMutex::new(BTreeSet::new()),
            file_blocked: StdMutex::new(BTreeSet::new()),
        })
    }

    fn net_err(&self, e: &reqwest::Error) -> PortError {
        // Kind only: no URL, host or library message (which may echo either).
        let what = if e.is_timeout() {
            "request timed out"
        } else if e.is_connect() {
            "connection failed"
        } else if e.is_body() || e.is_decode() {
            "response body interrupted"
        } else {
            "transport error"
        };
        err(ErrorClass::Unavailable, format!("qbittorrent: {what}"))
    }

    /// Status only: upstream bodies are never echoed (they could carry a SID or host).
    fn status_err(&self, status: u16) -> PortError {
        let class = match status {
            401 | 403 => ErrorClass::Unauthorized,
            404 => ErrorClass::NotFound,
            408 | 429 | 500..=599 => ErrorClass::Unavailable,
            _ => ErrorClass::Rejected,
        };
        err(class, format!("qbittorrent returned HTTP {status}"))
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        form: Option<&[(&str, &str)]>,
    ) -> Result<(u16, Vec<u8>), PortError> {
        let url = Url::parse(&format!("{}{path}", self.base))
            .map_err(|_| err(ErrorClass::Config, "invalid qbittorrent URL"))?;
        let mut rb = self
            .client
            .request(method, url)
            .header(reqwest::header::REFERER, &self.base);
        if !query.is_empty() {
            rb = rb.query(query);
        }
        if let Some(f) = form {
            rb = rb.form(f);
        }
        let mut resp = rb.send().await.map_err(|e| self.net_err(&e))?;
        let status = resp.status().as_u16();
        if resp.content_length().is_some_and(|l| l > self.max_bytes) {
            return Err(err(
                ErrorClass::Rejected,
                "qbittorrent response exceeds the size limit",
            ));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| self.net_err(&e))? {
            let next = buf.len() as u64 + chunk.len() as u64;
            if next > self.max_bytes {
                return Err(err(
                    ErrorClass::Rejected,
                    "qbittorrent response exceeds the size limit",
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok((status, buf))
    }

    async fn login_once(&self) -> Result<(), PortError> {
        let (status, body) = self
            .send_once(
                Method::POST,
                "/api/v2/auth/login",
                &[],
                Some(&[
                    ("username", self.username.as_str()),
                    ("password", self.password.expose()),
                ]),
            )
            .await?;
        let text = String::from_utf8_lossy(&body);
        match (status, text.trim()) {
            (204, _) | (200, "Ok." | "") => Ok(()),
            (200, "Fails.") => Err(err(
                ErrorClass::Unauthorized,
                "qbittorrent rejected the credentials",
            )),
            (200, _) => Err(err(
                ErrorClass::Unauthorized,
                "unexpected qbittorrent login response, authentication not confirmed",
            )),
            (403, _) => Err(err(
                ErrorClass::Unauthorized,
                "qbittorrent refused the login (HTTP 403, possibly banned)",
            )),
            _ => Err(self.status_err(status)),
        }
    }

    async fn ensure_login(&self) -> Result<(), PortError> {
        let mut s = self.session.lock().await;
        if s.logged_in {
            return Ok(());
        }
        if let Some((at, e)) = &s.rejected
            && at.elapsed() < self.cooldown
        {
            return Err(e.clone());
        }
        match self.login_once().await {
            Ok(()) => {
                s.logged_in = true;
                s.rejected = None;
                Ok(())
            }
            Err(e) => {
                if e.class == ErrorClass::Unauthorized {
                    s.rejected = Some((Instant::now(), e.clone()));
                }
                Err(e)
            }
        }
    }

    /// Authenticated request. A 401/403 means nothing was executed, so one re-login and one
    /// repeat are safe for reads and mutations alike. Transport errors are returned as is.
    pub(crate) async fn authed(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        form: Option<&[(&str, &str)]>,
    ) -> Result<Vec<u8>, PortError> {
        for attempt in 0..2 {
            self.ensure_login().await?;
            let (status, body) = self.send_once(method.clone(), path, query, form).await?;
            if status == 401 || status == 403 {
                self.session.lock().await.logged_in = false;
                if attempt == 0 {
                    continue;
                }
                return Err(self.status_err(status));
            }
            if !(200..300).contains(&status) {
                return Err(self.status_err(status));
            }
            return Ok(body);
        }
        Err(err(
            ErrorClass::Unauthorized,
            "qbittorrent session not established",
        ))
    }

    /// GET with the read retry policy (`Unavailable` only, exponential backoff with jitter).
    pub(crate) async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<u8>, PortError> {
        let mut delay = self.retry.base_delay;
        let mut tries = 0;
        loop {
            match self.authed(Method::GET, path, query, None).await {
                Err(e) if e.class.is_retryable_read() && tries < self.retry.max_retries => {
                    tries += 1;
                    tokio::time::sleep(jitter(delay)).await;
                    delay = delay.saturating_mul(2);
                }
                other => return other,
            }
        }
    }

    pub(crate) async fn fetch_version(&self) -> Result<VersionInfo, PortError> {
        let body = self.get("/api/v2/app/version", &[]).await?;
        let v = parse_version(&body)?;
        *lock(&self.version) = Some(v.clone());
        Ok(v)
    }

    pub(crate) async fn version(&self) -> Result<VersionInfo, PortError> {
        let cached = lock(&self.version).clone();
        match cached {
            Some(v) => Ok(v),
            None => self.fetch_version().await,
        }
    }

    pub(crate) async fn list_torrents(&self) -> Result<Vec<TorrentInfo>, PortError> {
        let body = self.get("/api/v2/torrents/info", &[]).await?;
        let list = parse_torrents(&body, &self.paths)?;
        *lock(&self.info_blocked) = list
            .iter()
            .filter(|t| crate::parse::info_mapping_uncertain(t))
            .map(|t| t.hash.clone())
            .collect();
        Ok(list)
    }

    pub(crate) async fn one_torrent(&self, hash: &TorrentHash) -> Result<TorrentInfo, PortError> {
        let body = self
            .get("/api/v2/torrents/info", &[("hashes", hash.as_str())])
            .await?;
        parse_torrents(&body, &self.paths)?
            .into_iter()
            .find(|t| &t.hash == hash)
            .ok_or_else(|| err(ErrorClass::NotFound, "torrent not found"))
    }

    pub(crate) fn set_file_uncertain(&self, hash: &TorrentHash, uncertain: bool) {
        let mut s = lock(&self.file_blocked);
        if uncertain {
            s.insert(hash.clone());
        } else {
            s.remove(hash);
        }
    }

    pub(crate) fn mapping_uncertain(&self, hash: &TorrentHash) -> bool {
        lock(&self.info_blocked).contains(hash) || lock(&self.file_blocked).contains(hash)
    }
}

/// +-20 % jitter from the clock; no randomness dependency needed for backoff spreading.
fn jitter(d: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos());
    let permille = 800 + (nanos % 401);
    d.mul_f64(f64::from(permille) / 1000.0)
}
