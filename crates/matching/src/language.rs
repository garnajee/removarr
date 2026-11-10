//! Language rules: release-name language classes (DR-15) and ffprobe language tags (INV-37).

use std::collections::BTreeSet;

use crate::parse::ParsedName;

/// Conservative language classes of a release name and whether it is MULTI/DUAL (legacy
/// `language_profile`). `None` when the name carries no language token.
#[must_use]
pub fn language_profile(languages: &BTreeSet<String>) -> Option<(BTreeSet<&'static str>, bool)> {
    if languages.is_empty() {
        return None;
    }
    let has = |set: &[&str]| languages.iter().any(|l| set.contains(&l.as_str()));
    let wide = has(&["multi", "dual"]);
    let mut classes = BTreeSet::new();
    if has(&[
        "french",
        "fr",
        "vf",
        "vff",
        "vfi",
        "vfq",
        "vf2",
        "vof",
        "truefrench",
    ]) {
        classes.insert("fr");
    }
    if has(&["english", "eng"]) {
        classes.insert("en");
    }
    if has(&["vostfr", "subfrench"]) {
        classes.insert("fr_sub");
    }
    if has(&["german", "ger"]) {
        classes.insert("de");
    }
    if has(&["spanish", "esp"]) {
        classes.insert("es");
    }
    if has(&["italian", "ita"]) {
        classes.insert("it");
    }
    if has(&["japanese", "jpn"]) {
        classes.insert("ja");
    }
    if wide {
        classes.insert("multi");
    }
    Some((classes, wide))
}

/// Legacy `languages_are_compatible`: missing tags on either side are incompatible; a MULTI/DUAL
/// source needs a MULTI/DUAL replacement; a MULTI/DUAL replacement is accepted; otherwise the
/// source classes must be a non-empty subset of the replacement classes.
#[must_use]
pub fn languages_are_compatible(source: &ParsedName, replacement: &ParsedName) -> bool {
    let (Some((sc, sw)), Some((rc, rw))) = (
        language_profile(&source.languages),
        language_profile(&replacement.languages),
    ) else {
        return false;
    };
    if sw {
        return rw;
    }
    if rw {
        return true;
    }
    !sc.is_empty() && sc.is_subset(&rc)
}

/// A normalized ffprobe language tag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LangTag {
    /// ISO 639-1 code when known, else the lower-case tag as written.
    Known(String),
    /// Missing, `und`, `unk`, `mis` or `mul`: the real language is not known.
    Undetermined,
    /// `zxx`: no linguistic content (music or effects track).
    NoLinguisticContent,
}

/// Normalizes an ffprobe `language` tag (ISO 639-2/B, 639-2/T, 639-1, or a region form).
#[must_use]
pub fn normalize_tag(tag: Option<&str>) -> LangTag {
    let raw = tag.unwrap_or("").trim().to_ascii_lowercase();
    let primary = raw.split(['-', '_']).next().unwrap_or("");
    let code = match primary {
        "" | "und" | "unk" | "mis" | "mul" => return LangTag::Undetermined,
        "zxx" => return LangTag::NoLinguisticContent,
        "fr" | "fre" | "fra" | "french" => "fr",
        "en" | "eng" | "english" => "en",
        "de" | "ger" | "deu" => "de",
        "es" | "spa" => "es",
        "it" | "ita" => "it",
        "ja" | "jpn" => "ja",
        "pt" | "por" => "pt",
        "ru" | "rus" => "ru",
        "zh" | "chi" | "zho" => "zh",
        "ko" | "kor" => "ko",
        "nl" | "dut" | "nld" => "nl",
        "pl" | "pol" => "pl",
        "sv" | "swe" => "sv",
        "da" | "dan" => "da",
        "no" | "nor" | "nob" | "nno" | "nb" | "nn" => "no",
        "fi" | "fin" => "fi",
        "ar" | "ara" => "ar",
        "tr" | "tur" => "tr",
        "cs" | "cze" | "ces" => "cs",
        "hu" | "hun" => "hu",
        "el" | "gre" | "ell" => "el",
        "he" | "heb" => "he",
        "hi" | "hin" => "hi",
        "uk" | "ukr" => "uk",
        other => other,
    };
    LangTag::Known(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_scene_name;

    fn compat(a: &str, b: &str) -> bool {
        languages_are_compatible(&parse_scene_name(a), &parse_scene_name(b))
    }

    /// DR-15 legacy cases (`language_profile`, `languages_are_compatible`).
    #[test]
    fn dr15_release_language_compatibility() {
        // MULTI source needs MULTI replacement.
        assert!(compat("M.2024.MULTi.1080p.mkv", "M.2024.MULTi.2160p.mkv"));
        assert!(!compat("M.2024.MULTi.1080p.mkv", "M.2024.FRENCH.720p.mkv"));
        // A MULTI replacement is accepted for a single-language source (legacy fixture Delta).
        assert!(compat("M.2018.FRENCH.720p.mkv", "M.2018.MULTi.1080p.mkv"));
        // Missing tags are incompatible (legacy fixture Kilo sample).
        assert!(!compat("M.2012.1080p.mkv", "M.2012.MULTi.1080p.mkv"));
        assert!(!compat("M.2012.FRENCH.1080p.mkv", "M.2012.1080p.mkv"));
        // Subset rule.
        assert!(compat("M.2012.FRENCH.mkv", "M.2012.TRUEFRENCH.mkv"));
        assert!(!compat("M.2012.FRENCH.mkv", "M.2012.VOSTFR.mkv"));
        assert!(!compat("M.2012.ENGLISH.mkv", "M.2012.FRENCH.mkv"));
        assert!(compat("M.2012.DUAL.mkv", "M.2012.MULTi.mkv"));
    }

    #[test]
    fn ffprobe_tags_normalize_and_und_is_never_a_language() {
        assert_eq!(normalize_tag(Some("fre")), LangTag::Known("fr".into()));
        assert_eq!(normalize_tag(Some("FRA")), LangTag::Known("fr".into()));
        assert_eq!(normalize_tag(Some("fr-CA")), LangTag::Known("fr".into()));
        assert_eq!(normalize_tag(Some("eng")), LangTag::Known("en".into()));
        assert_eq!(normalize_tag(Some("tlh")), LangTag::Known("tlh".into()));
        for und in [
            None,
            Some(""),
            Some("und"),
            Some("UND"),
            Some("mul"),
            Some("unk"),
        ] {
            assert_eq!(normalize_tag(und), LangTag::Undetermined, "{und:?}");
        }
        assert_eq!(normalize_tag(Some("zxx")), LangTag::NoLinguisticContent);
    }
}
