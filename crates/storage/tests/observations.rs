#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use removarr_domain::arr::{ArrMediaRef, ArrQualitySnapshot, ArrService, MediaIdentity, MediaKind};
use removarr_domain::error::ErrorClass;
use removarr_domain::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, IdentityProof, MatchCandidate, Provenance,
};
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{Issue, IssueCategory, Outcome, Severity, Unit};
use removarr_domain::ports::{IgnoreStore, IssueStore, MatchStore, SnapshotStore, StateStore};
use removarr_domain::session::{ServiceKind, ServiceState, ServiceStatus};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, TorrentState,
};
use sqlx::Row;

fn torrent(n: u8, progress: f64) -> TorrentInfo {
    TorrentInfo {
        hash: TorrentHash::new(format!("{n:040x}")).unwrap(),
        name: format!("t{n}"),
        state: TorrentState::StalledUp,
        progress,
        added_on: Timestamp(1),
        completion_on: None,
        category: "tv".into(),
        size: 10,
        save_path: MappedPath::NoRule { raw: "/dl".into() },
        download_path: None,
        content_path: MappedPath::NoRule {
            raw: "/dl/x".into(),
        },
    }
}

fn snapshot(ts: &[TorrentInfo], at: i64) -> TorrentSnapshot {
    let mut files = BTreeMap::new();
    for t in ts {
        files.insert(
            t.hash.clone(),
            vec![TorrentFile {
                index: 0,
                name: "a.mkv".into(),
                size: 10,
                priority: 1,
                progress: 1.0,
                candidates: vec![],
            }],
        );
    }
    TorrentSnapshot {
        taken_at: Timestamp(at),
        version: "5.0.1".into(),
        major: 5,
        torrents: ts.to_vec(),
        files,
    }
}

#[tokio::test]
async fn torrent_snapshot_roundtrip_skips_unchanged_and_drops_removed() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    assert!(f.repos.snapshots.latest_torrents().await.unwrap().is_none());
    let snap = snapshot(&[torrent(1, 1.0), torrent(2, 1.0)], 100);
    f.repos.snapshots.save_torrents(s.id, &snap).await.unwrap();
    assert_eq!(
        f.repos.snapshots.latest_torrents().await.unwrap().unwrap(),
        snap
    );

    // Torrent 1 unchanged, 2 changed, 3 new. Only 2 and 3 are written.
    let mut raw = f.raw().await;
    let v0: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    let snap2 = snapshot(&[torrent(1, 1.0), torrent(2, 0.5), torrent(3, 1.0)], 200);
    f.repos.snapshots.save_torrents(s.id, &snap2).await.unwrap();
    let v1: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(v1 - v0, 1, "one commit");
    assert_eq!(
        f.repos.snapshots.latest_torrents().await.unwrap().unwrap(),
        snap2
    );

    let snap3 = snapshot(&[torrent(1, 1.0)], 300);
    f.repos.snapshots.save_torrents(s.id, &snap3).await.unwrap();
    assert_eq!(
        f.repos.snapshots.latest_torrents().await.unwrap().unwrap(),
        snap3
    );
}

fn arr_snapshot(profile: i64, at: i64) -> ArrQualitySnapshot {
    ArrQualitySnapshot {
        service: ArrService::Sonarr,
        service_version: "4".into(),
        media: MediaIdentity {
            kind: MediaKind::Episode,
            arr: Some(ArrMediaRef {
                service: ArrService::Sonarr,
                media_id: 5,
                episode_ids: vec![1],
            }),
            external_ids: BTreeMap::new(),
            title: "Show".into(),
            year: None,
            season: Some(1),
            episode: Some(1),
            absolute: None,
        },
        profile_id: profile,
        profile_name: "HD".into(),
        cutoff: "WEB-1080p".into(),
        upgrade_allowed: true,
        min_format_score: 0,
        cutoff_format_score: 0,
        format_scores: BTreeMap::new(),
        current_files: vec![],
        candidates: vec![],
        media_management: None,
        captured_at: Timestamp(at),
        fingerprint: String::new(),
    }
    .seal()
    .unwrap()
}

#[tokio::test]
async fn arr_snapshots_dedupe_by_fingerprint_and_reject_bad_fingerprints() {
    let f = fx().await;
    let a = arr_snapshot(1, 100);
    f.repos.snapshots.save_arr(&a).await.unwrap();
    f.repos
        .snapshots
        .save_arr(&arr_snapshot(1, 200))
        .await
        .unwrap(); // same fingerprint, later
    f.repos
        .snapshots
        .save_arr(&arr_snapshot(1, 50))
        .await
        .unwrap(); // older: ignored
    f.repos
        .snapshots
        .save_arr(&arr_snapshot(2, 300))
        .await
        .unwrap(); // quality changed

    let mut raw = f.raw().await;
    let rows = sqlx::query("SELECT fingerprint, captured_at FROM arr_snapshots ORDER BY id")
        .fetch_all(&mut raw)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<String, _>(0), a.fingerprint);
    assert_eq!(rows[0].get::<i64, _>(1), 200);

    let mut bad = a.clone();
    bad.cutoff = "Bluray-2160p".into();
    assert_eq!(
        f.repos.snapshots.save_arr(&bad).await.unwrap_err().class,
        ErrorClass::Validation
    );
    let latest = f
        .repos
        .snapshots
        .latest_arr(&removarr_storage::arr_media_key(&a))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.profile_id, 2);
}

