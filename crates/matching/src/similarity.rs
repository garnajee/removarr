//! Identification score and bounded candidate ranking (DR-09, DR-10, INV-45).
//!
//! The score answers "is this probably the same title/episode?". It is never a quality score and
//! never proves equal content: it only decides which few pairs deserve a closer look.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::parse::{EpisodeNumbers, ParsedName};

/// One explainable component of an identification score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreReason {
    /// Both sides carry episode numbers and they differ: score forced to 0.
    EpisodesDiffer,
    /// No common title token (or an empty title): score forced to 0.
    NoCommonTitleToken,
    TitleEqual,
    /// Title similarity components, in thousandths.
    TitleSimilar {
        sequence_milli: u32,
        jaccard_milli: u32,
    },
    SizeRatio {
        milli: u32,
    },
    YearEqual {
        year: u16,
    },
    YearDiffers {
        a: u16,
        b: u16,
    },
    EpisodesEqual,
}

impl ScoreReason {
    /// Stable short code for persistence and logs.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            ScoreReason::EpisodesDiffer => "episodes_differ".into(),
            ScoreReason::NoCommonTitleToken => "no_common_title_token".into(),
            ScoreReason::TitleEqual => "title_equal".into(),
            ScoreReason::TitleSimilar {
                sequence_milli,
                jaccard_milli,
            } => format!("title_similar:seq={sequence_milli},jac={jaccard_milli}"),
            ScoreReason::SizeRatio { milli } => format!("size_ratio={milli}"),
            ScoreReason::YearEqual { year } => format!("year_equal={year}"),
            ScoreReason::YearDiffers { a, b } => format!("year_differs={a}/{b}"),
            ScoreReason::EpisodesEqual => "episodes_equal".into(),
        }
    }
}

/// Identification score in `[0, 1]` with its reasons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Similarity {
    pub score: f64,
    pub reasons: Vec<ScoreReason>,
}

