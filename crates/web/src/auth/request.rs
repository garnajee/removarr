//! Request helpers of the guard: client address behind trusted proxies, same-origin check,
//! CSRF token extraction (header or form field), constant-time comparison, random tokens.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Uri, header};
use removarr_config::TrustedProxies;

use crate::forms::MAX_BODY_BYTES;

/// Header carrying the CSRF token (HTMX: `hx-headers` on `<body>`).
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Hidden form field carrying the CSRF token.
pub const CSRF_FIELD: &str = "csrf_token";

/// Client address: the TCP peer, or the right-most untrusted `X-Forwarded-For` entry when the
/// peer is a configured trusted proxy (`TrustedProxies::client_ip`). Without connection info
/// (never in production) every such request shares the `0.0.0.0` bucket.
#[must_use]
pub fn client_ip(req: &Request, proxies: &TrustedProxies) -> IpAddr {
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    };
    // A non-UTF-8 header line becomes an unparsable entry, which stops the walk at the last
    // trusted hop (never a spoofed address).
    let xff: Vec<&str> = req
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .map(|v| v.to_str().unwrap_or(""))
        .collect();
    proxies.client_ip(peer.ip(), &xff)
}

/// `true` when the TCP peer is a configured trusted proxy.
#[must_use]
pub fn peer_is_trusted(req: &Request, proxies: &TrustedProxies) -> bool {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(p)| proxies.is_trusted(p.ip()))
}

/// Authority (`host[:port]`) of an absolute `http(s)` URL, lower-cased.
fn url_authority(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..end];
    (!auth.is_empty() && !auth.contains('@')).then(|| auth.to_ascii_lowercase())
}

/// The host this request was addressed to: a single-valued `X-Forwarded-Host` from a trusted
/// proxy, else `Host`, else the URI authority (HTTP/2).
fn request_host(headers: &HeaderMap, uri: &Uri, trusted_peer: bool) -> Option<String> {
    if trusted_peer {
        let mut fwd = headers.get_all("x-forwarded-host").iter();
        if let (Some(v), None) = (fwd.next(), fwd.next()) {
            let v = v.to_str().ok()?.trim();
            return (!v.is_empty() && !v.contains(',')).then(|| v.to_ascii_lowercase());
        }
    }
    match headers.get(header::HOST) {
        Some(h) => h.to_str().ok().map(|h| h.trim().to_ascii_lowercase()),
        None => uri.authority().map(|a| a.as_str().to_ascii_lowercase()),
    }
}

/// `Origin` (or, when absent, `Referer`) must name the request host. `Origin: null`, a missing
/// pair, or anything unparsable is refused (fail closed). The scheme is not compared, so TLS
/// may terminate at a reverse proxy.
#[must_use]
pub fn same_origin(headers: &HeaderMap, uri: &Uri, trusted_peer: bool) -> bool {
    let Some(host) = request_host(headers, uri, trusted_peer) else {
        return false;
    };
    let source = match headers.get(header::ORIGIN) {
        Some(o) => o.to_str().ok().and_then(url_authority),
        None => headers
            .get(header::REFERER)
            .and_then(|r| r.to_str().ok())
            .and_then(url_authority),
    };
    source.is_some_and(|s| s == host)
}

fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';').next().is_some_and(|m| {
                m.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        })
}

/// The presented CSRF token: the `X-CSRF-Token` header when present (no fallback), else the
/// `csrf_token` field of an urlencoded body. The body is buffered (bounded by the request body
/// limit) and the request is rebuilt with the same bytes for the handler.
pub async fn presented_token(req: Request) -> (Request, Option<String>) {
    if let Some(v) = req.headers().get(CSRF_HEADER) {
        let t = v.to_str().unwrap_or("").to_string();
        return (req, Some(t));
    }
    if !is_form(req.headers()) {
        return (req, None);
    }
    let (parts, body) = req.into_parts();
    match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => {
            let token = form_value(&bytes, CSRF_FIELD).ok().flatten();
            (Request::from_parts(parts, Body::from(bytes)), token)
        }
        Err(_) => (Request::from_parts(parts, Body::empty()), None),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// An urlencoded body that is ambiguous (repeated field) or badly encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadForm;

fn decode_component(raw: &[u8]) -> Result<String, BadForm> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = raw.get(i + 1).copied().and_then(hex_val).ok_or(BadForm)?;
                let lo = raw.get(i + 2).copied().and_then(hex_val).ok_or(BadForm)?;
                out.push((hi << 4) | lo);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| BadForm)
}

