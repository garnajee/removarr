//! UI translations (architecture 12.4): `locales/en.json` and `locales/fr.json` are embedded at
//! compile time, flat dotted keys, with an English fallback per key. The locale comes from the
//! `removarr_locale` cookie (not part of the session), then `Accept-Language`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::OnceLock;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;

/// Name of the locale cookie.
pub const LOCALE_COOKIE: &str = "removarr_locale";

const EN_JSON: &str = include_str!("../../../locales/en.json");
const FR_JSON: &str = include_str!("../../../locales/fr.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    En,
    Fr,
}

impl Locale {
    pub const ALL: [Locale; 2] = [Locale::En, Locale::Fr];

    /// Exact code only (`en`, `fr`); anything else is refused.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "en" => Some(Locale::En),
            "fr" => Some(Locale::Fr),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::Fr => "fr",
        }
    }

    /// Cookie first, then the first supported `Accept-Language` tag, else English.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let from_cookie = headers
            .get_all(axum::http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| *k == LOCALE_COOKIE)
            .and_then(|(_, v)| Locale::parse(v.trim()));
        if let Some(l) = from_cookie {
            return l;
        }
        headers
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.split(',').find_map(|tag| {
                    let primary = tag.split(';').next()?.trim().split('-').next()?;
                    Locale::parse(&primary.to_ascii_lowercase())
                })
            })
            .unwrap_or_default()
    }
}

type Table = HashMap<String, String>;

fn parse(json: &str) -> Table {
    // The files are part of the binary and checked by a unit test; a broken file degrades to
    // the key itself rather than failing a request.
    serde_json::from_str(json).unwrap_or_default()
}

fn table(locale: Locale) -> &'static Table {
    static EN: OnceLock<Table> = OnceLock::new();
    static FR: OnceLock<Table> = OnceLock::new();
    match locale {
        Locale::En => EN.get_or_init(|| parse(EN_JSON)),
        Locale::Fr => FR.get_or_init(|| parse(FR_JSON)),
    }
}

/// Translator for one request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tr {
    pub locale: Locale,
}

impl Tr {
    #[must_use]
    pub fn new(locale: Locale) -> Self {
        Self { locale }
    }

    /// The translation, the English text, or the key itself (never empty, never a panic).
    #[must_use]
    pub fn t<'k>(&self, key: &'k str) -> &'k str {
        table(self.locale)
            .get(key)
            .or_else(|| table(Locale::En).get(key))
            .map_or(key, String::as_str)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Tr {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Tr::new(Locale::from_headers(&parts.headers)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn locale_files_parse_and_french_keys_exist_in_english() {
        let en: Table = serde_json::from_str(EN_JSON).unwrap();
        let fr: Table = serde_json::from_str(FR_JSON).unwrap();
        assert!(!en.is_empty());
        for k in fr.keys() {
            assert!(en.contains_key(k), "{k} is French-only");
        }
        for k in en.keys() {
            assert!(fr.contains_key(k), "{k} has no French translation");
        }
    }

    #[test]
    fn missing_keys_fall_back_to_english_then_to_the_key() {
        let fr = Tr::new(Locale::Fr);
        assert_eq!(Tr::new(Locale::En).t("nav.dashboard"), "Dashboard");
        assert_ne!(fr.t("nav.dashboard"), "nav.dashboard");
        assert_eq!(fr.t("no.such.key"), "no.such.key");
    }

    #[test]
    fn locale_comes_from_the_cookie_then_accept_language() {
        let mut h = HeaderMap::new();
        assert_eq!(Locale::from_headers(&h), Locale::En);
        h.insert(
            axum::http::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("de-DE, fr-FR;q=0.8"),
        );
        assert_eq!(Locale::from_headers(&h), Locale::Fr);
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("a=b; removarr_locale=en"),
        );
        assert_eq!(Locale::from_headers(&h), Locale::En);
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("removarr_locale=xx"),
        );
        assert_eq!(Locale::from_headers(&h), Locale::Fr);
    }
}
