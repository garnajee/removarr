//! Scene/P2P release-name parsing (DR-08, CONTEXTE §3.3 level 3).
//!
//! The parser extracts identity fields (title, year, episode numbering) and technical tags
//! (resolution, codec, source, languages, editions, release group). Every field is explanatory:
//! identity fields feed candidate ranking, tags feed blockers and the UI. Nothing here ranks
//! quality (INV-11, INV-12): there is deliberately no ordering of resolutions, codecs or groups.
//!
//! Differences from the legacy `parse_scene_name` (recorded in the 07 handoff):
//! - multi-episode files (`S01E01E02`, `S01E01-E03`, `S01E01-03`), `1x02` and anime absolute
//!   numbering (`Title - 25`) are recognized;
//! - the year is the last year before the first technical marker (so `Blade.Runner.2049.2017`
//!   has year 2017 and title `blade runner 2049`), the legacy took the first one;
//! - tags are read only after the title, so a title word such as `French` in
//!   `The.French.Connection.1971` is not a language;
//! - a leading `[Group]` and a trailing `-GROUP` are captured as the release group.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::normalize::{ascii_fold, stem};

/// Season and episode numbers written in the name (aired or scene numbering, unknown which).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EpisodeNumbers {
    pub season: u16,
    /// Strictly increasing, never empty.
    pub episodes: Vec<u16>,
}

/// Parsed release name. All fields are lower-case ASCII.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedName {
    /// Every `[a-z0-9]+` token of the folded stem, space separated.
    pub normalized: String,
    pub title_tokens: Vec<String>,
    pub year: Option<u16>,
    pub episode: Option<EpisodeNumbers>,
    /// Absolute episode number (anime `Title - 25`), only when no `SxxEyy`/`1x02` form exists.
    pub absolute: Option<u32>,
    pub resolution: Option<String>,
    pub codec: Option<String>,
    pub source: Option<String>,
    /// Raw language tokens as written (`multi`, `french`, `vostfr`, ...).
    pub languages: BTreeSet<String>,
    /// Canonical edition markers (`extended`, `directorscut`, `openmatte`, ...).
    pub editions: BTreeSet<String>,
    pub group: Option<String>,
}

impl ParsedName {
    /// Title tokens joined with spaces.
    #[must_use]
    pub fn title(&self) -> String {
        self.title_tokens.join(" ")
    }
}

pub(crate) const RESOLUTION_MARKERS: &[&str] = &[
    "2160p", "1080p", "1080i", "720p", "576p", "480p", "4k", "uhd",
];
pub(crate) const CODEC_MARKERS: &[&str] = &[
    "x264", "x265", "h264", "h265", "hevc", "avc", "av1", "xvid", "vp9",
];
pub(crate) const SOURCE_MARKERS: &[&str] = &[
    "bluray", "bdrip", "brrip", "webrip", "webdl", "hdtv", "dvdrip", "remux",
];
pub(crate) const LANGUAGE_MARKERS: &[&str] = &[
    "french",
    "fr",
    "vf",
    "vff",
    "vfi",
    "vfq",
    "vf2",
    "vof",
    "truefrench",
    "vostfr",
    "subfrench",
    "english",
    "eng",
    "german",
    "ger",
    "spanish",
    "esp",
    "italian",
    "ita",
    "japanese",
    "jpn",
    "multi",
    "dual",
];
const EDITION_MARKERS: &[&str] = &[
    "extended",
    "imax",
    "uncut",
    "unrated",
    "theatrical",
    "remastered",
    "criterion",
    "fanedit",
    "openmatte",
    "directorscut",
    "finalcut",
    "redux",
];
/// Legacy `SCENE_MARKERS` plus the added language tokens. Used only by the title fallback.
const SCENE_MARKERS: &[&str] = &[
    "1080p",
    "1080i",
    "2160p",
    "720p",
    "576p",
    "480p",
    "4k",
    "uhd",
    "bluray",
    "blu",
    "ray",
    "bdrip",
    "brrip",
    "webrip",
    "web",
    "webdl",
    "dl",
    "hdtv",
    "dvdrip",
    "remux",
    "proper",
    "repack",
    "internal",
    "extended",
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "avc",
    "av1",
    "xvid",
    "hdr",
    "hdr10",
    "hdr10plus",
    "dv",
    "dolby",
    "vision",
    "sdr",
    "10bit",
    "aac",
    "ac3",
    "eac3",
    "ddp",
    "dts",
    "dtshd",
    "truehd",
    "atmos",
    "multi",
    "multisub",
    "dual",
    "audio",
    "vostfr",
    "subfrench",
    "french",
    "truefrench",
    "fr",
    "eng",
    "english",
    "vf",
    "vff",
    "vfi",
    "vfq",
    "vf2",
    "vof",
    "vo",
    "subs",
    "dubbed",
    "mkv",
    "readnfo",
    "limited",
    "unrated",
    "complete",
];
/// Extra source-like words that end a title (legacy `STRONG_TECHNICAL_MARKERS` additions).
const SOURCE_WORDS: &[&str] = &["web", "hdtv", "bluray", "bdrip", "brrip", "dvdrip", "remux"];

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| regex("[a-z0-9]+"));
static SXXEYY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"s(\d{1,2})[ ._-]*e(\d{1,3})"));
static NXNN_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(\d{1,2})x(\d{2,3})"));
static ABSOLUTE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"[ ._]-[ ._]*(\d{2,4})(?:v\d)?"));
static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:19|20)\d{2}"));

