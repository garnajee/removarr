//! Configured roots. Nothing is assumed: every root is named explicitly, canonical, distinct
//! and non-nested. This module is the only place in `config` that touches the filesystem, and
//! it only calls `canonicalize` and `metadata` on configured paths.

use std::path::{Path, PathBuf};

use removarr_domain::path::{RelPath, RootId, RootKind, RootedPath};
use serde::Serialize;

use crate::error::Problem;
use crate::lexpath::{self, is_within, normalize_abs};

pub const ENV_COMPLETED: &str = "REMOVARR_COMPLETED_ROOT";
pub const ENV_INCOMPLETE: &str = "REMOVARR_INCOMPLETE_ROOT";
pub const ENV_MEDIA: &str = "REMOVARR_MEDIA_ROOTS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Root {
    pub id: RootId,
    /// Canonical absolute path, UTF-8, no trailing slash.
    pub path: String,
}

impl Root {
    #[must_use]
    pub fn kind(&self) -> RootKind {
        self.id.kind()
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.path)
    }
}

/// Raw configured values, before validation.
#[derive(Debug, Clone, Default)]
pub struct RootPaths {
    pub completed: Option<String>,
    pub incomplete: Option<String>,
    pub media: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Roots {
    completed: Root,
    incomplete: Option<Root>,
    media: Vec<Root>,
}

fn check_one(key: &str, env: &'static str, raw: &str, problems: &mut Vec<Problem>) -> Option<Root> {
    let norm = match normalize_abs(raw) {
        Ok(n) => n,
        Err(e) => {
            problems.push(Problem::invalid(key, Some(env), e.reason()));
            return None;
        }
    };
    if norm == "/" {
        problems.push(Problem::invalid(
            key,
            Some(env),
            "the filesystem root cannot be a root",
        ));
        return None;
    }
    let canonical = match std::fs::canonicalize(&norm) {
        Ok(c) => c,
        Err(_) => {
            problems.push(Problem::invalid(key, Some(env), "path does not exist"));
            return None;
        }
    };
    if canonical != Path::new(&norm) {
        problems.push(Problem::invalid(
            key,
            Some(env),
            "path must be canonical (no symlink component)",
        ));
        return None;
    }
    match std::fs::metadata(&canonical) {
        Ok(m) if m.is_dir() => {}
        _ => {
            problems.push(Problem::invalid(key, Some(env), "path is not a directory"));
            return None;
        }
    }
    Some(Root {
        // Placeholder id; the caller assigns the final one.
        id: RootId::media(&norm),
        path: norm,
    })
}

impl Roots {
    /// Validates every configured root. All problems are collected.
    pub fn validate(input: &RootPaths) -> Result<Self, Vec<Problem>> {
        let mut problems = Vec::new();

        let completed = match input.completed.as_deref() {
            None => {
                problems.push(Problem::missing("roots.completed", Some(ENV_COMPLETED)));
                None
            }
            Some(p) => {
                check_one("roots.completed", ENV_COMPLETED, p, &mut problems).map(|mut r| {
                    r.id = RootId::completed();
                    r
                })
            }
        };
        let incomplete = input.incomplete.as_deref().and_then(|p| {
            check_one("roots.incomplete", ENV_INCOMPLETE, p, &mut problems).map(|mut r| {
                r.id = RootId::incomplete();
                r
            })
        });
        if input.media.is_empty() {
            problems.push(Problem::missing("roots.media", Some(ENV_MEDIA)));
        }
        let mut media = Vec::new();
        for (i, p) in input.media.iter().enumerate() {
            let key = format!("roots.media[{i}]");
            if let Some(r) = check_one(&key, ENV_MEDIA, p, &mut problems) {
                media.push(r);
            }
        }

        let mut all: Vec<(String, &Root)> = Vec::new();
        if let Some(r) = &completed {
            all.push(("roots.completed".into(), r));
        }
        if let Some(r) = &incomplete {
            all.push(("roots.incomplete".into(), r));
        }
        for (i, r) in media.iter().enumerate() {
            all.push((format!("roots.media[{i}]"), r));
        }
        for (i, (ka, a)) in all.iter().enumerate() {
            for (kb, b) in &all[i + 1..] {
                if a.path == b.path {
                    problems.push(Problem::invalid(
                        format!("{ka} / {kb}"),
                        None,
                        "roots must be distinct (identical paths)",
                    ));
                } else if is_within(&a.path, &b.path) || is_within(&b.path, &a.path) {
                    problems.push(Problem::invalid(
                        format!("{ka} / {kb}"),
                        None,
                        "roots must not be nested (ambiguous overlap)",
                    ));
                } else if a.id == b.id {
                    problems.push(Problem::invalid(
                        format!("{ka} / {kb}"),
                        None,
                        "root identifiers collide",
                    ));
                }
            }
        }

        match (problems.is_empty(), completed) {
            (true, Some(completed)) => Ok(Self {
                completed,
                incomplete,
                media,
            }),
            _ => Err(problems),
        }
    }

