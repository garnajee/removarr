#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_config::{Config, ProblemKind};
use removarr_domain::policy::Capability;

fn keys(e: &removarr_config::ConfigError) -> Vec<String> {
    e.problems.iter().map(|p| p.key.clone()).collect()
}

#[test]
fn defaults_are_safe() {
    let f = Fixture::new();
    let c = Config::from_sources(None, &f.env()).unwrap();
    assert!(c.mode.dry_run);
    assert!(!c.mode.auto_mode);
    assert!(c.live_mode().is_none());
    assert!(c.owner.from_process);
    assert_eq!(c.limits.max_response_bytes, 32 * 1024 * 1024);
    assert_eq!(c.scan.qbit_refresh_minutes, 60);
    assert_eq!(c.scan.interval_minutes, 1440);
    assert_eq!(c.thresholds.plan_ttl_hours, 24);
    assert!(c.capabilities.is_enabled(Capability::Consolidate));
    for cap in [
        Capability::ArrGrab,
        Capability::ArrManualImport,
        Capability::ArrQueueRemove,
        Capability::ArrSetMonitored,
        Capability::AutoRemoveEmptyTorrents,
    ] {
        assert!(!c.capabilities.is_enabled(cap), "{cap:?}");
    }
    assert!(c.roots().incomplete().is_none());
    assert!(c.services.qbittorrent.is_none());
    assert!(c.server.trusted_proxies.is_empty());
    assert!(c.files.is_junk(".R07") && c.files.is_video(".MKV") && !c.files.is_junk(".r100"));
}

#[test]
fn live_mode_only_when_dry_run_disabled() {
    let f = Fixture::new();
    let mut e = f.env();
    set(&mut e, "REMOVARR_DRY_RUN", "false");
    assert!(
        Config::from_sources(None, &e)
            .unwrap()
            .live_mode()
            .is_some()
    );
    set(&mut e, "REMOVARR_DRY_RUN", "true");
    assert!(
        Config::from_sources(None, &e)
            .unwrap()
            .live_mode()
            .is_none()
    );
}

#[test]
fn env_overrides_file_and_empty_env_is_unset() {
    let f = Fixture::new();
    let file = format!(
        "[thresholds]\nplan_ttl_hours = 10\nhide_recent_days = 7\n[mode]\nauto_mode = true\n[roots]\nincomplete = \"{}\"\n",
        f.p("incomplete")
    );
    let mut e = f.env();
    let c = Config::from_sources(Some(&file), &e).unwrap();
    assert_eq!(c.thresholds.plan_ttl_hours, 10);
    assert!(c.mode.auto_mode);
    assert!(c.roots().incomplete().is_some());

    set(&mut e, "REMOVARR_PLAN_TTL_HOURS", "20");
    set(&mut e, "REMOVARR_HIDE_RECENT_DAYS", "  ");
    set(&mut e, "REMOVARR_AUTO_MODE", "false");
    let c = Config::from_sources(Some(&file), &e).unwrap();
    assert_eq!(c.thresholds.plan_ttl_hours, 20);
    assert_eq!(c.thresholds.hide_recent_days, 7);
    assert!(!c.mode.auto_mode);
}

#[test]
fn unknown_file_keys_and_secret_keys_are_rejected_without_echoing_values() {
    let f = Fixture::new();
    let err =
        Config::from_sources(Some("[auth]\npassword = \"SENTINEL-PW\"\n"), &f.env()).unwrap_err();
    assert!(!err.to_string().contains("SENTINEL-PW"), "{err}");
    let err = Config::from_sources(
        Some("[thresholds]\nplan_ttl_hours = \"SENTINEL-V\"\n"),
        &f.env(),
    )
    .unwrap_err();
    assert!(!err.to_string().contains("SENTINEL-V"), "{err}");
    assert!(err.to_string().contains("line 2"), "{err}");
}

