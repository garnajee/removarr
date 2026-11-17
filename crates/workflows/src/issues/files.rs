//! File classes: video, junk, subtitles (OQ-02, OQ-03) and release-folder structure (OQ-11).

use std::collections::BTreeSet;

use removarr_domain::path::RelPath;

use super::input::IssueSettings;

/// Lower-cased extension with its leading dot (`"a.MKV"` -> `".mkv"`). Dot files have none.
#[must_use]
pub fn extension(name: &str) -> Option<String> {
    let (base, ext) = name.rsplit_once('.')?;
    if base.is_empty() || ext.is_empty() {
        return None;
    }
    Some(format!(".{}", ext.to_ascii_lowercase()))
}

/// Name without its last extension (`"a.b.mkv"` -> `"a.b"`).
#[must_use]
pub fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() && !ext.is_empty() => base,
        _ => name,
    }
}

/// `.r00` to `.r99` (RAR volume parts).
fn is_rar_part(ext: &str) -> bool {
    let b = ext.as_bytes();
    b.len() == 4 && b[0] == b'.' && b[1] == b'r' && b[2].is_ascii_digit() && b[3].is_ascii_digit()
}

impl IssueSettings {
    #[must_use]
    pub fn is_video(&self, name: &str) -> bool {
        extension(name).is_some_and(|e| self.video_extensions.contains(&e))
    }

    #[must_use]
    pub fn is_subtitle(&self, name: &str) -> bool {
        extension(name).is_some_and(|e| self.subtitle_extensions.contains(&e))
    }

    /// Junk by extension: the configured list or a RAR part. Videos are never junk, whatever
    /// the configuration says.
    #[must_use]
    pub fn is_junk_extension(&self, name: &str) -> bool {
        if self.is_video(name) {
            return false;
        }
        extension(name).is_some_and(|e| self.junk_extensions.contains(&e) || is_rar_part(&e))
    }
}

/// Tags that may trail a subtitle stem: language codes (2-3 ASCII letters) and flags (OQ-03).
fn is_subtitle_tag(tag: &str) -> bool {
    let t = tag.to_ascii_lowercase();
    matches!(t.as_str(), "forced" | "sdh" | "cc" | "default" | "hi")
        || ((2..=3).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Candidate base names of a subtitle: the stem and every stem obtained by repeatedly
/// stripping one trailing `.tag` (language or flag). The caller compares them with video stems.
fn subtitle_bases(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = stem(name).to_string();
    out.push(cur.to_ascii_lowercase());
    while let Some((head, tag)) = cur.rsplit_once('.') {
        if head.is_empty() || !is_subtitle_tag(tag) {
            break;
        }
        cur = head.to_string();
        out.push(cur.to_ascii_lowercase());
    }
    out
}

/// OQ-03: a subtitle is an orphan unless one of its base names equals the stem of a video in the
/// same directory, or (subtitle inside a `Subs`/`Subtitles` directory) in the parent directory.
/// `videos_in` returns the lower-cased video stems of a directory.
pub fn is_orphan_subtitle(
    rel: &RelPath,
    settings: &IssueSettings,
    videos_in: impl Fn(&RelPath) -> BTreeSet<String>,
) -> bool {
    let Some(name) = rel.file_name() else {
        return false;
    };
    if !settings.is_subtitle(name) {
        return false;
    }
    let Some(dir) = rel.parent() else {
        return false;
    };
    let bases = subtitle_bases(name);
    let mut dirs = vec![dir.clone()];
    if dir
        .file_name()
        .is_some_and(|d| matches!(d.to_ascii_lowercase().as_str(), "subs" | "subtitles"))
        && let Some(parent) = dir.parent()
    {
        dirs.push(parent);
    }
    !dirs
        .iter()
        .any(|d| videos_in(d).iter().any(|v| bases.contains(v)))
}

/// Directory names that are parts of a release rather than the release itself.
#[must_use]
pub fn is_release_part_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sample"
            | "samples"
            | "subs"
            | "sub"
            | "subtitles"
            | "extras"
            | "extra"
            | "featurettes"
            | "proof"
            | "screens"
            | "screenshots"
    )
}

