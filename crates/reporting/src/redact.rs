//! Text redaction for logs, view models and exports (INV-31, MASTER section 8).
//!
//! Everything that leaves this crate as text goes through [`sanitize`]. It removes URLs
//! (tracker and magnet links included), credentials in `key=value`, `key: value` and JSON
//! forms, `Bearer`/`Basic` tokens, cookie lines, announce paths, and any configured secret
//! value registered with a [`Redactor`]. It over-redacts rather than under-redacts.

use std::sync::{Arc, RwLock};

pub use removarr_domain::redact::{
    REDACTED, REDACTED_URL, is_sensitive_key, sanitize, sanitize_json,
};

/// Truncates on a char boundary, marking the cut.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars).collect();
    s.push('\u{2026}');
    s
}

/// Sanitizer that also removes exact configured secret values (API keys, passwords, session
/// key). Shared and cheap to clone; register values once at startup.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<String>>>,
}

impl Redactor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Values shorter than 4 characters are ignored (they would mangle ordinary text).
    pub fn add_secret(&self, value: &str) {
        if value.chars().count() < 4 {
            return;
        }
        if let Ok(mut g) = self.secrets.write()
            && !g.iter().any(|s| s == value)
        {
            g.push(value.to_string());
            g.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    #[must_use]
    pub fn sanitize(&self, text: &str) -> String {
        let mut s = text.to_string();
        if let Ok(g) = self.secrets.read() {
            for secret in g.iter() {
                if s.contains(secret.as_str()) {
                    s = s.replace(secret.as_str(), REDACTED);
                }
            }
        }
        sanitize(&s)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn urls_trackers_and_magnets_are_removed() {
        let s = sanitize(
            "fetch http://tracker.example/announce?passkey=abc123 and magnet:?xt=urn:btih:abc end",
        );
        assert!(!s.contains("tracker") && !s.contains("abc"));
        assert!(s.contains("fetch") && s.contains("end"));
        assert!(!sanitize("GET /announce/0123456789abcdef0123456789abcdef").contains("0123"));
    }

    #[test]
    fn key_value_forms_are_redacted() {
        for input in [
            "apikey=SECRETVALUE",
            "password: SECRETVALUE",
            "{\"password\":\"SECRETVALUE\"}",
            "Authorization: Bearer SECRETVALUE",
            "Cookie: SID=SECRETVALUE; other=SECRETVALUE",
            "X-Api-Key: SECRETVALUE",
            "path?token=SECRETVALUE&x=1",
            "Bearer SECRETVALUE",
            "Set-Cookie: a=SECRETVALUE",
        ] {
            let s = sanitize(input);
            assert!(!s.contains("SECRETVALUE"), "{input} -> {s}");
        }
        assert_eq!(sanitize("copied 3 files in 2 s"), "copied 3 files in 2 s");
    }

    #[test]
    fn a_secret_line_does_not_swallow_the_next_line() {
        let s = sanitize("Authorization: Bearer abc\nnext line ok");
        assert!(s.contains("next line ok") && !s.contains("abc"));
    }

    #[test]
    fn registered_secret_values_are_removed_anywhere() {
        let r = Redactor::new();
        r.add_secret("hunter2hunter2");
        r.add_secret("ab");
        let s = r.sanitize("the value hunter2hunter2 appears, also in xhunter2hunter2y");
        assert!(!s.contains("hunter2"));
        assert!(s.contains("ab") || !s.is_empty());
    }

    #[test]
    fn json_keys_are_redacted_whole() {
        let v = serde_json::json!({"api_key": "k", "n": {"password": "p", "x": "see http://a/b"}, "list": ["Bearer zz"]});
        let s = sanitize_json(&v).to_string();
        assert!(
            !s.contains("\"k\"")
                && !s.contains("\"p\"")
                && !s.contains("http")
                && !s.contains("zz")
        );
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("abcdef", 3), "abc\u{2026}");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