impl Similarity {
    /// Score in thousandths (rounded), as stored in `IdentityProof::NameParse`.
    #[must_use]
    pub fn score_milli(&self) -> u32 {
        milli(self.score)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn milli(x: f64) -> u32 {
    (x.clamp(0.0, 1.0) * 1000.0).round() as u32
}

/// Episode relation between two parsed names.
fn episodes_relation(a: &ParsedName, b: &ParsedName) -> Option<bool> {
    if let (Some(x), Some(y)) = (&a.episode, &b.episode) {
        return Some(x == y);
    }
    if let (Some(x), Some(y)) = (a.absolute, b.absolute) {
        return Some(x == y);
    }
    None
}

/// Legacy `similarity_score`: `0.55*sequence + 0.25*jaccard + 0.10*size_ratio`, `+0.10` same year,
/// `-0.20` different year, `+0.15` same episode, clamped. Zero when episodes differ or when no
/// title token is shared.
#[must_use]
pub fn similarity(a: &ParsedName, a_size: u64, b: &ParsedName, b_size: u64) -> Similarity {
    let zero = |r| Similarity {
        score: 0.0,
        reasons: vec![r],
    };
    let episodes = episodes_relation(a, b);
    if episodes == Some(false) {
        return zero(ScoreReason::EpisodesDiffer);
    }
    let ta: BTreeSet<&str> = a.title_tokens.iter().map(String::as_str).collect();
    let tb: BTreeSet<&str> = b.title_tokens.iter().map(String::as_str).collect();
    let common = ta.intersection(&tb).count();
    if ta.is_empty() || tb.is_empty() || common == 0 {
        return zero(ScoreReason::NoCommonTitleToken);
    }
    let union = ta.union(&tb).count();
    #[allow(clippy::cast_precision_loss)]
    let jaccard = common as f64 / union as f64;
    let sequence = sequence_ratio(&a.title(), &b.title());
    let larger = a_size.max(b_size);
    #[allow(clippy::cast_precision_loss)]
    let size_ratio = if larger == 0 {
        1.0
    } else {
        a_size.min(b_size) as f64 / larger as f64
    };
    let mut reasons = Vec::new();
    if a.title_tokens == b.title_tokens {
        reasons.push(ScoreReason::TitleEqual);
    } else {
        reasons.push(ScoreReason::TitleSimilar {
            sequence_milli: milli(sequence),
            jaccard_milli: milli(jaccard),
        });
    }
    reasons.push(ScoreReason::SizeRatio {
        milli: milli(size_ratio),
    });
    let mut score = 0.55 * sequence + 0.25 * jaccard + 0.10 * size_ratio;
    if let (Some(ya), Some(yb)) = (a.year, b.year) {
        if ya == yb {
            score += 0.10;
            reasons.push(ScoreReason::YearEqual { year: ya });
        } else {
            score -= 0.20;
            reasons.push(ScoreReason::YearDiffers { a: ya, b: yb });
        }
    }
    if episodes == Some(true) {
        score += 0.15;
        reasons.push(ScoreReason::EpisodesEqual);
    }
    Similarity {
        score: score.clamp(0.0, 1.0),
        reasons,
    }
}

/// Python `difflib.SequenceMatcher(None, a, b).ratio()` without the autojunk heuristic (which
/// only applies to sequences of 200+ items; titles are shorter).
#[must_use]
pub fn sequence_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let mut b2j: BTreeMap<char, Vec<usize>> = BTreeMap::new();
    for (j, c) in b.iter().enumerate() {
        b2j.entry(*c).or_default().push(j);
    }
    let mut matched = 0usize;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(&a, &b2j, alo, ahi, blo, bhi);
        if k > 0 {
            matched += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let r = 2.0 * matched as f64 / total as f64;
    r
}

fn longest_match(
    a: &[char],
    b2j: &BTreeMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestk) = (alo, blo, 0usize);
    let mut j2len: BTreeMap<usize, usize> = BTreeMap::new();
    for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut next: BTreeMap<usize, usize> = BTreeMap::new();
        for &j in b2j.get(c).map_or(&[][..], Vec::as_slice) {
            if j < blo {
                continue;
            }
            if j >= bhi {
                break;
            }
            let k = j
                .checked_sub(1)
                .and_then(|p| j2len.get(&p))
                .copied()
                .unwrap_or(0)
                + 1;
            next.insert(j, k);
            if k > bestk {
                besti = i + 1 - k;
                bestj = j + 1 - k;
                bestk = k;
            }
        }
        j2len = next;
    }
    (besti, bestj, bestk)
}

/// A ranked target: index into the engine's records, score and reasons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranked {
    pub index: usize,
    pub similarity: Similarity,
}

/// One record of the target side of a similarity search.
#[derive(Debug, Clone)]
pub struct Record {
    pub key: String,
    pub size: u64,
    pub parsed: ParsedName,
}

/// Token/episode/year indexes over target names (legacy `SimilarityEngine`).
#[derive(Debug, Clone, Default)]
pub struct SimilarityEngine {
    records: Vec<Record>,
    by_token: BTreeMap<String, BTreeSet<usize>>,
    by_episode: BTreeMap<EpisodeNumbers, BTreeSet<usize>>,
    by_absolute: BTreeMap<u32, BTreeSet<usize>>,
    by_year: BTreeMap<u16, BTreeSet<usize>>,
}

