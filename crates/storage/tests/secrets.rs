#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{IoKind, ScanErrorRecord};
use removarr_domain::gain::GainEstimate;
use removarr_domain::ids::{ActionRef, IntentId, JobId, RawUuid, SubjectKey};
use removarr_domain::jobs::{JobKind, JobOutcome, JobRecord, Lane};
use removarr_domain::journal::{IntentRecord, JournalMode, JournalStep, ResultRecord, StepOutcome};
use removarr_domain::ports::{
    IgnoreStore, JobStore, JournalStore, ScanStore, SnapshotStore, StateStore,
};
use removarr_domain::session::{ServiceKind, ServiceState, ServiceStatus};
use removarr_domain::time::Timestamp;
use serde_json::json;
use sqlx::Row;

const FORBIDDEN: &[&str] = &[
    "tracker.example",
    "passkey=abc123",
    "hunter2",
    "http://",
    "https://",
    "magnet:",
];

#[tokio::test]
async fn no_url_or_secret_reaches_any_table_or_the_wal_file() {
    let f = fx().await;
    let leak = "GET https://tracker.example/announce?passkey=abc123 failed apikey=hunter2 \
                Authorization: Bearer hunter2 see magnet:?xt=urn:btih:00";
    let s = begin(&f, 1).await;

    f.repos
        .snapshots
        .save_service_status(
            s.id,
            &[ServiceStatus {
                service: ServiceKind::Sonarr,
                state: ServiceState::Unavailable {
                    reason: leak.into(),
                },
                observed_at: Timestamp(1),
            }],
        )
        .await
        .unwrap();
    let mut inv = inventory(vec![entry("a", 1, 1)], true);
    inv.errors.push(ScanErrorRecord {
        path: rp("e"),
        io: IoKind::Other,
        message: leak.into(),
    });
    f.repos.scans.save_inventory(s.id, &inv).await.unwrap();
    f.repos
        .ignores
        .ignore(&[SubjectKey("t:00".into())], Some(leak), Timestamp(1))
        .await
        .unwrap();
    f.repos.state.set("note", leak).await.unwrap();

    let job = JobRecord::queued(
        JobId::new(RawUuid::from_u128(1)),
        JobKind::Scan,
        Timestamp(1),
    )
    .unwrap();
    f.repos.jobs.enqueue(&job).await.unwrap();
    f.repos.jobs.claim_next(Lane::Observe).await.unwrap();
    f.repos.jobs.set_stage(job.id, leak).await.unwrap();
    f.repos
        .jobs
        .finish(
            job.id,
            &JobOutcome::Failed {
                class: ErrorClass::Unavailable,
                message: leak.into(),
            },
        )
        .await
        .unwrap();

    let p = executing_plan(&f, 2, 1).await;
    let intent = IntentRecord {
        id: IntentId::new(RawUuid::from_u128(5)),
        action: ActionRef {
            plan: p.id,
            index: 0,
        },
        step: JournalStep::QbitStop,
        mode: JournalMode::Live,
        target: json!({"note": leak, "nested": [leak]}),
        observed: json!({}),
        expected_gain: GainEstimate::default(),
        hashes: BTreeSet::new(),
        created_at: Timestamp(1),
    };
    f.repos.journal.record_intent(&intent).await.unwrap();
    f.repos
        .journal
        .record_result(&ResultRecord {
            intent_id: intent.id,
            outcome: StepOutcome::Failed {
                class: ErrorClass::Unavailable,
                message: leak.into(),
            },
            at: Timestamp(2),
        })
        .await
        .unwrap();

    // Every text column of every table.
    let mut raw = f.raw().await;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE '\\_sqlx%' ESCAPE '\\'")
            .fetch_all(&mut raw)
            .await
            .unwrap();
    for t in tables {
        let cols: Vec<String> = sqlx::query(&format!("SELECT name FROM pragma_table_info('{t}')"))
            .fetch_all(&mut raw)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>(0))
            .collect();
        let expr = cols
            .iter()
            .map(|c| format!("COALESCE(CAST({c} AS TEXT), '')"))
            .collect::<Vec<_>>()
            .join(" || ' ' || ");
        for row in sqlx::query(&format!("SELECT {expr} AS line FROM {t}"))
            .fetch_all(&mut raw)
            .await
            .unwrap()
        {
            let line: String = row.get(0);
            for bad in FORBIDDEN {
                assert!(!line.contains(bad), "{t} contains {bad}: {line}");
            }
        }
    }
    // And the raw bytes of the main file and the WAL.
    for ext in ["db", "db-wal"] {
        if let Ok(bytes) = std::fs::read(f.path().with_extension(ext)) {
            for bad in FORBIDDEN {
                assert!(
                    !bytes.windows(bad.len()).any(|w| w == bad.as_bytes()),
                    "{ext} contains {bad}"
                );
            }
        }
    }
}