#[test]
fn invalid_values_name_the_key_not_the_value() {
    let f = Fixture::new();
    let cases = [
        (
            "REMOVARR_PLAN_TTL_HOURS",
            "999",
            "thresholds.plan_ttl_hours",
        ),
        (
            "REMOVARR_PLAN_TTL_HOURS",
            "abc",
            "thresholds.plan_ttl_hours",
        ),
        (
            "REMOVARR_SCAN_INTERVAL_MINUTES",
            "30",
            "scan.interval_minutes",
        ),
        (
            "REMOVARR_QBIT_REFRESH_MINUTES",
            "2",
            "scan.qbit_refresh_minutes",
        ),
        ("REMOVARR_SCAN_ANCHOR_UTC", "25:00", "scan.anchor_utc"),
        ("REMOVARR_DRY_RUN", "maybe", "mode.dry_run"),
        (
            "REMOVARR_TRUSTED_PROXIES",
            "0.0.0.0/0",
            "server.trusted_proxies",
        ),
        (
            "REMOVARR_TRUSTED_PROXIES",
            "bogus",
            "server.trusted_proxies",
        ),
        ("REMOVARR_EXPECTED_OWNER", "abc", "owner.expected"),
        ("REMOVARR_BIND", "nope", "server.bind"),
        ("REMOVARR_LOG_LEVEL", "trace", "server.log_level"),
        ("REMOVARR_VIDEO_EXTENSIONS", "mkv", "files.video_extensions"),
        (
            "REMOVARR_JUNK_EXTENSIONS",
            ".nfo,.MKV",
            "files.junk_extensions",
        ),
        (
            "REMOVARR_SUBTITLE_EXTENSIONS",
            ".srt,.nfo",
            "files.subtitle_extensions",
        ),
        (
            "REMOVARR_MAX_RESPONSE_BYTES",
            "1024",
            "limits.max_response_bytes",
        ),
        (
            "REMOVARR_HTTP_CONCURRENCY",
            "17",
            "limits.http_concurrency_per_service",
        ),
        (
            "REMOVARR_SIMILARITY_THRESHOLD",
            "NaN",
            "matching.similarity_threshold",
        ),
        (
            "REMOVARR_COMPARE_CHUNK_BYTES",
            "100",
            "matching.compare_chunk_bytes",
        ),
        ("REMOVARR_CAP_ARR_GRAB", "perhaps", "capabilities.arr_grab"),
        (
            "REMOVARR_SHUTDOWN_GRACE_SECONDS",
            "4",
            "server.shutdown_grace_seconds",
        ),
        ("REMOVARR_QBIT_URL", "ftp://h", "services.qbittorrent.url"),
        (
            "REMOVARR_SESSION_KEY",
            "tooshort-SENTINEL",
            "auth.session_key",
        ),
        (
            "REMOVARR_PASSWORD_HASH",
            "plaintext-SENTINEL",
            "auth.password_hash",
        ),
    ];
    for (name, value, key) in cases {
        let mut e = f.env();
        set(&mut e, name, value);
        let err = Config::from_sources(None, &e).expect_err(name);
        assert!(keys(&err).iter().any(|k| k == key), "{name}={value}: {err}");
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(value), "{name}: value leaked: {shown}");
        assert_eq!(err.exit_code(), 2);
    }
}

#[test]
fn doubled_dollars_in_hash_are_accepted() {
    let f = Fixture::new();
    let mut e = f.env();
    set(
        &mut e,
        "REMOVARR_PASSWORD_HASH",
        &DUMMY_HASH.replace('$', "$$"),
    );
    let c = Config::from_sources(None, &e).unwrap();
    assert_eq!(c.auth.password_hash.expose(), DUMMY_HASH);
}

#[test]
fn empty_environment_lists_every_missing_field() {
    let err = Config::from_sources(None, &Default::default()).unwrap_err();
    let missing: Vec<_> = err
        .problems
        .iter()
        .filter(|p| p.kind == ProblemKind::Missing)
        .map(|p| p.key.as_str())
        .collect();
    for k in [
        "roots.completed",
        "roots.media",
        "auth.password_hash",
        "auth.session_key",
    ] {
        assert!(missing.contains(&k), "{k} in {missing:?}");
    }
    let text = err.to_string();
    assert!(text.contains("REMOVARR_COMPLETED_ROOT") && text.contains("REMOVARR_SESSION_KEY"));
}

#[test]
fn services_need_their_secrets() {
    let f = Fixture::new();
    let mut e = f.env();
    set(&mut e, "REMOVARR_QBIT_URL", "http://qbit.example:8080");
    set(&mut e, "REMOVARR_SONARR_URL", "http://sonarr.example");
    set(&mut e, "REMOVARR_RADARR_URL", "http://radarr.example");
    set(&mut e, "REMOVARR_JELLYFIN_URL", "http://jf.example");
    let err = Config::from_sources(None, &e).unwrap_err();
    for k in [
        "services.qbittorrent.password",
        "services.sonarr.api_key",
        "services.radarr.api_key",
        "services.jellyfin.api_key",
    ] {
        assert!(keys(&err).iter().any(|x| x == k), "{k}");
    }
    set(&mut e, "REMOVARR_QBIT_PASSWORD", "dummy-qbit-pw");
    set(&mut e, "REMOVARR_SONARR_API_KEY", "dummy-sonarr-key");
    set(&mut e, "REMOVARR_RADARR_API_KEY", "dummy-radarr-key");
    set(&mut e, "REMOVARR_JELLYFIN_API_KEY", "dummy-jf-key");
    let c = Config::from_sources(None, &e).unwrap();
    assert_eq!(c.services.qbittorrent.as_ref().unwrap().username, "admin");
    assert!(c.services.jellyfin.is_some());
}

#[test]
fn url_with_embedded_credentials_is_rejected() {
    let f = Fixture::new();
    let mut e = f.env();
    set(
        &mut e,
        "REMOVARR_SONARR_URL",
        "http://user:pw@sonarr.example",
    );
    set(&mut e, "REMOVARR_SONARR_API_KEY", "dummy-key");
    let err = Config::from_sources(None, &e).unwrap_err();
    assert!(!err.to_string().contains("pw@"));
}

