#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::symlink;

use common::*;
use proptest::prelude::*;
use removarr_config::{PathMapper, RootPaths, Roots, normalize_abs};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::session::ServiceKind::{self, *};
use removarr_domain::torrent::MappedPath;

fn roots(f: &Fixture) -> Roots {
    Roots::validate(&RootPaths {
        completed: Some(f.p("completed")),
        incomplete: Some(f.p("incomplete")),
        media: vec![f.p("media1"), f.p("media2")],
    })
    .unwrap()
}

fn rules(list: &[(&str, String)]) -> BTreeMap<ServiceKind, Vec<(String, String)>> {
    let mut m = BTreeMap::new();
    m.insert(
        Qbittorrent,
        list.iter()
            .map(|(s, a)| ((*s).to_string(), a.clone()))
            .collect(),
    );
    m
}

fn std_mapper(f: &Fixture) -> PathMapper {
    PathMapper::new(
        &roots(f),
        &rules(&[
            ("/dl", f.p("completed")),
            ("/dl/inc", f.p("incomplete")),
            ("/m1", f.p("media1")),
            ("/m2", f.p("media2")),
        ]),
        &["/external".to_string()],
    )
    .unwrap()
}

#[test]
fn classification() {
    let f = Fixture::new();
    let m = std_mapper(&f);
    let rooted = |root: RootId, rel: &str| RootedPath::new(root, RelPath::new(rel).unwrap());
    assert_eq!(
        m.to_app(Qbittorrent, "/dl//Show/ep.mkv"),
        MappedPath::Mapped {
            raw: "/dl//Show/ep.mkv".into(),
            path: rooted(RootId::completed(), "Show/ep.mkv")
        }
    );
    // longest prefix wins
    assert_eq!(
        m.to_app(Qbittorrent, "/dl/inc/x").as_rooted().unwrap().root,
        RootId::incomplete()
    );
    assert!(matches!(
        m.to_app(Qbittorrent, "/dl"),
        MappedPath::Mapped { .. }
    ));
    // ambiguous string prefix: /dl2 is not under /dl
    assert!(matches!(
        m.to_app(Qbittorrent, "/dl2/x"),
        MappedPath::NoRule { .. }
    ));
    assert!(matches!(
        m.to_app(Qbittorrent, "/other"),
        MappedPath::NoRule { .. }
    ));
    assert!(matches!(
        m.to_app(Qbittorrent, "/external/a"),
        MappedPath::DeclaredExternal { .. }
    ));
    assert!(matches!(
        m.to_app(Qbittorrent, "/externalx"),
        MappedPath::NoRule { .. }
    ));
    // no rule for other services: identity is never assumed
    assert!(matches!(
        m.to_app(Sonarr, "/dl/x"),
        MappedPath::NoRule { .. }
    ));
}

#[test]
fn traversal_and_garbage_are_invalid() {
    let f = Fixture::new();
    let m = std_mapper(&f);
    for raw in [
        "/dl/../etc/passwd",
        "/dl/./x",
        "relative/path",
        "",
        "/dl/a\0b",
        "C:\\x",
    ] {
        assert!(
            matches!(m.to_app(Qbittorrent, raw), MappedPath::Invalid { .. }),
            "{raw:?}"
        );
    }
}

#[test]
fn rules_outside_roots_or_malformed_are_rejected() {
    let f = Fixture::new();
    let r = roots(&f);
    let bad = |list: &[(&str, String)]| PathMapper::new(&r, &rules(list), &[]).is_err();
    assert!(bad(&[("/dl", "/etc".to_string())]));
    assert!(bad(&[("/dl", f.p("completed/../../etc"))]));
    assert!(bad(&[("dl", f.p("completed"))]));
    assert!(bad(&[("/dl", "/".to_string())]));
    assert!(bad(&[
        ("/dl", f.p("completed")),
        ("/dl", f.p("incomplete"))
    ]));
    assert!(bad(&[("/a", f.p("completed")), ("/b", f.p("completed"))]));
    // sibling string prefix of a root is outside it
    assert!(bad(&[("/dl", format!("{}2", f.p("completed")))]));
    // ancestor of the roots is allowed
    assert!(!bad(&[("/data", f.base.display().to_string())]));
    // nested app paths need nested service prefixes
    assert!(bad(&[
        ("/a", f.p("completed")),
        ("/b", f.p("completed/sub"))
    ]));
    assert!(!bad(&[
        ("/a", f.p("completed")),
        ("/a/sub", f.p("completed/sub"))
    ]));
}

#[test]
fn ancestor_rule_still_confines_to_roots() {
    let f = Fixture::new();
    let m = PathMapper::new(
        &roots(&f),
        &rules(&[("/data", f.base.display().to_string())]),
        &[],
    )
    .unwrap();
    assert!(matches!(
        m.to_app(Qbittorrent, "/data/completed/x"),
        MappedPath::Mapped { .. }
    ));
    assert!(matches!(
        m.to_app(Qbittorrent, "/data/data/db.sqlite"),
        MappedPath::OutsideRoots { .. }
    ));
    assert!(matches!(
        m.to_app(Qbittorrent, "/data/../x"),
        MappedPath::Invalid { .. }
    ));
}

#[test]
fn symlink_inside_root_makes_the_rule_invalid() {
    let f = Fixture::new();
    symlink("/", f.base.join("completed/escape")).unwrap();
    let r = roots(&f);
    let res = PathMapper::new(&r, &rules(&[("/dl", f.p("completed/escape"))]), &[]);
    assert!(res.is_err());
}

