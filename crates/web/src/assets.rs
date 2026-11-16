//! Embedded static assets served at `/static/{file}` (architecture 12.4): the pinned HTMX file
//! and the first-party stylesheet. Nothing is read from disk at run time and there is no
//! first-party JavaScript. The CSP allows `script-src 'self'`, so both are same-origin files.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// File name of the pinned HTMX build in `/static/`.
pub const HTMX_FILE: &str = "htmx-2.0.4.min.js";
/// SHA-256 (hex) of `static/vendor/htmx-2.0.4.min.js` (npm `htmx.org@2.0.4`, `dist/htmx.min.js`).
pub const HTMX_SHA256: &str = "e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447";
/// SHA-384 SRI of the same file, published in the htmx 2.0.4 installation docs.
pub const HTMX_SRI: &str =
    "sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+";
/// Stylesheet file name (revalidated on every load).
pub const CSS_FILE: &str = "app.css";

static HTMX: &[u8] = include_bytes!("../static/vendor/htmx-2.0.4.min.js");
static CSS: &[u8] = include_bytes!("../static/app.css");

/// The bytes and content type of an embedded asset.
#[must_use]
pub fn lookup(file: &str) -> Option<(&'static [u8], &'static str)> {
    match file {
        HTMX_FILE => Some((HTMX, "text/javascript; charset=utf-8")),
        CSS_FILE => Some((CSS, "text/css; charset=utf-8")),
        _ => None,
    }
}

/// `GET /static/{file}`: public, immutable for the versioned HTMX file, revalidated for the CSS.
pub async fn serve(Path(file): Path<String>) -> Response {
    let Some((bytes, ctype)) = lookup(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if file == HTMX_FILE {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=0, must-revalidate"
    };
    let mut r = bytes.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn embedded_htmx_matches_the_pinned_sha256() {
        let (bytes, _) = lookup(HTMX_FILE).unwrap();
        assert_eq!(hex(&Sha256::digest(bytes)), HTMX_SHA256);
        assert_eq!(bytes.len(), 50917);
        assert!(HTMX_SHA256.len() == 64 && HTMX_SRI.starts_with("sha384-"));
    }

    #[test]
    fn stylesheet_keeps_the_accessibility_rules() {
        let css = std::str::from_utf8(lookup(CSS_FILE).unwrap().0).unwrap();
        for needle in [
            "--tap: 2.75rem",
            ":focus-visible",
            "prefers-reduced-motion: reduce",
            "prefers-contrast: more",
            "forced-colors: active",
            "prefers-color-scheme: dark",
            "env(safe-area-inset-bottom",
        ] {
            assert!(css.contains(needle), "{needle}");
        }
        assert!(!css.contains("outline: none;") || css.contains("h1:focus { outline: none; }"));
        assert!(
            !css.contains("@import") && !css.contains("url(http"),
            "no external resource"
        );
    }

    #[test]
    fn unknown_files_are_not_served() {
        assert!(lookup("../Cargo.toml").is_none());
        assert!(lookup("app.js").is_none());
    }
}