#[test]
fn debug_and_summary_never_reveal_secrets() {
    let f = Fixture::new();
    let mut e = f.env();
    set(&mut e, "REMOVARR_QBIT_URL", "http://qbit.example:8080");
    set(&mut e, "REMOVARR_QBIT_PASSWORD", "SENTINEL-QBIT-PW");
    set(&mut e, "REMOVARR_RADARR_URL", "http://radarr.example");
    set(&mut e, "REMOVARR_RADARR_API_KEY", "SENTINEL-RADARR-KEY");
    let c = Config::from_sources(None, &e).unwrap();
    let dump = format!("{c:?} {c:#?} {}", c.startup_summary().join("\n"));
    for s in [
        "SENTINEL-QBIT-PW",
        "SENTINEL-RADARR-KEY",
        DUMMY_SESSION_KEY,
        DUMMY_HASH,
    ] {
        assert!(!dump.contains(s), "{s} leaked");
    }
    assert!(dump.contains("[REDACTED]"));
    let scrubbed = c.scrub("upstream said: bad SENTINEL-RADARR-KEY here");
    assert!(!scrubbed.contains("SENTINEL-RADARR-KEY"));
}

#[test]
fn fingerprint_tracks_policy_not_secrets() {
    let f = Fixture::new();
    let base = Config::from_sources(None, &f.env()).unwrap().fingerprint();
    let mut e = f.env();
    set(&mut e, "REMOVARR_SESSION_KEY", &"z".repeat(70));
    set(&mut e, "REMOVARR_BIND", "127.0.0.1:9000");
    assert_eq!(Config::from_sources(None, &e).unwrap().fingerprint(), base);
    set(&mut e, "REMOVARR_DRY_RUN", "false");
    assert_ne!(Config::from_sources(None, &e).unwrap().fingerprint(), base);
    let mut e = f.env();
    set(
        &mut e,
        "REMOVARR_QBIT_PATH_MAP",
        &format!("/dl={}", f.p("completed")),
    );
    assert_ne!(Config::from_sources(None, &e).unwrap().fingerprint(), base);
}

#[test]
fn trusted_proxies_load() {
    let f = Fixture::new();
    let mut e = f.env();
    set(
        &mut e,
        "REMOVARR_TRUSTED_PROXIES",
        "10.0.0.0/8, 192.168.1.5",
    );
    let c = Config::from_sources(None, &e).unwrap();
    let peer = "10.1.1.1".parse().unwrap();
    assert_eq!(
        c.server.trusted_proxies.client_ip(peer, &["203.0.113.5"]),
        "203.0.113.5".parse::<std::net::IpAddr>().unwrap()
    );
}

#[test]
fn explicit_missing_config_file_fails_but_default_missing_is_fine() {
    let f = Fixture::new();
    let mut e = f.env();
    // default path (/config/removarr.toml) normally absent: environment only
    assert!(
        Config::load_with_env(&e).is_ok() || std::path::Path::new("/config/removarr.toml").exists()
    );
    set(&mut e, "REMOVARR_CONFIG_FILE", &f.p("missing.toml"));
    let err = Config::load_with_env(&e).unwrap_err();
    assert_eq!(err.problems[0].key, "config_file");
    std::fs::write(f.p("ok.toml"), "[thresholds]\nplan_ttl_hours = 5\n").unwrap();
    set(&mut e, "REMOVARR_CONFIG_FILE", &f.p("ok.toml"));
    assert_eq!(
        Config::load_with_env(&e).unwrap().thresholds.plan_ttl_hours,
        5
    );
}

#[test]
fn shipped_examples_load() {
    let f = Fixture::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let toml = std::fs::read_to_string(root.join("examples/removarr.toml")).unwrap();
    // The example paths are generic; point them at the fixture.
    let toml = toml
        .replace("/data/completed", &f.p("completed"))
        .replace("/data/incomplete", &f.p("incomplete"))
        .replace("/data/media", &f.p("media1"))
        .replace("\"/config\"", &format!("\"{}\"", f.p("data")));
    let mut e = f.env();
    e.remove("REMOVARR_COMPLETED_ROOT");
    e.remove("REMOVARR_MEDIA_ROOTS");
    e.remove("REMOVARR_DATA_DIR");
    let c = Config::from_sources(Some(&toml), &e).unwrap();
    assert!(c.mode.dry_run && !c.mode.auto_mode);
}

#[test]
fn env_example_has_no_values_for_secrets() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join(".env.example")).unwrap();
    for line in text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains('='))
    {
        let (k, v) = line.split_once('=').unwrap();
        if k.contains("PASSWORD") || k.contains("API_KEY") || k.contains("SESSION_KEY") {
            assert!(v.trim().is_empty(), "{k} must be empty in the template");
        }
    }
    assert!(text.contains("REMOVARR_SESSION_KEY=") && text.contains("REMOVARR_DRY_RUN=true"));
}
