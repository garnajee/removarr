//! Redaction of secrets, credentials, tracker parameters and sensitive headers for logs,
//! errors and reports. Over-redaction is accepted; leaking is not.

use url::Url;

use crate::secret::REDACTED;

const SENSITIVE_FRAGMENTS: [&str; 9] = [
    "pass", "pwd", "token", "secret", "key", "auth", "sig", "cookie", "session",
];
const SENSITIVE_EXACT: [&str; 3] = ["sid", "uid", "u"];

/// Query/header/field names whose values must not be logged.
#[must_use]
pub fn is_sensitive_key(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SENSITIVE_EXACT.contains(&n.as_str()) || SENSITIVE_FRAGMENTS.iter().any(|f| n.contains(f))
}

#[must_use]
pub fn is_sensitive_header(name: &str) -> bool {
    is_sensitive_key(name)
}

/// Header value safe to log: `[REDACTED]` for sensitive headers, the value otherwise
/// (still scrubbed with [`redact_text`]).
#[must_use]
pub fn redact_header(name: &str, value: &str) -> String {
    if is_sensitive_header(name) {
        REDACTED.to_string()
    } else {
        redact_text(value)
    }
}

/// Removes userinfo and fragment, masks values of sensitive query parameters.
#[must_use]
pub fn redact_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return redact_text(raw);
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if let Some(q) = url.query().map(str::to_string) {
        let masked: Vec<String> = q
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, _)) if is_sensitive_key(k) => format!("{k}={REDACTED}"),
                _ => pair.to_string(),
            })
            .collect();
        url.set_query(Some(&masked.join("&")));
    }
    url.to_string()
}

/// Tracker announce URLs carry the passkey in the path or query. Only scheme, host and port
/// are kept.
#[must_use]
pub fn redact_tracker_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) if u.host_str().is_some() => {
            let mut out = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default());
            if let Some(p) = u.port() {
                out.push_str(&format!(":{p}"));
            }
            if u.path() != "/" || u.query().is_some() {
                out.push('/');
                out.push_str(REDACTED);
            }
            out
        }
        _ => REDACTED.to_string(),
    }
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';' | ')' | '<' | '>')
}

fn mask_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn token_end(chars: &[char], mut j: usize) -> usize {
    while j < chars.len() && !is_delim(chars[j]) {
        j += 1;
    }
    j
}

/// Free-text scrubber: `key=value`, `key: value`, `"key":"value"` for sensitive keys,
/// `Bearer`/`Basic` tokens and `scheme://user:pass@host` userinfo.
#[must_use]
pub fn redact_text(input: &str) -> String {
    let masked = mask_userinfo(input);
    let chars: Vec<char> = masked.chars().collect();
    let mut out = String::with_capacity(masked.len());
    let mut i = 0;
    while i < chars.len() {
        if !is_key_char(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_key_char(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        out.push_str(&word);
        let lower = word.to_ascii_lowercase();
        if matches!(lower.as_str(), "bearer" | "basic" | "digest") {
            if i < chars.len() && chars[i] == ' ' {
                let j = token_end(&chars, i + 1);
                if j > i + 1 {
                    out.push(' ');
                    out.push_str(REDACTED);
                    i = j;
                }
            }
            continue;
        }
        if !is_sensitive_key(&word) {
            continue;
        }
        let mut j = i;
        if j < chars.len() && chars[j] == '"' {
            j += 1;
        }
        if j >= chars.len() || !matches!(chars[j], '=' | ':') {
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let quoted = j < chars.len() && matches!(chars[j], '"' | '\'');
        if quoted {
            j += 1;
        }
        let value_start = j;
        let mut end = j;
        if quoted {
            while end < chars.len() && !matches!(chars[end], '"' | '\'') {
                end += 1;
            }
        } else {
            end = token_end(&chars, j);
            let first: String = chars[value_start..end].iter().collect();
            if matches!(
                first.to_ascii_lowercase().as_str(),
                "bearer" | "basic" | "digest"
            ) && end < chars.len()
                && chars[end] == ' '
            {
                end = token_end(&chars, end + 1);
            }
        }
        if end > value_start {
            out.extend(&chars[i..value_start]);
            out.push_str(REDACTED);
            i = end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_userinfo_and_query() {
        let r = redact_url("http://user:hunter2@host:8080/api?apikey=abc123&x=1#frag");
        assert!(!r.contains("hunter2") && !r.contains("abc123") && !r.contains("user"));
        assert!(r.contains("x=1") && r.contains("host:8080"));
    }

    #[test]
    fn tracker_keeps_only_host() {
        let r = redact_tracker_url("https://tracker.example/abcdef0123456789/announce?passkey=zz");
        assert_eq!(r, "https://tracker.example/[REDACTED]");
        assert_eq!(redact_tracker_url("garbage"), REDACTED);
        assert_eq!(
            redact_tracker_url("http://t.example:6969/"),
            "http://t.example:6969"
        );
    }

    #[test]
    fn headers() {
        for h in [
            "Authorization",
            "cookie",
            "Set-Cookie",
            "X-Api-Key",
            "X-CSRF-Token",
        ] {
            assert_eq!(redact_header(h, "v"), REDACTED, "{h}");
        }
        assert_eq!(redact_header("Content-Type", "text/plain"), "text/plain");
    }

    #[test]
    fn text_patterns() {
        let r = redact_text("GET /x?api_key=SECRET1&a=2 failed; Authorization: Bearer TOK2 ok");
        assert!(!r.contains("SECRET1") && !r.contains("TOK2"), "{r}");
        assert!(r.contains("a=2"));
        let j = redact_text(r#"{"password": "p@ss w0rd", "name":"x"}"#);
        assert!(!j.contains("w0rd") && j.contains("name"), "{j}");
        let u = redact_text("connect to http://bob:pw@h/x");
        assert!(!u.contains("bob") && !u.contains("pw@"), "{u}");
    }
}
