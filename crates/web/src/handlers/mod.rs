//! Route handlers. They validate input, call one use case through the [`crate::state`]
//! interfaces and render a typed view; they hold no domain policy. Display handlers (`GET`)
//! only read; command handlers (`POST`) require the command grant of the guard middleware.

pub mod dashboard;
pub mod issues;
pub mod jobs;
pub mod login;
pub mod media;
pub mod ops;
pub mod plans;
pub mod reports;

#[cfg(test)]
mod tests;

use askama::Template;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};

use removarr_domain::issue::IssueCategory;

use crate::error::WebError;
use crate::i18n::{Locale, Tr};
use crate::state::{AppState, Principal};

/// Marker inserted by the router only after the guard approved an `Access::Command` request.
/// It cannot be built outside this crate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandGrant(());

impl CommandGrant {
    pub(crate) fn new() -> Self {
        Self(())
    }
}

/// Layout data shared by every page.
#[derive(Debug, Clone)]
pub struct Chrome {
    pub csrf: String,
    pub user: String,
    pub dry_run: bool,
    pub lang: &'static str,
}

/// Request context of a display route: locale, HTMX flag and the authenticated principal.
/// Fails closed (401) when the guard did not run.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub tr: Tr,
    pub htmx: bool,
    pub chrome: Chrome,
    /// i18n key of the one-shot confirmation left by the previous command (flash cookie).
    pub flash: Option<&'static str>,
    /// Request path, only used to mark the current navigation entry (`aria-current`).
    pub path: String,
}

/// Cookie carrying a one-shot confirmation code across the redirect that follows a command.
/// It holds only a code from [`FLASH_CODES`] (never text, never an id) and no authorization.
pub const FLASH_COOKIE: &str = "removarr_flash";

/// Accepted flash codes and their message keys; any other cookie value is ignored.
pub const FLASH_CODES: [(&str, &str); 9] = [
    ("ignored", "flash.ignored"),
    ("restored", "flash.restored"),
    ("plan_created", "flash.plan_created"),
    ("approved", "flash.approved"),
    ("discarded", "flash.discarded"),
    ("job_queued", "flash.job_queued"),
    ("job_cancel", "flash.job_cancel"),
    ("resumed", "flash.resumed"),
    ("locale", "flash.locale"),
];

fn flash_from(parts: &Parts) -> Option<&'static str> {
    let code = parts
        .headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == FLASH_COOKIE)
        .map(|(_, v)| v.trim())?;
    FLASH_CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, k)| *k)
}

fn is_htmx(parts: &Parts) -> bool {
    parts
        .headers
        .get("hx-request")
        .is_some_and(|v| v.as_bytes() == b"true")
}

impl FromRequestParts<AppState> for Ctx {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, WebError> {
        let principal = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| WebError::status(StatusCode::UNAUTHORIZED, "error.unauthorized"))?;
        let locale = Locale::from_headers(&parts.headers);
        Ok(Ctx {
            tr: Tr::new(locale),
            htmx: is_htmx(parts),
            chrome: Chrome {
                csrf: principal.csrf_token,
                user: principal.user,
                dry_run: s.settings.dry_run,
                lang: locale.as_str(),
            },
            flash: flash_from(parts),
            path: parts.uri.path().to_string(),
        })
    }
}

/// Context of a command route: requires the command grant (session + CSRF + origin checked
/// by the guard). Missing grant = 403, whatever the route wiring.
#[derive(Debug, Clone)]
pub struct Cmd(pub Ctx);

impl FromRequestParts<AppState> for Cmd {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, WebError> {
        if parts.extensions.get::<CommandGrant>().is_none() {
            return Err(WebError::status(StatusCode::FORBIDDEN, "error.forbidden"));
        }
        Ctx::from_request_parts(parts, s).await.map(Cmd)
    }
}