/// The release folder of a file: its parent, moved up past `Sample`/`Subs`/`Extras`-style
/// directories (CON-23: the release folder is one unit, never split into main file and
/// `Sample/`). `None` for a file directly under the root.
#[must_use]
pub fn release_dir(rel: &RelPath) -> Option<RelPath> {
    let mut dir = rel.parent()?;
    // Cut at the first release-part component: everything from there down belongs to the release.
    let comps: Vec<&str> = dir.components().collect();
    if let Some(i) = comps.iter().position(|c| is_release_part_dir(c)) {
        let mut cut = RelPath::root();
        for c in &comps[..i] {
            cut = cut.join(c).ok()?;
        }
        dir = cut;
    }
    if dir.is_root() { None } else { Some(dir) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn videos(
        items: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&RelPath) -> BTreeSet<String> {
        move |d: &RelPath| {
            items
                .iter()
                .filter(|(dir, _)| *dir == d.as_str())
                .map(|(_, v)| (*v).to_string())
                .collect()
        }
    }

    #[test]
    fn extensions_are_case_insensitive_and_dot_files_have_none() {
        assert_eq!(extension("A.MKV").as_deref(), Some(".mkv"));
        assert_eq!(extension(".hidden"), None);
        assert_eq!(extension("noext"), None);
        assert_eq!(stem("a.b.mkv"), "a.b");
    }

    #[test]
    fn junk_by_extension_includes_rar_parts_and_never_videos() {
        let mut s = IssueSettings::new((1, 1));
        assert!(s.is_junk_extension("readme.TXT"));
        assert!(s.is_junk_extension("x.r00"));
        assert!(s.is_junk_extension("x.r99"));
        assert!(!s.is_junk_extension("x.r100"));
        assert!(!s.is_junk_extension("movie.mkv"));
        s.junk_extensions.insert(".mkv".into());
        assert!(!s.is_junk_extension("movie.mkv"), "a video is never junk");
        assert!(
            !s.is_junk_extension("movie.srt"),
            "subtitles are not junk by extension"
        );
    }

    #[test]
    fn subtitle_is_orphan_without_a_video_of_the_same_base_name() {
        let s = IssueSettings::new((1, 1));
        let v = videos(&[("d", "movie")]);
        assert!(!is_orphan_subtitle(&rel("d/Movie.srt"), &s, &v));
        assert!(!is_orphan_subtitle(&rel("d/movie.en.srt"), &s, &v));
        assert!(!is_orphan_subtitle(&rel("d/movie.fr.forced.srt"), &s, &v));
        assert!(is_orphan_subtitle(&rel("d/other.srt"), &s, &v));
        assert!(
            is_orphan_subtitle(&rel("e/movie.srt"), &s, &v),
            "other directory"
        );
        assert!(
            !is_orphan_subtitle(&rel("d/movie.mkv"), &s, &v),
            "not a subtitle"
        );
    }

    #[test]
    fn subs_directory_looks_at_the_parent_directory() {
        let s = IssueSettings::new((1, 1));
        let v = videos(&[("d", "movie")]);
        assert!(!is_orphan_subtitle(&rel("d/Subs/movie.en.srt"), &s, &v));
        assert!(is_orphan_subtitle(&rel("d/Other/movie.en.srt"), &s, &v));
        assert!(is_orphan_subtitle(&rel("d/Subs/nope.srt"), &s, &v));
    }

    #[test]
    fn a_year_is_not_stripped_as_a_language_tag() {
        let s = IssueSettings::new((1, 1));
        let v = videos(&[("stray", "unrelated.film")]);
        assert!(
            is_orphan_subtitle(&rel("stray/Unrelated.Film.2010.srt"), &s, &v),
            "2010 is not a tag, so the base never reduces to a video stem"
        );
    }

    #[test]
    fn release_dir_skips_sample_and_subs_directories() {
        assert_eq!(release_dir(&rel("R/Sample/x.mkv")), Some(rel("R")));
        assert_eq!(release_dir(&rel("R/Subs/en/x.srt")), Some(rel("R")));
        assert_eq!(release_dir(&rel("R/x.mkv")), Some(rel("R")));
        assert_eq!(release_dir(&rel("x.mkv")), None);
        assert_eq!(release_dir(&rel("Sample/x.mkv")), None);
    }
}
