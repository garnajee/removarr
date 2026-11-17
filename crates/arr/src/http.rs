//! HTTP core shared by the reader and the commander: API key header, timeouts, bounded body
//! reads, status mapping, read retries and the concurrency limit.
//!
//! Reads go through [`Http::get`]. The only mutating entry point, [`Http::send`], requires a
//! `LiveMode` token, which the reader never holds, so a read path cannot issue a POST, PUT or
//! DELETE (INV-22, INV-38).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use removarr_config::{LiveMode, Secret};
use removarr_domain::arr::ArrService;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use url::Url;

use crate::error::ArrError;

/// The API key travels only in this header, never in the query string.
const API_KEY_HEADER: &str = "X-Api-Key";

/// Connection settings for one Sonarr or Radarr instance.
#[derive(Debug, Clone)]
pub struct ArrSettings {
    pub service: ArrService,
    pub base_url: Url,
    pub api_key: Secret,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    /// Interactive searches query every indexer, which routinely exceeds the read timeout.
    pub search_timeout: Duration,
    pub max_response_bytes: u64,
    pub http_concurrency: usize,
    /// Retries of read calls that fail with `Unavailable`. Mutating calls are never retried.
    pub read_retries: u32,
    /// First backoff, doubled per retry, with +-20 % jitter.
    pub backoff_base: Duration,
}

