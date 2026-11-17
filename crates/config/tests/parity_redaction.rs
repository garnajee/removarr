//! Pass 2 gap tests: secrets never appear in Debug/Display/errors, redaction properties,
//! constant-time comparison semantics, and mapping edge cases (T-26 family).
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::collections::BTreeMap;

use common::*;
use proptest::prelude::*;
use removarr_config::{
    Config, PathMapper, REDACTED, RootPaths, Roots, Secret, redact_header, redact_text, redact_url,
};
use removarr_domain::session::ServiceKind::{Qbittorrent, Sonarr};
use removarr_domain::torrent::MappedPath;

#[test]
fn secret_never_prints_and_compares_exactly() {
    let s = Secret::new("DUMMY-SECRET-VALUE");
    for out in [format!("{s:?}"), format!("{s}"), format!("{s:#?}")] {
        assert!(!out.contains("DUMMY-SECRET-VALUE"), "{out}");
        assert!(out.contains(REDACTED));
    }
    assert!(s.ct_eq("DUMMY-SECRET-VALUE"));
    assert!(!s.ct_eq("DUMMY-SECRET-VALUE "), "longer input");
    assert!(!s.ct_eq("DUMMY-SECRET-VALUE"[..10].as_ref()), "prefix");
    assert!(!s.ct_eq("dummy-secret-value"), "case");
    assert!(!s.ct_eq(""));
    assert!(Secret::new("").is_empty() && Secret::new("").ct_eq(""));
}

#[test]
fn configuration_errors_never_echo_secret_values() {
    let f = Fixture::new();
    // Every invalid input below also carries a sentinel secret in a neighbouring variable.
    let mut e = f.env();
    set(&mut e, "REMOVARR_QBIT_URL", "not a url");
    set(&mut e, "REMOVARR_QBIT_PASSWORD", "SENTINEL-PW-1");
    set(
        &mut e,
        "REMOVARR_SONARR_URL",
        "http://u:SENTINEL-PW-2@sonarr.example",
    );
    set(&mut e, "REMOVARR_SONARR_API_KEY", "SENTINEL-KEY-3");
    set(&mut e, "REMOVARR_SESSION_KEY", "SENTINEL-SHORT-KEY-4");
    set(&mut e, "REMOVARR_PASSWORD_HASH", "SENTINEL-NOT-A-HASH-5");
    let err = Config::from_sources(None, &e).unwrap_err();
    assert!(!err.problems.is_empty());
    let shown = format!(
        "{err} {err:?} {}",
        err.problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|")
    );
    for s in [
        "SENTINEL-PW-1",
        "SENTINEL-PW-2",
        "SENTINEL-KEY-3",
        "SENTINEL-SHORT-KEY-4",
        "SENTINEL-NOT-A-HASH-5",
    ] {
        assert!(!shown.contains(s), "{s} leaked: {shown}");
    }
}

#[test]
fn redaction_covers_url_header_and_text_forms() {
    let u = redact_url("https://alice:DUMMY-PW@host.example/p?token=DUMMY-TOK&page=2#DUMMY-FRAG");
    for s in ["alice", "DUMMY-PW", "DUMMY-TOK", "DUMMY-FRAG"] {
        assert!(!u.contains(s), "{u}");
    }
    assert!(u.contains("page=2"));
    assert_eq!(redact_header("X-Api-Key", "DUMMY"), REDACTED);
    assert_eq!(redact_header("authorization", "Bearer DUMMY"), REDACTED);
    let t = redact_text("Authorization: Basic ZHVtbXk6ZHVtbXk= and cookie=SID123; ok=1");
    assert!(
        !t.contains("ZHVtbXk6ZHVtbXk=") && !t.contains("SID123"),
        "{t}"
    );
    assert!(t.contains("ok=1"));
    // Text without secrets is returned untouched.
    assert_eq!(
        redact_text("scan finished: 12 files"),
        "scan finished: 12 files"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A value following a sensitive key never survives, whatever the separator style.
    #[test]
    fn sensitive_values_never_survive(
        key in prop::sample::select(vec!["password", "api_key", "token", "secret", "apikey", "passkey"]),
        value in "[A-Za-z0-9]{8,16}",
        sep in prop::sample::select(vec!["=", ": ", "="]),
        pre in "[a-z ]{0,8}",
    ) {
        let secret = format!("ZZ{value}");
        let text = format!("{pre} {key}{sep}{secret} tail=1");
        let out = redact_text(&text);
        prop_assert!(!out.contains(&secret), "{}", out);
        prop_assert!(out.contains("tail=1"));
    }

    #[test]
    fn redact_text_is_idempotent(text in "[ -~]{0,60}") {
        let once = redact_text(&text);
        prop_assert_eq!(redact_text(&once), once);
    }
}

// ---- mapping edge cases ------------------------------------------------------------------

fn mapper(f: &Fixture) -> PathMapper {
    let roots = Roots::validate(&RootPaths {
        completed: Some(f.p("completed")),
        incomplete: None,
        media: vec![f.p("media1")],
    })
    .unwrap();
    let mut rules = BTreeMap::new();
    rules.insert(
        Qbittorrent,
        vec![
            ("/downloads".to_string(), f.p("completed")),
            ("/media".to_string(), f.p("media1")),
        ],
    );
    PathMapper::new(&roots, &rules, &[]).unwrap()
}

#[test]
fn trailing_slash_and_double_slash_are_normalized_inside_the_rule() {
    let f = Fixture::new();
    let m = mapper(&f);
    for raw in [
        "/downloads/",
        "/downloads//Show//ep.mkv",
        "/downloads/Show/",
    ] {
        let r = m.to_app(Qbittorrent, raw);
        assert!(matches!(r, MappedPath::Mapped { .. }), "{raw}: {r:?}");
    }
    // The original string is preserved for evidence.
    assert_eq!(
        m.to_app(Qbittorrent, "/downloads//x").raw(),
        "/downloads//x"
    );
}

#[test]
fn prefix_matching_is_case_sensitive_and_component_bounded() {
    let f = Fixture::new();
    let m = mapper(&f);
    for raw in ["/Downloads/x", "/downloadsx/x", "/download/x", "/media2/x"] {
        assert!(
            matches!(m.to_app(Qbittorrent, raw), MappedPath::NoRule { .. }),
            "{raw}"
        );
    }
}

#[test]
fn dot_dot_never_escapes_even_when_it_would_land_in_another_root() {
    let f = Fixture::new();
    let m = mapper(&f);
    // Lexically /downloads/../media/x would be /media/x (a valid root): still refused.
    for raw in [
        "/downloads/../media/x",
        "/media/sub/../../downloads",
        "/downloads/..",
    ] {
        assert!(
            matches!(m.to_app(Qbittorrent, raw), MappedPath::Invalid { .. }),
            "{raw}"
        );
    }
}

#[test]
fn services_without_rules_get_no_identity_mapping() {
    let f = Fixture::new();
    let m = mapper(&f);
    // A path that happens to equal an app path is not mapped for a service with no rule.
    assert!(matches!(
        m.to_app(Sonarr, &f.p("completed/x")),
        MappedPath::NoRule { .. }
    ));
}
