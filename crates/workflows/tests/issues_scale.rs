#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Classification of a reference-size library stays near linear (20k files, 3k torrents): it is
//! pure CPU over stored data, which is what a refresh pays for.

mod common;

use std::time::Instant;

use common::*;
use removarr_domain::issue::IssueCategory as C;
use removarr_workflows::issues::QbitObservation;

#[test]
fn twenty_thousand_files_and_three_thousand_torrents_classify_quickly() {
    let mut c = Inv::new(completed());
    let mut m = Inv::new(media());
    let mut tors = Vec::new();
    for i in 0..20_000u64 {
        let dir = i % 200;
        c = c.file(&format!("d{dir}/f{i}.mkv"), 1_000 + i, 4096);
        // Half of them are hardlinked into the library, the rest are copies or absent.
        if i % 2 == 0 {
            m = m.file_nlink(&format!("lib{dir}/f{i}.mkv"), 1_000 + i, 4096, 2);
        }
        if i < 3_000 {
            let name = format!("d{dir}/f{i}.mkv");
            let mut t = Tor::new(
                'a',
                &completed(),
                "uploading",
                1.0,
                &name,
                &[(&name, &name)],
            );
            t.info.hash = removarr_domain::torrent::TorrentHash::new(format!("{i:040x}")).unwrap();
            tors.push(t);
        }
    }
    let o = obs(
        vec![c.build(), m.build()],
        QbitObservation::Available(snapshot(5, tors)),
    );
    let t = Instant::now();
    let r = run(&o);
    let elapsed = t.elapsed();
    assert!(!of(&r, C::HardlinkMissing).is_empty());
    assert!(elapsed.as_secs() < 20, "classification took {elapsed:?}");
}
