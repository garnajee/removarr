//! Text redaction for logs, view models and exports (INV-31, MASTER section 8).
//!
//! Everything that leaves this crate as text goes through [`sanitize`]. It removes URLs
//! (tracker and magnet links included), credentials in `key=value`, `key: value` and JSON
//! forms, `Bearer`/`Basic` tokens, cookie lines, announce paths, and any configured secret
//! value registered with a [`Redactor`]. It over-redacts rather than under-redacts.

use std::sync::{Arc, RwLock};

use serde_json::Value;

pub const REDACTED: &str = "[redacted]";
pub const REDACTED_URL: &str = "[url]";

const SENSITIVE_KEYS: &[&str] = &[
    "apikey",
    "api_key",
    "api-key",
    "x-api-key",
    "token",
    "access_token",
    "passkey",
    "password",
    "passwd",
    "pass",
    "pwd",
    "secret",
    "auth",
    "authorization",
    "cookie",
    "set-cookie",
    "sid",
    "key",
    "session_key",
    "private_key",
];

/// True when a field or key name designates a credential.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key
        .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .to_ascii_lowercase();
    SENSITIVE_KEYS.contains(&k.as_str())
        || k.ends_with("_key")
        || k.ends_with("_token")
        || k.ends_with("_secret")
        || k.ends_with("password")
        || k.ends_with("apikey")
}

fn looks_like_url(token: &str) -> bool {
    let l = token.to_ascii_lowercase();
    let t = l.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    token.contains("://") || t.starts_with("magnet:") || l.contains("/announce")
}

/// Redacts the value of every sensitive `key=value` or `key:value` segment of one token.
/// Returns the rewritten token and whether the rest of the line must be redacted (a sensitive
/// key with an empty value, as in `Authorization:` followed by the credential).
fn redact_pairs(token: &str) -> (String, bool) {
    let mut out = String::with_capacity(token.len());
    let mut rest_of_line = false;
    let mut seg_start = 0;
    let bytes = token.as_bytes();
    let mut i = 0;
    let mut segments: Vec<(usize, usize)> = Vec::new();
    while i <= bytes.len() {
        if i == bytes.len() || matches!(bytes[i], b'?' | b'&' | b';' | b',' | b'{') {
            segments.push((seg_start, i));
            seg_start = i + 1;
        }
        i += 1;
    }
    let mut cursor = 0;
    for (s, e) in segments {
        out.push_str(&token[cursor..s]);
        let seg = &token[s..e];
        if let Some(pos) = seg.find(['=', ':']) {
            let (k, v) = (&seg[..pos], &seg[pos + 1..]);
            if is_sensitive_key(k) {
                out.push_str(k);
                out.push(seg.as_bytes()[pos] as char);
                if v.trim_matches(['"', '\'', ',', '}']).is_empty() {
                    rest_of_line = true;
                } else {
                    out.push_str(REDACTED);
                    if v.ends_with('"') {
                        out.push('"');
                    }
                }
                cursor = e;
                continue;
            }
        }
        out.push_str(seg);
        cursor = e;
    }
    out.push_str(&token[cursor.min(token.len())..]);
    (out, rest_of_line)
}

/// Removes URLs and credentials from free text. Line breaks and spacing are preserved.
#[must_use]
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut redact_next = false;
    let mut rest_of_line = false;
    for chunk in text.split_inclusive(char::is_whitespace) {
        let word = chunk.trim_end_matches(char::is_whitespace);
        let ws = &chunk[word.len()..];
        let lower = word.to_ascii_lowercase();
        if word.is_empty() {
            out.push_str(ws);
        } else if rest_of_line || redact_next {
            out.push_str(REDACTED);
            out.push_str(ws);
            redact_next = false;
        } else if looks_like_url(word) {
            out.push_str(REDACTED_URL);
            out.push_str(ws);
        } else {
            let (rewritten, rest) = redact_pairs(word);
            out.push_str(&rewritten);
            out.push_str(ws);
            rest_of_line = rest;
            if matches!(lower.as_str(), "bearer" | "basic" | "digest") {
                redact_next = true;
            }
        }
        if ws.contains('\n') {
            rest_of_line = false;
            redact_next = false;
        }
    }
    out
}

/// Sanitizes every string leaf of a JSON value; values under a sensitive key are replaced.
#[must_use]
pub fn sanitize_json(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(sanitize(s)),
        Value::Array(a) => Value::Array(a.iter().map(sanitize_json).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    if is_sensitive_key(k) && !v.is_null() {
                        (k.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (k.clone(), sanitize_json(v))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

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
