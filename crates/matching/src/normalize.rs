//! Name normalization (DR-08, CON-16).
//!
//! Two different normalizations exist on purpose:
//! - [`ascii_fold`] feeds the scene/P2P parser: NFKD, lower case, ASCII only (legacy
//!   `ascii_text`). Non-ASCII letters disappear, which is fine for parsing tags.
//! - [`name_key`] is the name half of the name+size index: NFKD, lower case, combining marks
//!   removed, every other character kept. Precomposed and decomposed forms (`é` vs `e` + U+0301,
//!   common on macOS-created files) give the same key, but non-Latin names never collapse into an
//!   empty key.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// NFKD + lower case + ASCII only (legacy `ascii_text`).
#[must_use]
pub fn ascii_fold(value: &str) -> String {
    value
        .nfkd()
        .flat_map(char::to_lowercase)
        .filter(char::is_ascii)
        .collect()
}

/// Key of a file name for the name+size index. Equal keys mean "same name", never same content.
#[must_use]
pub fn name_key(file_name: &str) -> String {
    file_name
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// File name without its extension. The extension is stripped only when it looks like one
/// (1 to 5 ASCII alphanumerics, not all digits), so `Movie.2024` keeps its year.
#[must_use]
pub fn stem(file_name: &str) -> &str {
    match file_name.rsplit_once('.') {
        Some((base, ext))
            if !base.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && !ext.chars().all(|c| c.is_ascii_digit()) =>
        {
            base
        }
        _ => file_name,
    }
}

/// Lower-case, dot-prefixed extension of a file name (`".mkv"`), if any.
#[must_use]
pub fn extension(file_name: &str) -> Option<String> {
    let (base, ext) = file_name.rsplit_once('.')?;
    if base.is_empty() || ext.is_empty() {
        return None;
    }
    Some(format!(".{}", ext.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fold_matches_legacy_ascii_text() {
        assert_eq!(ascii_fold("Amélie.2001.MULTi"), "amelie.2001.multi");
        assert_eq!(ascii_fold("Ｆｕｌｌ"), "full");
        assert_eq!(ascii_fold("進撃の巨人"), "");
    }

    #[test]
    fn name_key_unifies_unicode_forms_and_case_but_keeps_non_latin_names() {
        let precomposed = "Am\u{e9}lie.mkv";
        let decomposed = "Ame\u{301}lie.MKV";
        assert_eq!(name_key(precomposed), name_key(decomposed));
        assert_eq!(name_key(precomposed), "amelie.mkv");
        assert_ne!(name_key("進撃の巨人.mkv"), name_key("鬼滅の刃.mkv"));
        assert_ne!(name_key("a.mkv"), name_key("b.mkv"));
    }

    #[test]
    fn stem_and_extension() {
        assert_eq!(stem("Movie.2024.mkv"), "Movie.2024");
        assert_eq!(stem("Movie.2024"), "Movie.2024");
        assert_eq!(stem("Movie.Name"), "Movie");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("Show - 01 [ABCD1234].mkv"), "Show - 01 [ABCD1234]");
        assert_eq!(extension("A.MKV").as_deref(), Some(".mkv"));
        assert_eq!(extension("noext"), None);
        assert_eq!(extension(".mkv"), None);
    }
}