    #[must_use]
    pub fn completed(&self) -> &Root {
        &self.completed
    }

    #[must_use]
    pub fn incomplete(&self) -> Option<&Root> {
        self.incomplete.as_ref()
    }

    #[must_use]
    pub fn media(&self) -> &[Root] {
        &self.media
    }

    /// Completed and incomplete (the torrent download side).
    #[must_use]
    pub fn download_roots(&self) -> Vec<&Root> {
        std::iter::once(&self.completed)
            .chain(self.incomplete.iter())
            .collect()
    }

    #[must_use]
    pub fn media_roots(&self) -> Vec<&Root> {
        self.media.iter().collect()
    }

    pub fn all(&self) -> impl Iterator<Item = &Root> {
        std::iter::once(&self.completed)
            .chain(self.incomplete.iter())
            .chain(self.media.iter())
    }

    #[must_use]
    pub fn get(&self, id: &RootId) -> Option<&Root> {
        self.all().find(|r| &r.id == id)
    }

    /// Classifies a normalized absolute application path. Roots never nest, so at most one
    /// root matches. Purely lexical: physical confinement is `removarr-fs`'s job.
    #[must_use]
    pub fn locate(&self, abs_path: &str) -> Option<RootedPath> {
        let norm = normalize_abs(abs_path).ok()?;
        self.all().find_map(|r| {
            let rest = lexpath::strip_prefix(&norm, &r.path)?;
            let rel = RelPath::new(rest).ok()?;
            Some(RootedPath::new(r.id.clone(), rel))
        })
    }

    /// Absolute application path of a rooted path, or `None` for an unknown root.
    #[must_use]
    pub fn abs_path(&self, p: &RootedPath) -> Option<String> {
        let root = self.get(&p.root)?;
        Some(lexpath::join(&root.path, p.rel.as_str()))
    }

    /// Whether `candidate` is lexically an ancestor of (or equal to) at least one root.
    #[must_use]
    pub fn is_ancestor_of_any(&self, candidate: &str) -> bool {
        self.all().any(|r| is_within(&r.path, candidate))
    }
}

/// Data directory: exists, is a directory, canonical, outside every root and containing none.
pub fn validate_data_dir(raw: &str, roots: Option<&Roots>) -> Result<PathBuf, Vec<Problem>> {
    const KEY: &str = "server.data_dir";
    const ENV: &str = "REMOVARR_DATA_DIR";
    let mut dummy = Vec::new();
    let Some(r) = check_one(KEY, ENV, raw, &mut dummy) else {
        return Err(dummy);
    };
    if let Some(roots) = roots {
        for root in roots.all() {
            if is_within(&r.path, &root.path) || is_within(&root.path, &r.path) {
                return Err(vec![Problem::invalid(
                    KEY,
                    Some(ENV),
                    "data directory must be outside every root and contain none",
                )]);
            }
        }
    }
    Ok(PathBuf::from(r.path))
}

/// Checks that a mapping's application path, when it lies inside a root, crosses no symlink.
/// Only paths below an already-canonical configured root are inspected. A missing component ends
/// the walk (nothing to follow).
pub fn reject_symlinks_below_root(roots: &Roots, abs_path: &str) -> Result<(), &'static str> {
    let Some(rooted) = roots.locate(abs_path) else {
        return Ok(());
    };
    let Some(root) = roots.get(&rooted.root) else {
        return Ok(());
    };
    let mut cur = PathBuf::from(&root.path);
    for c in rooted.rel.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err("path crosses a symlink"),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}