/// The single value of `name` in an urlencoded body. `Err` when the field is repeated or a
/// component is badly encoded (ambiguous input is refused).
pub fn form_value(body: &[u8], name: &str) -> Result<Option<String>, BadForm> {
    let mut found = None;
    for pair in body.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.iter().position(|b| *b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &pair[pair.len()..]),
        };
        if decode_component(k)? == name {
            if found.is_some() {
                return Err(BadForm);
            }
            found = Some(decode_component(v)?);
        }
    }
    Ok(found)
}

/// Comparison whose time depends only on the lengths.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// Lower-case hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Exactly 32 bytes from 64 hex characters.
#[must_use]
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (hex_val(b[2 * i])? << 4) | hex_val(b[2 * i + 1])?;
    }
    Some(out)
}

/// 32 bytes from the operating system's CSPRNG. `None` when it is unavailable (callers fail
/// closed).
#[must_use]
pub fn random32() -> Option<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).ok().map(|()| b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn uri() -> Uri {
        Uri::from_static("/scan")
    }

    #[test]
    fn same_origin_accepts_only_the_request_host() {
        let ok = |p: &[(&'static str, &str)]| same_origin(&headers(p), &uri(), false);
        assert!(ok(&[("host", "nas:8012"), ("origin", "http://nas:8012")]));
        assert!(ok(&[("host", "NAS:8012"), ("origin", "https://nas:8012")]));
        assert!(ok(&[
            ("host", "nas:8012"),
            ("referer", "http://nas:8012/plans/1?x=1")
        ]));
        assert!(!ok(&[("host", "nas:8012"), ("origin", "http://evil.test")]));
        assert!(!ok(&[("host", "nas:8012"), ("origin", "http://nas:8013")]));
        assert!(!ok(&[("host", "nas:8012"), ("origin", "null")]));
        assert!(!ok(&[("host", "nas:8012")]), "no Origin and no Referer");
        assert!(!ok(&[("origin", "http://nas:8012")]), "no host");
        // Origin present and wrong: Referer is not consulted.
        assert!(!ok(&[
            ("host", "nas:8012"),
            ("origin", "http://evil.test"),
            ("referer", "http://nas:8012/")
        ]));
        assert!(!ok(&[
            ("host", "nas"),
            ("referer", "http://nas@evil.test/")
        ]));
        assert!(!ok(&[("host", "nas"), ("origin", "ftp://nas")]));
    }

    #[test]
    fn forwarded_host_is_used_only_from_a_trusted_peer() {
        let h = headers(&[
            ("host", "removarr:8080"),
            ("x-forwarded-host", "media.example.test"),
            ("origin", "https://media.example.test"),
        ]);
        assert!(same_origin(&h, &uri(), true));
        assert!(!same_origin(&h, &uri(), false));
        let two = headers(&[
            ("host", "removarr:8080"),
            ("x-forwarded-host", "a.test, media.example.test"),
            ("origin", "https://media.example.test"),
        ]);
        assert!(!same_origin(&two, &uri(), true));
    }

    #[test]
    fn form_values_are_decoded_and_ambiguity_is_refused() {
        assert_eq!(
            form_value(b"a=1&csrf_token=ab%20c+d&b=2", "csrf_token"),
            Ok(Some("ab c d".into()))
        );
        assert_eq!(form_value(b"a=1", "csrf_token"), Ok(None));
        assert_eq!(
            form_value(b"csrf_token", "csrf_token"),
            Ok(Some(String::new()))
        );
        assert_eq!(
            form_value(b"csrf%5Ftoken=x", "csrf_token"),
            Ok(Some("x".into()))
        );
        assert!(form_value(b"csrf_token=a&csrf_token=b", "csrf_token").is_err());
        assert!(form_value(b"csrf_token=%zz", "csrf_token").is_err());
        assert!(
            form_value(b"csrf_token=%ff", "csrf_token").is_err(),
            "not UTF-8"
        );
    }

    #[test]
    fn constant_time_equality_and_hex() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
        let r = random32().unwrap();
        assert_eq!(unhex32(&hex(&r)), Some(r));
        assert_eq!(unhex32("zz"), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
        assert_ne!(random32(), random32());
    }
}