#[allow(clippy::expect_used)] // Constant patterns, covered by every parser test.
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid constant regex")
}

fn is_strong_marker(token: &str) -> bool {
    is_hard_marker(token) || LANGUAGE_MARKERS.contains(&token)
}

/// Technical markers that can never be part of a title (languages can, `The French Connection`).
fn is_hard_marker(token: &str) -> bool {
    RESOLUTION_MARKERS.contains(&token)
        || CODEC_MARKERS.contains(&token)
        || SOURCE_MARKERS.contains(&token)
        || SOURCE_WORDS.contains(&token)
}

struct Token<'a> {
    text: &'a str,
    start: usize,
}

fn tokens(raw: &str) -> Vec<Token<'_>> {
    TOKEN_RE
        .find_iter(raw)
        .map(|m| Token {
            text: m.as_str(),
            start: m.start(),
        })
        .collect()
}

fn prev_is_alnum(b: &[u8], start: usize) -> bool {
    start > 0 && b[start - 1].is_ascii_alphanumeric()
}

fn next_is_digit(b: &[u8], end: usize) -> bool {
    b.get(end).is_some_and(u8::is_ascii_digit)
}

fn find_sxxeyy(raw: &str) -> Option<(usize, EpisodeNumbers)> {
    let b = raw.as_bytes();
    for caps in SXXEYY_RE.captures_iter(raw) {
        let (Some(m), Some(s), Some(e)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        if prev_is_alnum(b, m.start()) || next_is_digit(b, m.end()) {
            continue;
        }
        let (Ok(season), Ok(first)) = (s.as_str().parse(), e.as_str().parse()) else {
            continue;
        };
        let episodes = continue_episodes(b, m.end(), first);
        return Some((m.start(), EpisodeNumbers { season, episodes }));
    }
    None
}

/// Multi-episode continuation after `SxxEyy`: `E02` (list), `-E03` or `-03` (range, at most 50).
fn continue_episodes(b: &[u8], mut i: usize, first: u16) -> Vec<u16> {
    let mut eps = vec![first];
    loop {
        let mut j = i;
        let mut hyphen = false;
        while j < b.len() && matches!(b[j], b' ' | b'.' | b'_' | b'-') {
            hyphen |= b[j] == b'-';
            j += 1;
        }
        let has_e = b.get(j) == Some(&b'e');
        if has_e {
            j += 1;
        } else if !hyphen {
            break;
        }
        let digits_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let n = j - digits_start;
        if n == 0 || n > 3 {
            break;
        }
        // `-720p`: digits glued to letters are not an episode range.
        if !has_e && b.get(j).is_some_and(u8::is_ascii_alphanumeric) {
            break;
        }
        let Ok(num) = std::str::from_utf8(&b[digits_start..j])
            .unwrap_or("")
            .parse::<u16>()
        else {
            break;
        };
        let last = *eps.last().unwrap_or(&first);
        if num <= last {
            break;
        }
        if hyphen {
            if num - last > 50 {
                break;
            }
            eps.extend(last + 1..=num);
        } else {
            eps.push(num);
        }
        i = j;
    }
    eps
}

fn find_nxnn(raw: &str) -> Option<(usize, EpisodeNumbers)> {
    let b = raw.as_bytes();
    for caps in NXNN_RE.captures_iter(raw) {
        let (Some(m), Some(s), Some(e)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        if prev_is_alnum(b, m.start()) || b.get(m.end()).is_some_and(u8::is_ascii_alphanumeric) {
            continue;
        }
        let (Ok(season), Ok(ep)) = (s.as_str().parse(), e.as_str().parse()) else {
            continue;
        };
        return Some((
            m.start(),
            EpisodeNumbers {
                season,
                episodes: vec![ep],
            },
        ));
    }
    None
}

fn find_absolute(raw: &str) -> Option<(usize, u32)> {
    let b = raw.as_bytes();
    for caps in ABSOLUTE_RE.captures_iter(raw) {
        let (Some(m), Some(n)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        if b.get(m.end()).is_some_and(u8::is_ascii_alphanumeric) {
            continue;
        }
        let Ok(value) = n.as_str().parse::<u32>() else {
            continue;
        };
        if n.as_str().len() == 4 && (1900..=2099).contains(&value) {
            continue; // `Title - 2019` is a year
        }
        return Some((m.start(), value));
    }
    None
}

fn find_years(raw: &str) -> Vec<(usize, u16)> {
    let b = raw.as_bytes();
    YEAR_RE
        .find_iter(raw)
        .filter(|m| {
            (m.start() == 0 || !b[m.start() - 1].is_ascii_digit()) && !next_is_digit(b, m.end())
        })
        .filter_map(|m| m.as_str().parse().ok().map(|y| (m.start(), y)))
        .collect()
}

fn valid_group(g: &str) -> bool {
    !g.is_empty()
        && g.len() <= 40
        && g.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        && !RESOLUTION_MARKERS.contains(&g)
}

fn suffix_group(raw: &str) -> Option<String> {
    let (_, seg) = raw.rsplit_once('-')?;
    let ok = !seg.is_empty()
        && seg.len() <= 24
        && seg.chars().all(|c| c.is_ascii_alphanumeric())
        && !seg.chars().all(|c| c.is_ascii_digit())
        && !SCENE_MARKERS.contains(&seg);
    ok.then(|| seg.to_string())
}

/// Parses a file name (with or without extension).
#[must_use]
pub fn parse_scene_name(file_name: &str) -> ParsedName {
    let full = ascii_fold(stem(file_name));
    let normalized = tokens(&full)
        .iter()
        .map(|t| t.text)
        .collect::<Vec<_>>()
        .join(" ");

    let mut raw = full.as_str();
    let mut group = None;
    if let Some(rest) = raw.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let g = rest[..end].trim();
        if valid_group(g) {
            group = Some(g.to_string());
        }
        raw = &rest[end + 1..];
    }
    if group.is_none() {
        group = suffix_group(raw);
    }

    let toks = tokens(raw);
    let (episode, absolute, ep_start) = match find_sxxeyy(raw).or_else(|| find_nxnn(raw)) {
        Some((start, ep)) => (Some(ep), None, Some(start)),
        None => match find_absolute(raw) {
            Some((start, n)) => (None, Some(n), Some(start)),
            None => (None, None, None),
        },
    };

    let years = find_years(raw);
    let hard_start = toks
        .iter()
        .find(|t| is_hard_marker(t.text))
        .map(|t| t.start);
    let limit = ep_start
        .unwrap_or(raw.len())
        .min(hard_start.unwrap_or(raw.len()));
    let eligible: Vec<(usize, u16)> = years.iter().copied().filter(|y| y.0 < limit).collect();
    let chosen = eligible
        .iter()
        .rev()
        .find(|y| y.0 > 0)
        .or_else(|| eligible.first())
        .copied();
    let year = chosen.or_else(|| years.first().copied()).map(|y| y.1);

    let boundary = match (ep_start, chosen.map(|y| y.0)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => toks
            .iter()
            .find(|t| is_strong_marker(t.text))
            .map(|t| t.start),
    };
    let title_raw = boundary.map_or(raw, |b| &raw[..b]);
    let mut title_tokens: Vec<String> = tokens(title_raw)
        .iter()
        .map(|t| t.text.to_string())
        .collect();
    if title_tokens.is_empty() {
        title_tokens = toks
            .iter()
            .map(|t| t.text)
            .filter(|t| {
                !SCENE_MARKERS.contains(t)
                    && (t.len() > 4 || !t.chars().all(|c| c.is_ascii_digit()))
                    && !is_episode_token(t)
            })
            .map(str::to_string)
            .collect();
    }

    let tags: Vec<&str> = toks
        .iter()
        .filter(|t| boundary.is_none_or(|b| t.start >= b))
        .map(|t| t.text)
        .collect();
    let resolution = tags
        .iter()
        .find(|t| RESOLUTION_MARKERS.contains(t))
        .map(|t| (*t).to_string());
    let codec = tags.iter().enumerate().find_map(|(i, t)| {
        if CODEC_MARKERS.contains(t) {
            Some((*t).to_string())
        } else if *t == "h" {
            match tags.get(i + 1) {
                Some(&n @ ("264" | "265")) => Some(format!("h{n}")),
                _ => None,
            }
        } else {
            None
        }
    });
    let source = tags
        .iter()
        .find(|t| SOURCE_MARKERS.contains(t))
        .map(|t| (*t).to_string())
        .or_else(|| {
            let web = tags.contains(&"web");
            if web && tags.contains(&"dl") {
                Some("web-dl".to_string())
            } else {
                web.then(|| "web".to_string())
            }
        });
    let languages = tags
        .iter()
        .filter(|t| LANGUAGE_MARKERS.contains(t))
        .map(|t| (*t).to_string())
        .collect();
    let mut editions: BTreeSet<String> = tags
        .iter()
        .filter(|t| EDITION_MARKERS.contains(t))
        .map(|t| (*t).to_string())
        .collect();
    let joined = format!(" {} ", tags.join(" "));
    for (phrase, canonical) in [
        (" director s cut ", "directorscut"),
        (" directors cut ", "directorscut"),
        (" open matte ", "openmatte"),
        (" final cut ", "finalcut"),
    ] {
        if joined.contains(phrase) {
            editions.insert(canonical.to_string());
        }
    }

    ParsedName {
        normalized,
        title_tokens,
        year,
        episode,
        absolute,
        resolution,
        codec,
        source,
        languages,
        editions,
        group,
    }
}

fn is_episode_token(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('s') else {
        return false;
    };
    let Some((s, e)) = rest.split_once('e') else {
        return false;
    };
    (1..=2).contains(&s.len())
        && (1..=3).contains(&e.len())
        && s.chars().all(|c| c.is_ascii_digit())
        && e.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(season: u16, episodes: &[u16]) -> Option<EpisodeNumbers> {
        Some(EpisodeNumbers {
            season,
            episodes: episodes.to_vec(),
        })
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// T-05 `test_scene_title_stops_before_identity_marker_and_ignores_release_group`.
    #[test]
    fn t05_title_stops_before_episode_marker_and_ignores_release_group() {
        let supply = parse_scene_name("When.They.See.Us.S01E01.MULTi.1080p.WEB.x264-AXWELL.mkv");
        let other = parse_scene_name("When.They.See.Us.S01E01.MULTi.1080p.WEB.x264-ARK01.mkv");
        assert_eq!(supply.title_tokens, ["when", "they", "see", "us"]);
        assert_eq!(supply.title_tokens, other.title_tokens);
        assert_eq!(supply.episode, ep(1, &[1]));
        assert_eq!(supply.group.as_deref(), Some("axwell"));
        assert_eq!(other.group.as_deref(), Some("ark01"));
        assert_eq!(supply.resolution.as_deref(), Some("1080p"));
        assert_eq!(supply.codec.as_deref(), Some("x264"));
        assert_eq!(supply.source.as_deref(), Some("web"));
        assert_eq!(supply.languages, set(&["multi"]));
    }

    #[test]
    fn legacy_fixture_names_parse_like_the_legacy_parser() {
        let c = parse_scene_name("Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv");
        let m = parse_scene_name("Charlie Movie (2019) MULTi 1080p WEB.mkv");
        assert_eq!(c.title_tokens, ["charlie", "movie"]);
        assert_eq!(c.title_tokens, m.title_tokens);
        assert_eq!((c.year, m.year), (Some(2019), Some(2019)));
        assert_eq!(c.codec.as_deref(), Some("h264"));
        assert_eq!(c.group.as_deref(), Some("grp"));
        assert_eq!(m.group, None);
        let d = parse_scene_name("Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv");
        assert_eq!(d.languages, set(&["french"]));
        assert_eq!(d.resolution.as_deref(), Some("720p"));
        let ex = parse_scene_name("The.Movie.2024.EXTENDED.MULTi.1080p.mkv");
        assert_eq!(ex.editions, set(&["extended"]));
        assert_eq!(ex.title_tokens, ["the", "movie"]);
        let web_dl = parse_scene_name("Show.S01E01.1080p.WEB-DL.DDP5.1.H.264-GRP.mkv");
        assert_eq!(web_dl.source.as_deref(), Some("web-dl"));
        assert_eq!(web_dl.codec.as_deref(), Some("h264"));
    }

    #[test]
    fn multi_episode_forms() {
        assert_eq!(
            parse_scene_name("Show.S01E01E02.mkv").episode,
            ep(1, &[1, 2])
        );
        assert_eq!(
            parse_scene_name("Show.S01E01-E03.mkv").episode,
            ep(1, &[1, 2, 3])
        );
        assert_eq!(
            parse_scene_name("Show.S01E01-03.720p.mkv").episode,
            ep(1, &[1, 2, 3])
        );
        assert_eq!(
            parse_scene_name("Show.S01E01E03.mkv").episode,
            ep(1, &[1, 3])
        );
        assert_eq!(parse_scene_name("Show S02 E05.mkv").episode, ep(2, &[5]));
        // Not ranges: a resolution after a hyphen, a title after a dash, a backward number.
        assert_eq!(
            parse_scene_name("Show.S01E05-720p.mkv").episode,
            ep(1, &[5])
        );
        assert_eq!(
            parse_scene_name("Show.S01E05-1080p.mkv").episode,
            ep(1, &[5])
        );
        assert_eq!(
            parse_scene_name("Show - S01E05 - Pilot.mkv").episode,
            ep(1, &[5])
        );
        assert_eq!(parse_scene_name("Show.S01E05-E02.mkv").episode, ep(1, &[5]));
        // Boundaries: glued to letters, or followed by a digit, is not an episode.
        assert_eq!(parse_scene_name("Mass01e01.mkv").episode, None);
        assert_eq!(parse_scene_name("Show.S01E1234.mkv").episode, None);
    }

    #[test]
    fn nxnn_and_absolute_numbering() {
        let x = parse_scene_name("Show.Name.1x02.HDTV.mkv");
        assert_eq!(x.episode, ep(1, &[2]));
        assert_eq!(x.title_tokens, ["show", "name"]);
        assert_eq!(parse_scene_name("Movie.1920x1080.mkv").episode, None);

        let a = parse_scene_name("[SubsPlease] Jujutsu Kaisen - 25 (1080p) [A1B2C3D4].mkv");
        assert_eq!(a.absolute, Some(25));
        assert_eq!(a.episode, None);
        assert_eq!(a.title_tokens, ["jujutsu", "kaisen"]);
        assert_eq!(a.group.as_deref(), Some("subsplease"));
        assert_eq!(a.resolution.as_deref(), Some("1080p"));
        assert_eq!(
            parse_scene_name("Show - 047v2 [720p].mkv").absolute,
            Some(47)
        );
        assert_eq!(parse_scene_name("Movie - 2019.mkv").absolute, None);
        assert_eq!(parse_scene_name("Movie Name - 720p.mkv").absolute, None);
        // SxxEyy wins over a dash number.
        let s = parse_scene_name("Show - S02E25 - 25.mkv");
        assert_eq!((s.episode, s.absolute), (ep(2, &[25]), None));
    }

    #[test]
    fn year_is_the_last_year_before_technical_markers() {
        let br = parse_scene_name("Blade.Runner.2049.2017.1080p.BluRay.x264-GRP.mkv");
        assert_eq!(
            (br.year, br.title()),
            (Some(2017), "blade runner 2049".into())
        );
        let so = parse_scene_name("2001.A.Space.Odyssey.1968.1080p.mkv");
        assert_eq!(
            (so.year, so.title()),
            (Some(1968), "2001 a space odyssey".into())
        );
        let w = parse_scene_name("1917.2019.MULTi.2160p.mkv");
        assert_eq!((w.year, w.title()), (Some(2019), "1917".into()));
        let after = parse_scene_name("Movie.1080p.2019.mkv");
        assert_eq!((after.year, after.title()), (Some(2019), "movie".into()));
        let ep_year = parse_scene_name("Show.S01E01.2019.mkv");
        assert_eq!((ep_year.year, ep_year.title()), (Some(2019), "show".into()));
        // Only a leading year: title falls back to the non-marker tokens.
        let lead = parse_scene_name("2012.1080p.mkv");
        assert_eq!(lead.year, Some(2012));
        assert!(lead.title_tokens.is_empty());
    }

    #[test]
    fn title_words_are_not_tags() {
        let f = parse_scene_name("The.French.Connection.1971.1080p.BluRay.x264-GRP.mkv");
        assert_eq!(f.title(), "the french connection");
        assert!(f.languages.is_empty());
        let it = parse_scene_name("The.Italian.Job.2003.MULTi.1080p.mkv");
        assert_eq!(it.languages, set(&["multi"]));
    }

    #[test]
    fn editions_languages_and_fallback_title() {
        let d = parse_scene_name("Movie.2010.Directors.Cut.TRUEFRENCH.1080p.mkv");
        assert_eq!(d.editions, set(&["directorscut"]));
        assert_eq!(d.languages, set(&["truefrench"]));
        let om = parse_scene_name("Movie.2010.Open.Matte.VOSTFR.mkv");
        assert_eq!(om.editions, set(&["openmatte"]));
        assert_eq!(om.languages, set(&["vostfr"]));
        let i = parse_scene_name("Movie.2010.IMAX.UNRATED.mkv");
        assert_eq!(i.editions, set(&["imax", "unrated"]));
        // No year and no episode: the title stops at the first technical marker.
        let n = parse_scene_name("Some.Documentary.1080p.WEB.mkv");
        assert_eq!(n.title(), "some documentary");
        assert_eq!(n.year, None);
        // Nothing but markers.
        assert!(parse_scene_name("1080p.x264.mkv").title_tokens.is_empty());
    }

    #[test]
    fn group_capture_rules() {
        assert_eq!(parse_scene_name("Movie.2010.1080p.WEB-DL.mkv").group, None);
        assert_eq!(parse_scene_name("Movie.2010.mkv").group, None);
        assert_eq!(parse_scene_name("Show - 25.mkv").group, None);
        assert_eq!(
            parse_scene_name("Movie.2010.1080p-FLUX.mkv")
                .group
                .as_deref(),
            Some("flux")
        );
    }
}