impl SimilarityEngine {
    #[must_use]
    pub fn new(records: Vec<Record>) -> Self {
        let mut e = Self {
            records,
            ..Self::default()
        };
        for (i, r) in e.records.iter().enumerate() {
            for t in &r.parsed.title_tokens {
                e.by_token.entry(t.clone()).or_default().insert(i);
            }
            if let Some(ep) = &r.parsed.episode {
                e.by_episode.entry(ep.clone()).or_default().insert(i);
            }
            if let Some(abs) = r.parsed.absolute {
                e.by_absolute.entry(abs).or_default().insert(i);
            }
            if let Some(y) = r.parsed.year {
                e.by_year.entry(y).or_default().insert(i);
            }
        }
        e
    }

    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Legacy `find`: candidate pool from the indexes, keep `score >= threshold`, sort by
    /// (score desc, size difference asc, key asc), keep at most `limit`.
    #[must_use]
    pub fn find(
        &self,
        parsed: &ParsedName,
        size: u64,
        threshold: f64,
        limit: usize,
    ) -> Vec<Ranked> {
        let mut token_pool = BTreeSet::new();
        for t in &parsed.title_tokens {
            if let Some(s) = self.by_token.get(t) {
                token_pool.extend(s.iter().copied());
            }
        }
        let numbered = parsed
            .episode
            .as_ref()
            .map(|ep| self.by_episode.get(ep))
            .or_else(|| parsed.absolute.map(|a| self.by_absolute.get(&a)));
        let pool: BTreeSet<usize> = match (numbered, parsed.year) {
            (Some(ep_pool), _) => {
                let ep_pool = ep_pool.cloned().unwrap_or_default();
                if token_pool.is_empty() {
                    ep_pool
                } else {
                    token_pool.intersection(&ep_pool).copied().collect()
                }
            }
            (None, Some(year)) if !token_pool.is_empty() => {
                let same: BTreeSet<usize> = self
                    .by_year
                    .get(&year)
                    .map(|y| token_pool.intersection(y).copied().collect())
                    .unwrap_or_default();
                if same.is_empty() { token_pool } else { same }
            }
            _ => token_pool,
        };
        let mut scored: Vec<Ranked> = pool
            .into_iter()
            .filter_map(|index| {
                let r = &self.records[index];
                let similarity = similarity(parsed, size, &r.parsed, r.size);
                (similarity.score >= threshold).then_some(Ranked { index, similarity })
            })
            .collect();
        scored.sort_by(|x, y| {
            y.similarity
                .score
                .total_cmp(&x.similarity.score)
                .then_with(|| {
                    self.records[x.index]
                        .size
                        .abs_diff(size)
                        .cmp(&self.records[y.index].size.abs_diff(size))
                })
                .then_with(|| self.records[x.index].key.cmp(&self.records[y.index].key))
        });
        scored.truncate(limit);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_scene_name;

    fn rec(name: &str, size: u64) -> Record {
        Record {
            key: name.to_string(),
            size,
            parsed: parse_scene_name(name),
        }
    }

    #[test]
    fn sequence_ratio_matches_python_difflib() {
        // Values computed with CPython difflib.SequenceMatcher(None, a, b).ratio().
        let cases = [
            ("abcd", "bcde", 0.75),
            ("charlie movie", "charlie movie", 1.0),
            ("golf movie", "hotel movie", 0.761_904_761_904_762),
            ("movie name", "movie name returns", 0.714_285_714_285_714_3),
            ("", "", 1.0),
            ("abc", "", 0.0),
            ("example show", "other show", 0.545_454_545_454_545_4),
        ];
        for (a, b, want) in cases {
            let got = sequence_ratio(a, b);
            assert!((got - want).abs() < 1e-9, "{a:?} vs {b:?}: {got} != {want}");
        }
    }

    /// T-04 `test_similarity_uses_title_year_episode_and_size`.
    #[test]
    fn t04_same_show_and_episode_ranks_first_and_other_episode_is_excluded() {
        let engine = SimilarityEngine::new(vec![
            rec("Other.Show.S02E04.1080p.mkv", 9000),
            rec("Example.Show.S02E03.2160p.FRENCH.x265.mkv", 10000),
        ]);
        let src = parse_scene_name("Example.Show.S02E03.1080p.MULTI.x264.mkv");
        let found = engine.find(&src, 9000, 0.60, 3);
        assert_eq!(found.len(), 1, "the other episode is excluded");
        assert_eq!(
            engine.records()[found[0].index].key,
            "Example.Show.S02E03.2160p.FRENCH.x265.mkv"
        );
        assert!(
            found[0]
                .similarity
                .reasons
                .contains(&ScoreReason::EpisodesEqual)
        );
        // Score: 0.55 + 0.25 + 0.10 * 0.9 + 0.15, clamped to 1.
        assert_eq!(found[0].similarity.score_milli(), 1000);
    }

    #[test]
    fn legacy_fixture_scores() {
        let c = parse_scene_name("Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv");
        let cs = parse_scene_name("charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv");
        let m = parse_scene_name("Charlie Movie (2019) MULTi 1080p WEB.mkv");
        assert_eq!(similarity(&c, 98304, &m, 98304).score_milli(), 1000);
        assert_eq!(similarity(&cs, 8192, &m, 98304).score_milli(), 908);
        let d1 = parse_scene_name("Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv");
        let d2 = parse_scene_name("Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv");
        assert_eq!(similarity(&d1, 40960, &d2, 81920).score_milli(), 950);
        // Different titles sharing only "movie" with a different year stay below 0.60.
        let g = parse_scene_name("Golf.Movie.2016.1080p.WEB.x264-GRP.mkv");
        let h = parse_scene_name("Hotel Movie (2015).mkv");
        assert!(similarity(&g, 16384, &h, 16384).score < 0.60);
    }

    #[test]
    fn zero_rules_and_year_penalty() {
        let a = parse_scene_name("Show.S01E01.mkv");
        let b = parse_scene_name("Show.S01E02.mkv");
        let s = similarity(&a, 1, &b, 1);
        assert_eq!(
            (s.score, s.reasons.clone()),
            (0.0, vec![ScoreReason::EpisodesDiffer])
        );
        let multi = parse_scene_name("Show.S01E01E02.mkv");
        assert_eq!(
            similarity(&a, 1, &multi, 1).score,
            0.0,
            "different coverage"
        );
        let x = parse_scene_name("Alpha.2020.mkv");
        let y = parse_scene_name("Beta.2020.mkv");
        assert_eq!(
            similarity(&x, 1, &y, 1).reasons,
            vec![ScoreReason::NoCommonTitleToken]
        );
        let y1 = parse_scene_name("Movie.2020.mkv");
        let y2 = parse_scene_name("Movie.2021.mkv");
        let s = similarity(&y1, 10, &y2, 10);
        assert!((s.score - 0.70).abs() < 1e-9);
        assert!(
            s.reasons
                .contains(&ScoreReason::YearDiffers { a: 2020, b: 2021 })
        );
        let abs1 = parse_scene_name("Anime - 25.mkv");
        let abs2 = parse_scene_name("Anime - 26.mkv");
        assert_eq!(similarity(&abs1, 1, &abs2, 1).score, 0.0);
    }

    #[test]
    fn ranking_is_bounded_and_ties_break_on_size_then_key() {
        let engine = SimilarityEngine::new(vec![
            rec("Movie.2020.c.mkv", 130),
            rec("Movie.2020.a.mkv", 100),
            rec("Movie.2020.b.mkv", 100),
            rec("Movie.2020.d.mkv", 70),
        ]);
        let src = parse_scene_name("Movie.2020.mkv");
        let found = engine.find(&src, 100, 0.0, 3);
        let keys: Vec<&str> = found
            .iter()
            .map(|r| engine.records()[r.index].key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["Movie.2020.a.mkv", "Movie.2020.b.mkv", "Movie.2020.c.mkv"]
        );
        // Same year subset is preferred to the whole token pool.
        let engine =
            SimilarityEngine::new(vec![rec("Movie.2019.mkv", 1), rec("Movie.2020.mkv", 1)]);
        let found = engine.find(&src, 1, 0.0, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(engine.records()[found[0].index].key, "Movie.2020.mkv");
    }
}
