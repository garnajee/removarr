//! `FfprobeProbe` process handling with a fake `ffprobe` (a shell script). The real binary is
//! never required; the last test runs it only when explicitly requested (`--ignored`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use removarr_domain::error::ErrorClass;
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::MediaProbe;
use removarr_matching::{FfprobeProbe, FfprobeSettings};

const JSON: &str = r#"{"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720},{"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":"fre"}}],"format":{"duration":"1325.5"}}"#;

struct Env {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    media: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let media = base.join("media");
    std::fs::create_dir_all(media.join("tv")).unwrap();
    std::fs::write(media.join("tv/Show.S01E01.mkv"), b"not really a video").unwrap();
    Env {
        _tmp: tmp,
        base,
        media,
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn media_id(e: &Env) -> RootId {
    RootId::media(e.media.to_str().unwrap())
}

fn probe_for(e: &Env, program: PathBuf) -> FfprobeProbe {
    let mut roots = BTreeMap::new();
    roots.insert(media_id(e), e.media.clone());
    let mut s = FfprobeSettings::new(program, roots);
    s.timeout = Duration::from_secs(5);
    FfprobeProbe::new(s)
}

fn target(e: &Env) -> RootedPath {
    RootedPath::new(media_id(e), RelPath::new("tv/Show.S01E01.mkv").unwrap())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn probe_runs_argv_only_with_file_protocol_and_parses_the_report() {
    let e = env();
    let log = e.base.join("args.log");
    let prog = script(
        &e.base,
        "ffprobe",
        &format!(
            "for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\necho '{JSON}'",
            log.display()
        ),
    );
    let report = rt()
        .block_on(probe_for(&e, prog).probe(&target(&e)))
        .unwrap();
    assert_eq!(report.duration_s, Some(1325.5));
    assert_eq!(report.video[0].height, Some(720));
    assert_eq!(report.audio[0].language.as_deref(), Some("fre"));
    let args: Vec<String> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let file_arg = format!("file:{}", e.media.join("tv/Show.S01E01.mkv").display());
    assert_eq!(
        args.last().unwrap(),
        &file_arg,
        "path is one argv entry, never a shell string"
    );
    let wl = args
        .iter()
        .position(|a| a == "-protocol_whitelist")
        .unwrap();
    assert_eq!(args[wl + 1], "file");
    assert!(args.contains(&"json".to_string()));
}

#[test]
fn the_child_does_not_inherit_the_process_environment() {
    let e = env();
    let log = e.base.join("env.log");
    let prog = script(
        &e.base,
        "ffprobe",
        &format!("env > '{}'\necho '{JSON}'", log.display()),
    );
    rt().block_on(probe_for(&e, prog).probe(&target(&e)))
        .unwrap();
    let env_dump = std::fs::read_to_string(log).unwrap();
    assert!(env_dump.contains("PATH="), "{env_dump}");
    for leaked in ["HOME=", "USER=", "REMOVARR_", "CARGO"] {
        assert!(!env_dump.contains(leaked), "leaked {leaked}:\n{env_dump}");
    }
}

#[test]
fn failure_timeout_and_oversized_output_are_typed_errors_without_host_paths() {
    let e = env();
    let rt = rt();
    let fail = script(
        &e.base,
        "fail",
        "echo \"$7 $8 $9 ${10}: Invalid data\" >&2\necho '' >&2\nexit 1",
    );
    let err = rt
        .block_on(probe_for(&e, fail).probe(&target(&e)))
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    assert!(err.detail.contains("ffprobe failed"), "{}", err.detail);
    assert!(
        !err.detail.contains(e.base.to_str().unwrap()),
        "{}",
        err.detail
    );

    let slow = script(&e.base, "slow", "sleep 10");
    let mut roots = BTreeMap::new();
    roots.insert(media_id(&e), e.media.clone());
    let mut s = FfprobeSettings::new(slow, roots.clone());
    s.timeout = Duration::from_millis(300);
    let start = Instant::now();
    let err = rt
        .block_on(FfprobeProbe::new(s).probe(&target(&e)))
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    assert!(err.detail.contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(5));

    let big = script(&e.base, "big", "exec head -c 200000 /dev/zero");
    let mut s = FfprobeSettings::new(big, roots.clone());
    s.max_output_bytes = 1024;
    s.timeout = Duration::from_secs(5);
    let err = rt
        .block_on(FfprobeProbe::new(s).probe(&target(&e)))
        .unwrap_err();
    assert!(err.detail.contains("exceeds"), "{}", err.detail);

    let garbage = script(&e.base, "garbage", "echo not-json");
    let err = rt
        .block_on(probe_for(&e, garbage).probe(&target(&e)))
        .unwrap_err();
    assert!(err.detail.contains("unreadable"), "{}", err.detail);

    let missing = rt
        .block_on(probe_for(&e, e.base.join("no-such-ffprobe")).probe(&target(&e)))
        .unwrap_err();
    assert_eq!(missing.class, ErrorClass::Config);
    let avail = rt
        .block_on(probe_for(&e, e.base.join("no-such-ffprobe")).check_available())
        .unwrap_err();
    assert_eq!(avail.class, ErrorClass::Config);
}

#[test]
fn symlinks_unknown_roots_and_directories_are_refused_before_spawning() {
    let e = env();
    let marker = e.base.join("spawned");
    let prog = script(
        &e.base,
        "ffprobe",
        &format!("touch '{}'\necho '{JSON}'", marker.display()),
    );
    let outside = e.base.join("outside.mkv");
    std::fs::write(&outside, b"x").unwrap();
    std::os::unix::fs::symlink(&outside, e.media.join("tv/link.mkv")).unwrap();
    std::os::unix::fs::symlink(e.base.clone(), e.media.join("dirlink")).unwrap();
    let p = probe_for(&e, prog);
    let rt = rt();
    let at = |rel: &str| RootedPath::new(media_id(&e), RelPath::new(rel).unwrap());
    let err = rt.block_on(p.probe(&at("tv/link.mkv"))).unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    let err = rt
        .block_on(p.probe(&at("dirlink/outside.mkv")))
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    let err = rt.block_on(p.probe(&at("tv"))).unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    let unknown = RootedPath::new(RootId::completed(), RelPath::new("a.mkv").unwrap());
    assert_eq!(
        rt.block_on(p.probe(&unknown)).unwrap_err().class,
        ErrorClass::Confinement
    );
    assert!(!marker.exists(), "ffprobe must not run on a refused path");
}

#[test]
fn concurrent_probes_are_bounded() {
    let e = env();
    let log = e.base.join("spans.log");
    let prog = script(
        &e.base,
        "ffprobe",
        &format!(
            "echo start >> '{l}'\nsleep 0.2\necho end >> '{l}'\necho '{JSON}'",
            l = log.display()
        ),
    );
    let mut roots = BTreeMap::new();
    roots.insert(media_id(&e), e.media.clone());
    let mut s = FfprobeSettings::new(prog, roots);
    s.concurrency = 1;
    let probe = FfprobeProbe::new(s);
    let t = target(&e);
    rt().block_on(async {
        let (a, b, c, d) = tokio::join!(
            probe.probe(&t),
            probe.probe(&t),
            probe.probe(&t),
            probe.probe(&t)
        );
        for r in [a, b, c, d] {
            r.unwrap();
        }
    });
    let lines: Vec<String> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 8);
    for chunk in lines.chunks(2) {
        assert_eq!(
            chunk,
            ["start", "end"],
            "never two ffprobe processes at once"
        );
    }
}

/// Real ffprobe, opt-in only (`cargo test -p removarr-matching -- --ignored`). Not coverage:
/// it only proves the argument set is accepted by an installed binary.
#[test]
#[ignore = "requires a real ffprobe binary on PATH"]
fn real_ffprobe_accepts_the_argument_set() {
    let e = env();
    let err = rt()
        .block_on(probe_for(&e, PathBuf::from("ffprobe")).probe(&target(&e)))
        .unwrap_err();
    // The fixture file is not a video: ffprobe must fail on content, not on its arguments.
    assert_eq!(err.class, ErrorClass::Rejected, "{}", err.detail);
    assert!(
        !err.detail.contains("Unrecognized option"),
        "{}",
        err.detail
    );
}
