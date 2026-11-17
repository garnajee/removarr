//! Bounded service-path to application-path mappings (architecture section 11).
//!
//! Algorithm: lexically normalize the service path (reject relative paths, `.`, `..`, NUL;
//! collapse `//`), take the longest rule prefix on component boundaries, replace only that
//! prefix, then classify against the configured roots. Reverse mapping uses the same rules.
//! Physical confinement (no-follow directory handles) is enforced later by `removarr-fs`.

use std::collections::BTreeMap;

use removarr_domain::path::RootedPath;
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::MappedPath;
use serde::Serialize;

use crate::error::Problem;
use crate::lexpath::{self, is_within, normalize_abs};
use crate::roots::{Roots, reject_symlinks_below_root};

/// One mapping entry: the service's view of a directory and where the application sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MapRule {
    pub service_prefix: String,
    pub app_path: String,
}

#[must_use]
pub fn env_name(service: ServiceKind) -> &'static str {
    match service {
        ServiceKind::Qbittorrent => "REMOVARR_QBIT_PATH_MAP",
        ServiceKind::Sonarr => "REMOVARR_SONARR_PATH_MAP",
        ServiceKind::Radarr => "REMOVARR_RADARR_PATH_MAP",
        ServiceKind::Jellyfin => "REMOVARR_JELLYFIN_PATH_MAP",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMapper {
    roots: Roots,
    rules: BTreeMap<ServiceKind, Vec<MapRule>>,
    external: Vec<String>,
}

fn bad_chars(s: &str) -> bool {
    s.contains(',') || s.contains('=')
}

impl PathMapper {
    /// Validates and normalizes `raw_rules` (service prefix, app path) per service, and the
    /// qBittorrent `external_prefixes`. Returns every problem found.
    pub fn new(
        roots: &Roots,
        raw_rules: &BTreeMap<ServiceKind, Vec<(String, String)>>,
        external_prefixes: &[String],
    ) -> Result<Self, Vec<Problem>> {
        let mut problems = Vec::new();
        let mut rules: BTreeMap<ServiceKind, Vec<MapRule>> = BTreeMap::new();

        for (service, entries) in raw_rules {
            let key = format!("path_mappings.{}", service.as_str());
            let env = Some(env_name(*service));
            let mut ok: Vec<MapRule> = Vec::new();
            for (i, (sp, ap)) in entries.iter().enumerate() {
                let k = format!("{key}[{i}]");
                if bad_chars(sp) || bad_chars(ap) {
                    problems.push(Problem::invalid(
                        k,
                        env,
                        "paths must not contain `,` or `=`",
                    ));
                    continue;
                }
                let sp = match normalize_abs(sp) {
                    Ok(v) => v,
                    Err(e) => {
                        problems.push(Problem::invalid(
                            k,
                            env,
                            format!("service prefix: {}", e.reason()),
                        ));
                        continue;
                    }
                };
                let ap = match normalize_abs(ap) {
                    Ok(v) => v,
                    Err(e) => {
                        problems.push(Problem::invalid(
                            k,
                            env,
                            format!("app path: {}", e.reason()),
                        ));
                        continue;
                    }
                };
                if ap == "/" {
                    problems.push(Problem::invalid(
                        k,
                        env,
                        "app path cannot be the filesystem root",
                    ));
                    continue;
                }
                let inside = roots.locate(&ap).is_some();
                if !inside && !roots.is_ancestor_of_any(&ap) {
                    problems.push(Problem::invalid(
                        k,
                        env,
                        "app path is outside every configured root",
                    ));
                    continue;
                }
                if let Err(why) = reject_symlinks_below_root(roots, &ap) {
                    problems.push(Problem::invalid(k, env, why));
                    continue;
                }
                ok.push(MapRule {
                    service_prefix: sp,
                    app_path: ap,
                });
            }
            for i in 0..ok.len() {
                for j in (i + 1)..ok.len() {
                    if let Some(why) = conflict(&ok[i], &ok[j]) {
                        problems.push(Problem::invalid(format!("{key}[{j}]"), env, why));
                    }
                }
            }
            rules.insert(*service, ok);
        }

        let mut external = Vec::new();
        for (i, p) in external_prefixes.iter().enumerate() {
            let k = format!("services.qbittorrent.external_prefixes[{i}]");
            let env = Some("REMOVARR_QBIT_EXTERNAL_PREFIXES");
            if bad_chars(p) {
                problems.push(Problem::invalid(
                    k,
                    env,
                    "paths must not contain `,` or `=`",
                ));
                continue;
            }
            match normalize_abs(p) {
                Ok(n) if n != "/" => external.push(n),
                Ok(_) => problems.push(Problem::invalid(k, env, "prefix cannot be `/`")),
                Err(e) => problems.push(Problem::invalid(k, env, e.reason())),
            }
        }

        if !problems.is_empty() {
            return Err(problems);
        }
        let mapper = Self {
            roots: roots.clone(),
            rules,
            external,
        };
        for (i, p) in mapper.external.iter().enumerate() {
            if matches!(
                mapper.to_app(ServiceKind::Qbittorrent, p),
                MappedPath::Mapped { .. }
            ) {
                problems.push(Problem::invalid(
                    format!("services.qbittorrent.external_prefixes[{i}]"),
                    Some("REMOVARR_QBIT_EXTERNAL_PREFIXES"),
                    "external prefix maps inside a root",
                ));
            }
        }
        if problems.is_empty() {
            Ok(mapper)
        } else {
            Err(problems)
        }
    }

    #[must_use]
    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    #[must_use]
    pub fn rules(&self, service: ServiceKind) -> &[MapRule] {
        self.rules.get(&service).map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn external_prefixes(&self) -> &[String] {
        &self.external
    }

    fn is_external(&self, normalized: &str) -> bool {
        self.external.iter().any(|p| is_within(normalized, p))
    }

    /// Service path -> classified application path.
    #[must_use]
    pub fn to_app(&self, service: ServiceKind, raw: &str) -> MappedPath {
        let raw_owned = raw.to_string();
        let norm = match normalize_abs(raw) {
            Ok(n) => n,
            Err(e) => {
                return MappedPath::Invalid {
                    raw: raw_owned,
                    reason: e.reason().to_string(),
                };
            }
        };
        let best = self
            .rules(service)
            .iter()
            .filter_map(|r| lexpath::strip_prefix(&norm, &r.service_prefix).map(|rest| (r, rest)))
            .max_by_key(|(r, _)| r.service_prefix.len());
        let Some((rule, rest)) = best else {
            return if service == ServiceKind::Qbittorrent && self.is_external(&norm) {
                MappedPath::DeclaredExternal { raw: raw_owned }
            } else {
                MappedPath::NoRule { raw: raw_owned }
            };
        };
        let app = lexpath::join(&rule.app_path, rest);
        match self.roots.locate(&app) {
            Some(path) => MappedPath::Mapped {
                raw: raw_owned,
                path,
            },
            None if service == ServiceKind::Qbittorrent && self.is_external(&norm) => {
                MappedPath::DeclaredExternal { raw: raw_owned }
            }
            None => MappedPath::OutsideRoots { raw: raw_owned },
        }
    }

    /// Application path -> the service's view. `None` when no rule covers it.
    #[must_use]
    pub fn to_service(&self, service: ServiceKind, path: &RootedPath) -> Option<String> {
        let app = self.roots.abs_path(path)?;
        let (rule, rest) = self
            .rules(service)
            .iter()
            .filter_map(|r| lexpath::strip_prefix(&app, &r.app_path).map(|rest| (r, rest)))
            .max_by_key(|(r, _)| r.app_path.len())?;
        Some(lexpath::join(&rule.service_prefix, rest))
    }
}

/// Rules of one service must stay reversible: equal service prefixes or equal app paths are
/// ambiguous, and nested app paths need the matching nested service prefixes.
fn conflict(a: &MapRule, b: &MapRule) -> Option<&'static str> {
    if a.service_prefix == b.service_prefix {
        return Some("duplicate service prefix");
    }
    if a.app_path == b.app_path {
        return Some("two prefixes map to the same app path (ambiguous reverse mapping)");
    }
    let nested = |outer: &MapRule, inner: &MapRule| -> bool {
        match lexpath::strip_prefix(&inner.app_path, &outer.app_path) {
            Some(app_rest) => lexpath::strip_prefix(&inner.service_prefix, &outer.service_prefix)
                .is_some_and(|svc_rest| svc_rest == app_rest),
            None => true,
        }
    };
    if (is_within(&b.app_path, &a.app_path) && !nested(a, b))
        || (is_within(&a.app_path, &b.app_path) && !nested(b, a))
    {
        return Some("overlapping app paths with inconsistent service prefixes");
    }
    None
}