impl ArrSettings {
    /// Settings for `service` from the validated configuration; `None` when not configured.
    #[must_use]
    pub fn from_config(cfg: &removarr_config::Config, service: ArrService) -> Option<Self> {
        let svc = match service {
            ArrService::Sonarr => cfg.services.sonarr.as_ref(),
            ArrService::Radarr => cfg.services.radarr.as_ref(),
        }?;
        Some(Self {
            service,
            base_url: svc.url.clone(),
            api_key: svc.api_key.clone(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            search_timeout: Duration::from_secs(120),
            max_response_bytes: cfg.limits.max_response_bytes,
            http_concurrency: cfg.limits.http_concurrency_per_service.max(1) as usize,
            read_retries: 3,
            backoff_base: Duration::from_secs(1),
        })
    }
}

#[derive(Clone)]
pub(crate) struct Http {
    inner: Arc<Inner>,
}

struct Inner {
    service: ArrService,
    client: reqwest::Client,
    base: Url,
    api_key: Secret,
    permits: Semaphore,
    max_bytes: u64,
    request_timeout: Duration,
    search_timeout: Duration,
    read_retries: u32,
    backoff_base: Duration,
}

impl fmt::Debug for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http")
            .field("service", &self.inner.service)
            .field("base", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// A read whose timeout differs from the default one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timeout {
    Default,
    Search,
}

impl Http {
    pub(crate) fn new(s: &ArrSettings) -> Result<Self, ArrError> {
        let mut key = HeaderValue::from_str(s.api_key.expose()).map_err(|_| {
            ArrError::Config(format!(
                "{} api key is not a valid header value",
                s.service.as_str()
            ))
        })?;
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, key);
        headers.insert(USER_AGENT, HeaderValue::from_static("removarr"));
        // No redirects: the API key header would follow them to another host. No environment
        // proxy: ARR runs on the local network.
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(s.connect_timeout)
            .timeout(s.request_timeout)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| {
                ArrError::Config(format!(
                    "{} http client could not be built",
                    s.service.as_str()
                ))
            })?;
        let mut base = s.base_url.clone();
        base.set_query(None);
        base.set_fragment(None);
        Ok(Self {
            inner: Arc::new(Inner {
                service: s.service,
                client,
                base,
                api_key: s.api_key.clone(),
                permits: Semaphore::new(s.http_concurrency.max(1)),
                max_bytes: s.max_response_bytes,
                request_timeout: s.request_timeout,
                search_timeout: s.search_timeout,
                read_retries: s.read_retries,
                backoff_base: s.backoff_base,
            }),
        })
    }

    pub(crate) fn service(&self) -> ArrService {
        self.inner.service
    }

    /// Operation label used in errors and logs: service, method and path, never the query.
    fn op(&self, method: &Method, path: &str) -> String {
        format!("{} {method} {path}", self.inner.service.as_str())
    }

    /// Scrubs free text that came from ARR (rejection reasons, queue messages): the API key
    /// value and anything that looks like a credential.
    pub(crate) fn scrub(&self, text: &str) -> String {
        let key = self.inner.api_key.expose();
        let out = if key.len() >= 4 {
            text.replace(key, removarr_config::REDACTED)
        } else {
            text.to_string()
        };
        removarr_config::redact_text(&out)
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url, ArrError> {
        let mut u = self.inner.base.clone();
        {
            let mut seg = u.path_segments_mut().map_err(|()| {
                ArrError::Config(format!(
                    "{} base url cannot carry a path",
                    self.inner.service.as_str()
                ))
            })?;
            seg.pop_if_empty()
                .extend(path.split('/').filter(|s| !s.is_empty()));
        }
        if !query.is_empty() {
            u.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        Ok(u)
    }

    async fn once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<Vec<u8>, ArrError> {
        let op = self.op(&method, path);
        let url = self.url(path, query)?;
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|_| ArrError::Unavailable {
                op: op.clone(),
                reason: "connector closed",
            })?;
        let mut req = self.inner.client.request(method, url).timeout(timeout);
        if let Some(b) = body {
            req = req.json(b);
        }
        let mut resp = req.send().await.map_err(|e| transport(&op, &e))?;
        let status = resp.status();
        tracing::debug!(op = %op, status = status.as_u16(), "arr request");
        check_status(&op, status)?;
        let max = self.inner.max_bytes;
        if resp.content_length().is_some_and(|l| l > max) {
            return Err(ArrError::TooLarge { op, limit: max });
        }
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| transport(&op, &e))? {
            if (buf.len() as u64).saturating_add(chunk.len() as u64) > max {
                return Err(ArrError::TooLarge { op, limit: max });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    /// Jitter in [0.8, 1.2) derived from the clock (no rng dependency).
    fn backoff(&self, attempt: u32) -> Duration {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let jitter = 0.8 + f64::from(nanos % 400) / 1000.0;
        self.inner
            .backoff_base
            .saturating_mul(1u32 << attempt.min(6))
            .mul_f64(jitter)
    }

    /// GET with bounded read retries (only `Unavailable` is retried) and JSON decoding.
    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        timeout: Timeout,
    ) -> Result<T, ArrError> {
        let timeout = match timeout {
            Timeout::Default => self.inner.request_timeout,
            Timeout::Search => self.inner.search_timeout,
        };
        let mut attempt = 0;
        let body = loop {
            match self.once(Method::GET, path, query, None, timeout).await {
                Ok(b) => break b,
                Err(e @ ArrError::Unavailable { .. }) if attempt < self.inner.read_retries => {
                    tracing::debug!(error = %e, attempt, "arr read failed, retrying");
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        };
        serde_json::from_slice(&body).map_err(|_| ArrError::Decode {
            op: self.op(&Method::GET, path),
        })
    }

    /// The only mutating request path. It needs a `LiveMode` token and is never retried
    /// (REJ-21). Returns the raw body (may be empty).
    pub(crate) async fn send(
        &self,
        _live: &LiveMode,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>, ArrError> {
        let timeout = self.inner.request_timeout;
        let out = self.once(method.clone(), path, query, body, timeout).await;
        match &out {
            Ok(_) => tracing::info!(op = %self.op(&method, path), "arr mutating request accepted"),
            Err(e) => tracing::warn!(error = %e, "arr mutating request failed"),
        }
        out
    }

    pub(crate) fn decode<T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
        body: &[u8],
    ) -> Result<T, ArrError> {
        serde_json::from_slice(body).map_err(|_| ArrError::Decode {
            op: self.op(method, path),
        })
    }
}

fn transport(op: &str, e: &reqwest::Error) -> ArrError {
    let reason = if e.is_timeout() {
        "request timed out"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "response body could not be read"
    } else {
        "transport error"
    };
    ArrError::Unavailable {
        op: op.to_string(),
        reason,
    }
}

fn check_status(op: &str, status: StatusCode) -> Result<(), ArrError> {
    let op = op.to_string();
    let code = status.as_u16();
    match code {
        200..=299 => Ok(()),
        300..=399 => Err(ArrError::Redirect { op, status: code }),
        401 | 403 => Err(ArrError::Auth { op, status: code }),
        404 => Err(ArrError::NotFound { op }),
        408 | 429 | 500..=599 => Err(ArrError::Unavailable {
            op,
            reason: "server error or overload",
        }),
        _ => Err(ArrError::Http { op, status: code }),
    }
}
