#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
//! Wave 16 memory evidence (ignored): size of what the job runner keeps after a scan.
//! Run: `cargo test -p removarr-jobs --test memory -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::process::Command;

use removarr_domain::path::RootedPath;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot,
};

/// Resident set in KiB. The workspace forbids `unsafe`, so no counting allocator: a fresh test
/// process only grows its heap while data is alive, which makes the RSS delta a fair estimate of
/// the live bytes of what was just built.
fn rss_kib() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn mapped(rel: &str) -> MappedPath {
    MappedPath::Mapped {
        raw: format!("/data/torrents/completed/{rel}"),
        path: RootedPath::parse("completed", rel).unwrap(),
    }
}

/// 3,000 torrents, 6 files each (video, 4 subtitles, nfo), 6 candidate paths per file (DR-23).
fn snapshot() -> TorrentSnapshot {
    let mut torrents = Vec::new();
    let mut files = BTreeMap::new();
    for t in 0..3000 {
        let hash = TorrentHash::new(format!("{t:040x}")).unwrap();
        let name = format!("Some.Release.Name.{t}.2024.1080p.WEB-DL.x264-GROUP");
        torrents.push(TorrentInfo {
            hash: hash.clone(),
            name: name.clone(),
            state: "stalledUP".to_string().into(),
            progress: 1.0,
            added_on: Timestamp(0),
            completion_on: Some(Timestamp(1)),
            category: "radarr".into(),
            size: 1 << 30,
            save_path: mapped("Movies"),
            download_path: None,
            content_path: mapped(&name),
        });
        let list = (0..6)
            .map(|i| TorrentFile {
                index: i,
                name: format!("{name}/Subs/{i}_English.forced.srt"),
                size: 1000,
                priority: 1,
                progress: 1.0,
                candidates: (0..6)
                    .map(|c| mapped(&format!("{name}/Subs/{i}_English.forced.srt.{c}")))
                    .collect(),
            })
            .collect();
        files.insert(hash, list);
    }
    TorrentSnapshot {
        taken_at: Timestamp(0),
        version: "5.0".into(),
        major: 5,
        torrents,
        files,
    }
}

#[test]
#[ignore = "memory evidence"]
fn heap_held_by_the_service_cache_of_one_scan() {
    let base = rss_kib();
    let s = snapshot();
    let one = rss_kib() - base;
    let copy = s.clone();
    let two = rss_kib() - base - one;
    println!(
        "qbit snapshot (3000 torrents, 18000 files): ~{:.1} MiB; one clone: ~{:.1} MiB",
        one as f64 / 1024.0,
        two as f64 / 1024.0
    );
    drop(copy);
    drop(s);
}