/// Renders the full page, or only the fragment for an HTMX request.
pub(crate) fn render(ctx: &Ctx, page: &impl Template, fragment: &impl Template) -> Response {
    let out = if ctx.htmx {
        fragment.render()
    } else {
        page.render()
    };
    match out {
        Ok(html) => {
            let mut r = Html(html).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if ctx.htmx {
                r.headers_mut()
                    .insert(header::VARY, HeaderValue::from_static("HX-Request"));
            }
            if ctx.flash.is_some() {
                // Shown once: the cookie is consumed by the page that displays it.
                r.headers_mut().append(
                    header::SET_COOKIE,
                    HeaderValue::from_static(
                        "removarr_flash=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
                    ),
                );
            }
            r
        }
        Err(e) => WebError::internal(&format!("template rendering failed: {e}")).into_response(),
    }
}

/// Unwraps a form, mapping any rejection to the same validation shape (413 for oversize).
pub(crate) fn parse_form<T>(
    f: Result<axum_extra::extract::Form<T>, axum_extra::extract::FormRejection>,
) -> Result<T, WebError> {
    match f {
        Ok(axum_extra::extract::Form(t)) => Ok(t),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(WebError::status(
            StatusCode::PAYLOAD_TOO_LARGE,
            "error.too_large",
        )),
        Err(_) => Err(WebError::invalid("form", "malformed form")),
    }
}

/// After a command: 303 to `to`, or `HX-Redirect` for HTMX. `to` is always built by the
/// handler from validated ids (never from user text).
pub(crate) fn see_other(ctx: &Ctx, to: &str) -> Response {
    if ctx.htmx
        && let Ok(v) = HeaderValue::from_str(to)
    {
        let mut r = StatusCode::OK.into_response();
        r.headers_mut().insert("hx-redirect", v);
        return r;
    }
    Redirect::to(to).into_response()
}

/// Like [`see_other`], and leaves a one-shot confirmation (`code` from [`FLASH_CODES`]) for the
/// destination page.
pub(crate) fn see_other_flash(ctx: &Ctx, to: &str, code: &'static str) -> Response {
    debug_assert!(FLASH_CODES.iter().any(|(c, _)| *c == code));
    let mut r = see_other(ctx, to);
    r.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{FLASH_COOKIE}={code}; Path=/; Max-Age=60; HttpOnly; SameSite=Strict"
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("removarr_flash=; Max-Age=0")),
    );
    r
}

/// `CamelCase` or `Variant:detail` to the `snake_case` part before the colon.
pub(crate) fn snake(raw: &str) -> String {
    let head = raw.split([':', '(', ' ']).next().unwrap_or(raw);
    let mut out = String::with_capacity(head.len() + 4);
    let mut prev_lower = false;
    for ch in head.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_lower = true;
        } else {
            out.push('_');
            prev_lower = false;
        }
    }
    out
}

/// Presentation of a severity: `(css class, glyph id, label key)`. The glyph shape differs per
/// level so the state survives greyscale; the label is always rendered next to it.
#[must_use]
pub fn severity_look(sev: &str) -> (&'static str, &'static str, &'static str) {
    match sev {
        "critical" => ("crit", "alert-octagon", "severity.critical"),
        "warning" => ("warn", "alert-triangle", "severity.warning"),
        _ => ("info", "info-circle", "severity.info"),
    }
}

/// i18n key of a category label (`category.<slug>`).
#[must_use]
pub fn category_key(c: IssueCategory) -> &'static str {
    match c {
        IssueCategory::IncompleteStalled => "category.incomplete_stalled",
        IssueCategory::IncompleteOrphan => "category.incomplete_orphan",
        IssueCategory::HardlinkMissing => "category.hardlink_missing",
        IssueCategory::Duplicate => "category.duplicate",
        IssueCategory::OrphanCompleted => "category.orphan_completed",
        IssueCategory::TorrentMissingFiles => "category.torrent_missing_files",
        IssueCategory::MediaWithoutSource => "category.media_without_source",
        IssueCategory::Permission => "category.permission",
        IssueCategory::Junk => "category.junk",
        IssueCategory::TorrentProtected => "category.torrent_protected",
        IssueCategory::UnmappedTorrent => "category.unmapped_torrent",
        IssueCategory::Symlink => "category.symlink",
        IssueCategory::ArrQueue => "category.arr_queue",
    }
}

/// One issue tab of the navigation.
#[derive(Debug, Clone)]
pub struct Tab {
    pub slug: &'static str,
    pub label: &'static str,
    pub current: bool,
}

