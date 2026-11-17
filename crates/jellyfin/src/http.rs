//! Shared HTTP core: token header, timeouts, bounded body reads, status mapping, read retries.
//!
//! Errors never carry the host, the URL, headers or upstream bodies: only the operation label
//! (`GET /Items`) and a reason. The token lives in a sensitive header value.

use std::fmt;
use std::time::Duration;

use removarr_config::Secret;
use removarr_domain::error::{ErrorClass, PortError};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;
use url::Url;

/// Header carrying the API key. Never put in the query string (it would reach logs).
const TOKEN_HEADER: &str = "X-Emby-Token";

/// Timeouts, response cap and read retry policy (architecture section 8.4).
#[derive(Debug, Clone)]
pub struct HttpSettings {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_response_bytes: u64,
    /// Retries of read calls that fail with `Unavailable`. Mutating calls are never retried.
    pub read_retries: u32,
    /// First backoff, doubled per retry, with +-20 % jitter.
    pub backoff_base: Duration,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(20),
            max_response_bytes: 32 * 1024 * 1024,
            read_retries: 3,
            backoff_base: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Core {
    client: reqwest::Client,
    base: Url,
    settings: HttpSettings,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("base", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

fn err(class: ErrorClass, op: &str, reason: &str) -> PortError {
    PortError::new(class, format!("jellyfin {op}: {reason}"))
}

fn map_transport(op: &str, e: &reqwest::Error) -> PortError {
    let reason = if e.is_timeout() {
        "request timed out"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "response body could not be read"
    } else {
        "transport error"
    };
    err(ErrorClass::Unavailable, op, reason)
}

fn map_status(op: &str, status: reqwest::StatusCode) -> PortError {
    let code = status.as_u16();
    let class = match code {
        401 => ErrorClass::Unauthorized,
        403 => ErrorClass::Forbidden,
        404 => ErrorClass::NotFound,
        429 => ErrorClass::RateLimited,
        408 | 500..=599 => ErrorClass::Unavailable,
        _ => ErrorClass::Rejected,
    };
    err(class, op, &format!("HTTP {code}"))
}

impl Core {
    pub(crate) fn new(
        base: &Url,
        api_key: &Secret,
        settings: HttpSettings,
    ) -> Result<Self, PortError> {
        let mut token = HeaderValue::from_str(api_key.expose()).map_err(|_| {
            PortError::new(
                ErrorClass::Config,
                "jellyfin api key is not a valid header value",
            )
        })?;
        token.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(TOKEN_HEADER, token);
        headers.insert(USER_AGENT, HeaderValue::from_static("removarr"));
        // No redirects (the token must never follow a redirect) and no environment proxy
        // (Jellyfin is a local service).
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(settings.connect_timeout)
            .timeout(settings.request_timeout)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| {
                PortError::new(
                    ErrorClass::Config,
                    "jellyfin http client could not be built",
                )
            })?;
        Ok(Self {
            client,
            base: base.clone(),
            settings,
        })
    }

    fn url(&self, op: &str, segments: &[&str], query: &[(&str, &str)]) -> Result<Url, PortError> {
        let mut u = self.base.clone();
        u.set_query(None);
        u.set_fragment(None);
        {
            let mut seg = u
                .path_segments_mut()
                .map_err(|()| err(ErrorClass::Config, op, "base url cannot carry a path"))?;
            seg.pop_if_empty().extend(segments);
        }
        if !query.is_empty() {
            u.query_pairs_mut().extend_pairs(query);
        }
        Ok(u)
    }

    async fn send_once(
        &self,
        post: bool,
        op: &str,
        url: Url,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), PortError> {
        let req = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        };
        let mut resp = req.send().await.map_err(|e| map_transport(op, &e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(map_status(op, status));
        }
        let max = self.settings.max_response_bytes;
        if resp.content_length().is_some_and(|l| l > max) {
            return Err(err(ErrorClass::Rejected, op, "response exceeds size limit"));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| map_transport(op, &e))? {
            if (buf.len() as u64).saturating_add(chunk.len() as u64) > max {
                return Err(err(ErrorClass::Rejected, op, "response exceeds size limit"));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok((status, buf))
    }

    /// Jitter in [0.8, 1.2) derived from the clock; no rng dependency.
    fn backoff(&self, attempt: u32) -> Duration {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let jitter = 0.8 + f64::from(nanos % 400) / 1000.0;
        self.settings
            .backoff_base
            .saturating_mul(1u32 << attempt.min(6))
            .mul_f64(jitter)
    }

    /// GET with bounded read retries (only `Unavailable` is retried).
    pub(crate) async fn get_bytes(
        &self,
        op: &'static str,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Vec<u8>, PortError> {
        let mut attempt = 0;
        loop {
            let url = self.url(op, segments, query)?;
            match self.send_once(false, op, url).await {
                Ok((_, body)) => return Ok(body),
                Err(e) if e.class.is_retryable_read() && attempt < self.settings.read_retries => {
                    tracing::debug!(op, attempt, "jellyfin read failed, retrying");
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        op: &'static str,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, PortError> {
        let body = self.get_bytes(op, segments, query).await?;
        serde_json::from_slice(&body)
            .map_err(|_| err(ErrorClass::Rejected, op, "malformed response"))
    }

    /// POST that must answer 2xx (Jellyfin answers 204). Never retried.
    pub(crate) async fn post_accepted(
        &self,
        op: &'static str,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<(), PortError> {
        let url = self.url(op, segments, query)?;
        self.send_once(true, op, url).await.map(|_| ())
    }
}