#[tokio::test]
async fn service_status_is_stored_per_session_and_scrubbed() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let st = |reason: &str| ServiceStatus {
        service: ServiceKind::Qbittorrent,
        state: ServiceState::Unavailable {
            reason: reason.into(),
        },
        observed_at: Timestamp(5),
    };
    f.repos
        .snapshots
        .save_service_status(s.id, &[st("connect to http://10.0.0.5:8080 refused")])
        .await
        .unwrap();
    f.repos
        .snapshots
        .save_service_status(s.id, &[st("timeout")])
        .await
        .unwrap();
    let back = f.repos.scans.session(s.id).await.unwrap().unwrap();
    assert_eq!(back.services.len(), 1);
    assert_eq!(
        back.services[0].state,
        ServiceState::Unavailable {
            reason: "timeout".into()
        }
    );
}

fn candidate(
    session: removarr_domain::ids::SessionId,
    a: &str,
    b: &str,
    content: ContentProof,
) -> MatchCandidate {
    MatchCandidate {
        a: rp(a),
        b: rp(b),
        sig_a: sig(1, 10),
        sig_b: sig(2, 10),
        identity: IdentityProof::ArrMatched,
        content,
        reasons: vec!["same size".into()],
        session,
    }
}

#[tokio::test]
async fn match_candidates_are_replaced_diffed_and_proofs_updated() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    let s2 = begin(&f, 2).await;
    assert!(f.repos.matches.current().await.unwrap().is_empty());
    let c1 = candidate(s1.id, "a", "b", ContentProof::NameSize);
    let c2 = candidate(s1.id, "c", "d", ContentProof::Sampled);
    f.repos
        .matches
        .replace_for_session(s1.id, &[c1.clone(), c2.clone()])
        .await
        .unwrap();
    assert_eq!(
        f.repos.matches.current().await.unwrap(),
        vec![c1.clone(), c2.clone()]
    );

    // A new session: c1 gone, c2 unchanged, c3 new. Candidates report the new session.
    let c3 = candidate(s2.id, "e", "f", ContentProof::None);
    f.repos
        .matches
        .replace_for_session(s2.id, &[c2.clone(), c3.clone()])
        .await
        .unwrap();
    let now = f.repos.matches.current().await.unwrap();
    assert_eq!(now.len(), 2);
    assert!(now.iter().all(|c| c.session == s2.id));

    // VerifyContent upgrades the proof; the reverse pair order finds the same row.
    let identical = ContentProof::Identical {
        compared_at: Timestamp(9),
    };
    f.repos
        .matches
        .update_proof(&rp("d"), &rp("c"), &identical)
        .await
        .unwrap();
    let c = f.repos.matches.current().await.unwrap();
    assert_eq!(
        c.iter().find(|x| x.a == rp("c")).unwrap().content,
        identical
    );
    assert_eq!(
        f.repos
            .matches
            .update_proof(&rp("x"), &rp("y"), &identical)
            .await
            .unwrap_err()
            .class,
        ErrorClass::NotFound
    );
    let dup = f
        .repos
        .matches
        .replace_for_session(s2.id, &[c3.clone(), c3])
        .await
        .unwrap_err();
    assert_eq!(dup.class, ErrorClass::Validation);
}

fn issue(
    subject: &str,
    session: removarr_domain::ids::SessionId,
    first_seen: i64,
    bytes: u64,
) -> Issue {
    let subject = SubjectKey(subject.into());
    Issue {
        id: IssueId::derive(IssueCategory::HardlinkMissing, &subject),
        category: IssueCategory::HardlinkMissing,
        subject,
        unit: Unit::File(rp("a.mkv")),
        severity: Severity::Warning,
        outcome: Outcome::Consolidate,
        evidence: vec![],
        blockers: vec![],
        torrent_hashes: BTreeSet::new(),
        logical_bytes: bytes,
        potential_bytes: bytes,
        age_basis: Timestamp(0),
        first_seen_at: Timestamp(first_seen),
        session,
        physical_ids: BTreeSet::new(),
    }
}

fn evidence() -> Evidence {
    Evidence {
        kind: EvidenceKind::Content {
            a: rp("a"),
            b: rp("b"),
            proof: ContentProof::Sampled,
        },
        provenance: Provenance::Computed,
        observed_at: Timestamp(3),
        completeness: Completeness::Complete,
    }
}