#[test]
fn external_prefix_cannot_shadow_a_root() {
    let f = Fixture::new();
    let r = roots(&f);
    let res = PathMapper::new(
        &r,
        &rules(&[("/dl", f.p("completed"))]),
        &["/dl/sub".to_string()],
    );
    assert!(res.is_err());
    assert!(PathMapper::new(&r, &rules(&[]), &["relative".to_string()]).is_err());
}

#[test]
fn reverse_mapping() {
    let f = Fixture::new();
    let m = std_mapper(&f);
    let p = RootedPath::new(RootId::incomplete(), RelPath::new("a/b").unwrap());
    assert_eq!(m.to_service(Qbittorrent, &p).unwrap(), "/dl/inc/a/b");
    assert!(m.to_service(Sonarr, &p).is_none());
    let unknown = RootedPath::new(RootId::new("media-00000000").unwrap(), RelPath::root());
    assert!(m.to_service(Qbittorrent, &unknown).is_none());
}

#[test]
fn roots_locate_is_component_bounded() {
    let f = Fixture::new();
    let r = roots(&f);
    assert!(r.locate(&format!("{}/x", f.p("completed"))).is_some());
    assert!(r.locate(&format!("{}2/x", f.p("completed"))).is_none());
    assert!(r.locate(&format!("{}/../etc", f.p("completed"))).is_none());
}

fn raw_path() -> impl Strategy<Value = String> {
    let seg = prop_oneof![
        3 => "[a-z0-9]{1,4}",
        1 => Just("..".to_string()),
        1 => Just(".".to_string()),
        1 => Just(String::new()),
        1 => Just("dl".to_string()),
        1 => Just("inc".to_string()),
        1 => Just("m1".to_string()),
        1 => Just("external".to_string()),
    ];
    (prop::bool::weighted(0.9), prop::collection::vec(seg, 0..7)).prop_map(|(abs, v)| {
        let j = v.join("/");
        if abs { format!("/{j}") } else { j }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn confinement_and_round_trip(raw in raw_path()) {
        let f = Fixture::new();
        let m = std_mapper(&f);
        let res = m.to_app(Qbittorrent, &raw);
        prop_assert_eq!(res.raw(), raw.as_str());
        let has_dots = raw.split('/').any(|c| c == ".." || c == ".");
        if has_dots || !raw.starts_with('/') {
            let invalid = matches!(res, MappedPath::Invalid { .. });
            prop_assert!(invalid, "expected Invalid");
        }
        if let MappedPath::Mapped { path, .. } = &res {
            prop_assert!(!has_dots);
            // inside a configured root, no dot components in the relative part
            let abs = m.roots().abs_path(path).unwrap();
            let inside = m.roots().all().any(|r| abs == r.path || abs.starts_with(&format!("{}/", r.path)));
            prop_assert!(inside, "outside roots");
            prop_assert!(path.rel.components().all(|c| c != ".." && c != "." && !c.is_empty()));
            // round trip back to the service view
            prop_assert_eq!(m.to_service(Qbittorrent, path).unwrap(), normalize_abs(&raw).unwrap());
        }
    }

    #[test]
    fn rooted_paths_round_trip(root in 0usize..4, rel in prop::collection::vec("[a-z0-9_ -][a-z0-9_. -]{0,6}", 0..5)) {
        let f = Fixture::new();
        let m = std_mapper(&f);
        let ids = [RootId::completed(), RootId::incomplete(),
            m.roots().media()[0].id.clone(), m.roots().media()[1].id.clone()];
        let rooted = RootedPath::new(ids[root].clone(), RelPath::new(rel.join("/")).unwrap());
        let svc = m.to_service(Qbittorrent, &rooted).unwrap();
        match m.to_app(Qbittorrent, &svc) {
            MappedPath::Mapped { path, .. } => prop_assert_eq!(path, rooted),
            other => prop_assert!(false, "{:?}", other),
        }
    }

    /// Any accepted rule set is reversible and confined.
    #[test]
    fn accepted_rule_sets_are_reversible(
        picks in prop::collection::vec((0usize..6, 0usize..6), 1..4),
        probes in prop::collection::vec(prop::collection::vec(0usize..6, 0..4), 1..6),
    ) {
        let f = Fixture::new();
        let svc_prefixes = ["/a", "/a/b", "/a/b/c", "/x", "/x/y", "/"];
        let apps = [f.p("completed"), f.p("completed/sub"), f.p("incomplete"),
            f.base.display().to_string(), f.p("media1"), f.p("media1/sub/deep")];
        let list: Vec<(&str, String)> = picks.iter().map(|(s, a)| (svc_prefixes[*s], apps[*a].clone())).collect();
        let Ok(m) = PathMapper::new(&roots(&f), &rules(&list), &[]) else { return Ok(()); };
        let names = ["a", "b", "c", "x", "y", "sub", "deep", "completed", "incomplete", "media1", "q"];
        for probe in probes {
            let raw = format!("/{}", probe.iter().map(|i| names[*i]).collect::<Vec<_>>().join("/"));
            if let MappedPath::Mapped { path, .. } = m.to_app(Qbittorrent, &raw) {
                prop_assert_eq!(m.to_service(Qbittorrent, &path).unwrap(), normalize_abs(&raw).unwrap(), "rules {:?}", list);
            }
        }
    }
}
