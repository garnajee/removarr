//! Text redaction shared by every crate that persists or publishes text (INV-31, MASTER
//! section 8). It lives in the lowest crate so storage, jobs and reporting use one rule.
//!
//! [`sanitize`] removes URLs (tracker and magnet links included), credentials in `key=value`,
//! `key: value` and JSON forms, `Bearer`/`Basic` tokens, cookie lines and announce paths. It
//! over-redacts rather than under-redacts.

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_credential_forms_are_removed() {
        let s = sanitize(
            "GET http://tracker.example/announce?passkey=abc failed Authorization: Bearer tok\ndone password=pw cookie: sid=zzz",
        );
        for leaked in ["tracker", "abc", "tok", "pw", "zzz"] {
            assert!(!s.contains(leaked), "{leaked} in {s}");
        }
        assert!(s.contains("failed") && s.contains("done") && s.contains('\n'));
    }
}