#[tokio::test]
async fn issues_keep_first_seen_resolve_when_absent_and_reappear() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    let s2 = begin(&f, 2).await;
    let s3 = begin(&f, 3).await;
    let a1 = issue("p:completed:a", s1.id, 100, 10);
    let b1 = issue("p:completed:b", s1.id, 100, 20);
    f.repos
        .issues
        .replace_current(s1.id, &[a1.clone(), b1.clone()])
        .await
        .unwrap();
    assert_eq!(
        f.repos
            .issues
            .get_many(&[a1.id.clone(), b1.id.clone()])
            .await
            .unwrap(),
        vec![a1.clone(), b1.clone()]
    );

    // Later evaluation: `a` changed (size) and reports a later first_seen_at; `b` disappeared.
    let a2 = issue("p:completed:a", s2.id, 999, 11);
    f.clock.advance_millis(1000);
    f.repos
        .issues
        .replace_current(s2.id, &[a2.clone()])
        .await
        .unwrap();
    let got = f
        .repos
        .issues
        .get_many(&[a1.id.clone(), b1.id.clone()])
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "b is resolved");
    assert_eq!(
        got[0].first_seen_at,
        Timestamp(100),
        "first_seen_at survives the replace"
    );
    assert_eq!((got[0].logical_bytes, got[0].session), (11, s2.id));

    // b reappears: back to current with its original first_seen_at.
    f.repos
        .issues
        .replace_current(
            s3.id,
            &[a2.clone(), issue("p:completed:b", s3.id, 5000, 20)],
        )
        .await
        .unwrap();
    let b = f.repos.issues.get_many(&[b1.id.clone()]).await.unwrap();
    assert_eq!(b[0].first_seen_at, Timestamp(100));

    // Wrong id is rejected.
    let mut bad = a2.clone();
    bad.id = IssueId("0".repeat(32));
    assert_eq!(
        f.repos
            .issues
            .replace_current(s3.id, &[bad])
            .await
            .unwrap_err()
            .class,
        ErrorClass::Validation
    );
}

#[tokio::test]
async fn unchanged_issues_cause_no_write_and_evidence_attaches_once() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    let s2 = begin(&f, 2).await;
    let i = issue("p:completed:a", s1.id, 1, 10);
    f.repos
        .issues
        .replace_current(s1.id, &[i.clone()])
        .await
        .unwrap();

    let mut raw = f.raw().await;
    let v0: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    let mut again = i.clone();
    again.session = s2.id;
    f.repos
        .issues
        .replace_current(s2.id, &[again])
        .await
        .unwrap();
    let v1: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(v1 - v0, 1, "only the session marker commits");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE resolved_at IS NULL")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(n, 1);

    f.repos
        .issues
        .attach_evidence(&i.id, &[evidence(), evidence()])
        .await
        .unwrap();
    f.repos
        .issues
        .attach_evidence(&i.id, &[evidence()])
        .await
        .unwrap();
    let got = f.repos.issues.get_many(&[i.id.clone()]).await.unwrap();
    assert_eq!(got[0].evidence, vec![evidence()]);
    assert_eq!(
        f.repos
            .issues
            .attach_evidence(&IssueId("1".repeat(32)), &[evidence()])
            .await
            .unwrap_err()
            .class,
        ErrorClass::NotFound
    );
}

#[tokio::test]
async fn ignore_list_restore_and_reignore() {
    let f = fx().await;
    let (a, b) = (
        SubjectKey("t:aa".into()),
        SubjectKey("p:completed:b".into()),
    );
    f.repos
        .ignores
        .ignore(&[a.clone(), b.clone()], Some("keep seeding"), Timestamp(1))
        .await
        .unwrap();
    f.repos
        .ignores
        .ignore(&[a.clone()], Some("keep seeding"), Timestamp(2))
        .await
        .unwrap();
    assert_eq!(
        f.repos.ignores.ignored().await.unwrap(),
        BTreeSet::from([a.clone(), b.clone()])
    );
    f.repos
        .ignores
        .restore(&[a.clone(), SubjectKey("never-ignored".into())])
        .await
        .unwrap();
    assert_eq!(
        f.repos.ignores.ignored().await.unwrap(),
        BTreeSet::from([b.clone()])
    );
    f.repos
        .ignores
        .ignore(&[a.clone()], None, Timestamp(3))
        .await
        .unwrap();
    assert!(f.repos.ignores.ignored().await.unwrap().contains(&a));
    let mut raw = f.raw().await;
    let created: i64 =
        sqlx::query_scalar("SELECT created_at FROM ignores WHERE subject_key = 't:aa'")
            .fetch_one(&mut raw)
            .await
            .unwrap();
    assert_eq!(created, 3, "a restored subject gets a fresh created_at");
}

#[tokio::test]
async fn state_store_roundtrip_and_no_rewrite() {
    let f = fx().await;
    assert_eq!(f.repos.state.get("auto_paused").await.unwrap(), None);
    f.repos.state.set("auto_paused", "true").await.unwrap();
    f.clock.advance_millis(5);
    f.repos.state.set("auto_paused", "true").await.unwrap();
    let mut raw = f.raw().await;
    let at: i64 = sqlx::query_scalar("SELECT updated_at FROM app_state")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(at, 1_700_000_000_000, "unchanged value is not rewritten");
    f.repos.state.set("auto_paused", "false").await.unwrap();
    assert_eq!(
        f.repos.state.get("auto_paused").await.unwrap().as_deref(),
        Some("false")
    );
    assert_eq!(
        f.repos.state.set("", "x").await.unwrap_err().class,
        ErrorClass::Validation
    );
}