/// A number the templates pass to the plural helpers (values or references; negatives are 0).
pub trait Count {
    fn count(&self) -> u64;
}

macro_rules! count_impl {
    ($($t:ty),*) => {$(impl Count for $t {
        fn count(&self) -> u64 {
            u64::try_from(*self).unwrap_or(0)
        }
    })*};
}
count_impl!(u8, u16, u32, u64, usize, i32, i64);

impl<T: Count + ?Sized> Count for &T {
    fn count(&self) -> u64 {
        (**self).count()
    }
}

impl Ctx {
    /// Translates a machine code (`Draft`, `scan_incomplete`, `ServiceUnavailable:sonarr`) through
    /// `<prefix>.<snake_case>`; an unknown code is shown as received (the template escapes it).
    #[must_use]
    pub fn coded(&self, prefix: &str, raw: &str) -> String {
        match self.tr.get(&format!("{prefix}.{}", snake(raw))) {
            Some(text) => match raw.split_once(':') {
                Some((_, detail)) if !detail.is_empty() => format!("{text} ({detail})"),
                _ => text.to_string(),
            },
            None => raw.to_string(),
        }
    }

    /// Text of a reporting explanation: `explain.<code>` with its parameters when translated,
    /// else the English text built by the reporting crate (escaped by the template).
    #[must_use]
    pub fn explanation(&self, e: &removarr_reporting::ExplanationVm) -> String {
        match self.tr.get(&format!("explain.{}", e.code)) {
            Some(text) => {
                let mut out = text.to_string();
                for (k, v) in &e.params {
                    out = out.replace(&format!("{{{k}}}"), v);
                }
                out
            }
            None => e.text.clone(),
        }
    }

    /// SRI value of the pinned HTMX file (`integrity` attribute of the script tag).
    #[must_use]
    pub fn htmx_sri(&self) -> &'static str {
        crate::assets::HTMX_SRI
    }

    /// True when the current page is `path`, or below it when `prefix` is set.
    #[must_use]
    pub fn on(&self, path: &str, prefix: bool) -> bool {
        if prefix {
            self.path == path || self.path.starts_with(&format!("{path}/"))
        } else {
            self.path == path
        }
    }

    /// Plural text with `{n}` (see [`Tr::tn`]).
    #[must_use]
    pub fn tn(&self, key: &str, n: impl Count) -> String {
        self.tr.tn(key, n.count())
    }

    /// Text with `{n}` and `{m}` replaced (see [`Tr::fmt`]).
    #[must_use]
    pub fn nm(&self, key: &str, n: impl Count, m: impl Count) -> String {
        let (n, m) = (n.count(), m.count());
        self.tr
            .fmt(key, &[("n", &n.to_string()), ("m", &m.to_string())])
    }

    /// Issue tabs, in category order.
    #[must_use]
    pub fn tabs(&self) -> Vec<Tab> {
        IssueCategory::ALL
            .iter()
            .map(|c| Tab {
                slug: c.as_str(),
                label: self.tr.t(category_key(*c)),
                current: self.on(&format!("/issues/{}", c.as_str()), true),
            })
            .collect()
    }
}

/// Minimal percent-encoding for query values built by handlers.
#[must_use]
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Exact UTC time of a millisecond instant.
#[must_use]
pub fn iso(ms: i64) -> String {
    removarr_reporting::timefmt::iso(ms)
}

/// Human-readable size (binary units, one decimal).
#[must_use]
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Signed sizes (observed deltas can be negative).
#[must_use]
pub fn signed_bytes(n: i64) -> String {
    if n < 0 {
        format!("-{}", bytes(n.unsigned_abs()))
    } else {
        bytes(n.unsigned_abs())
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn codes_become_snake_case_keys() {
        assert_eq!(snake("ScanIncomplete"), "scan_incomplete");
        assert_eq!(snake("ServiceUnavailable:sonarr"), "service_unavailable");
        assert_eq!(snake("Failed (Io)"), "failed");
        assert_eq!(snake("T4"), "t4");
        assert_eq!(snake("ERROR"), "error");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(signed_bytes(-2048), "-2.0 KiB");
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
