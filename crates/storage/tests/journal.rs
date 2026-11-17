#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::gain::GainEstimate;
use removarr_domain::ids::{ActionRef, IntentId, RawUuid};
use removarr_domain::journal::{
    ActionOutcome, ActionState, IntentRecord, JournalMode, JournalStep, ResultRecord, StepOutcome,
};
use removarr_domain::plan::PlanStatus;
use removarr_domain::ports::{JournalStore, PlanStore};
use removarr_domain::time::Timestamp;
use removarr_storage::Db;
use serde_json::json;

fn intent(
    n: u128,
    plan: removarr_domain::ids::PlanId,
    idx: u32,
    mode: JournalMode,
) -> IntentRecord {
    IntentRecord {
        id: IntentId::new(RawUuid::from_u128(0x2000 + n)),
        action: ActionRef { plan, index: idx },
        step: JournalStep::FsRemoveFile,
        mode,
        target: json!({"path": "junk/a.nfo"}),
        observed: json!({"size": 1}),
        expected_gain: GainEstimate::default(),
        hashes: BTreeSet::new(),
        created_at: Timestamp(5),
    }
}

fn result(i: &IntentRecord) -> ResultRecord {
    ResultRecord {
        intent_id: i.id,
        outcome: StepOutcome::Done {
            final_state: json!({"gone": true}),
        },
        at: Timestamp(6),
    }
}

#[tokio::test]
async fn intent_result_roundtrip_in_journal_order() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 2).await;
    let i0 = intent(1, p.id, 0, JournalMode::Live);
    let i1 = intent(2, p.id, 1, JournalMode::Live);
    f.repos.journal.record_intent(&i0).await.unwrap();
    f.repos.journal.record_intent(&i1).await.unwrap();
    f.repos.journal.record_result(&result(&i0)).await.unwrap();

    let all = f.repos.journal.for_plan(p.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], (i0.clone(), Some(result(&i0))));
    assert_eq!(all[1], (i1.clone(), None));
}

#[tokio::test]
async fn a_result_needs_an_intent_and_is_unique() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let i = intent(1, p.id, 0, JournalMode::Live);
    // Ordering: no result before its intent.
    assert_eq!(
        f.repos
            .journal
            .record_result(&result(&i))
            .await
            .unwrap_err()
            .class,
        ErrorClass::NotFound
    );
    f.repos.journal.record_intent(&i).await.unwrap();
    f.repos.journal.record_result(&result(&i)).await.unwrap();
    assert_eq!(
        f.repos
            .journal
            .record_result(&result(&i))
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );
    assert_eq!(
        f.repos.journal.record_intent(&i).await.unwrap_err().class,
        ErrorClass::Conflict
    );
}

#[tokio::test]
async fn a_live_intent_requires_an_executing_plan() {
    let f = fx().await;
    let draft = insert_plan(&f, 1, 1).await;
    let err = f
        .repos
        .journal
        .record_intent(&intent(1, draft.id, 0, JournalMode::Live))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
    // Simulated intents are allowed for a draft (dry-run of a plan before approval).
    f.repos
        .journal
        .record_intent(&intent(2, draft.id, 0, JournalMode::Simulated))
        .await
        .unwrap();
    // Unknown action: foreign key.
    let p = executing_plan(&f, 2, 1).await;
    let err = f
        .repos
        .journal
        .record_intent(&intent(3, p.id, 5, JournalMode::Live))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
}

#[tokio::test]
async fn unfinished_lists_live_intents_without_a_result_only() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 3).await;
    let done = intent(1, p.id, 0, JournalMode::Live);
    let open = intent(2, p.id, 1, JournalMode::Live);
    let sim = intent(3, p.id, 2, JournalMode::Simulated);
    for i in [&done, &open, &sim] {
        f.repos.journal.record_intent(i).await.unwrap();
    }
    f.repos.journal.record_result(&result(&done)).await.unwrap();
    let unfinished = f.repos.journal.unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].intent, open);
}

#[tokio::test]
async fn interrupted_action_is_visible_after_a_restart() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 2).await;
    let open = intent(1, p.id, 0, JournalMode::Live);
    f.repos.journal.record_intent(&open).await.unwrap();
    // "Crash": the process dies between the intent and its result. Reopen from the file.
    let path = f.path();
    f.db.clone().close().await;
    let db = Db::open(&path).await.unwrap();
    let repos = removarr_storage::Repos::new(&db, f.clock.clone());

    let snap = repos.recovery.snapshot().await.unwrap();
    assert!(snap.needs_recovery());
    assert_eq!(snap.executing_plans, vec![p.id]);
    assert_eq!(snap.unfinished.len(), 1);
    assert_eq!(snap.unfinished_by_plan()[&p.id][0].id, open.id);

    // Recovery resolves it: result recorded, plan interrupted, nothing is left to recover.
    repos
        .journal
        .record_result(&ResultRecord {
            intent_id: open.id,
            outcome: StepOutcome::Skipped {
                reason: "observed state recorded".into(),
            },
            at: Timestamp(9),
        })
        .await
        .unwrap();
    repos
        .plans
        .transition(p.id, PlanStatus::Executing, PlanStatus::Interrupted)
        .await
        .unwrap();
    assert!(!repos.recovery.snapshot().await.unwrap().needs_recovery());
    db.close().await;
}

#[tokio::test]
async fn intent_is_durable_in_the_wal_before_record_intent_returns() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let i = intent(1, p.id, 0, JournalMode::Live);
    f.repos.journal.record_intent(&i).await.unwrap();

    // Simulate a power cut right after the call returned: copy the database file set as it is
    // on disk (main file + WAL, no clean shutdown) and open the copy.
    let crash = tempfile::tempdir().unwrap();
    for ext in ["db", "db-wal"] {
        let src = f.path().with_extension(ext);
        if src.exists() {
            std::fs::copy(&src, crash.path().join(format!("removarr.{ext}"))).unwrap();
        }
    }
    let db = Db::open(&crash.path().join("removarr.db")).await.unwrap();
    let repos = removarr_storage::Repos::new(&db, f.clock.clone());
    let found = repos.journal.unfinished().await.unwrap();
    assert_eq!(found.len(), 1, "intent survived without any checkpoint");
    assert_eq!(found[0].intent, i);
    assert_eq!(f.db.pragma("synchronous").await.unwrap(), "2");
    db.close().await;
}

#[tokio::test]
async fn the_intent_commit_is_visible_to_other_connections_immediately() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    f.repos
        .journal
        .record_intent(&intent(1, p.id, 0, JournalMode::Live))
        .await
        .unwrap();
    let mut raw = f.raw().await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal_intents")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn outcome_recording_follows_the_action_state_machine() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 2).await;
    let a = |i| ActionRef {
        plan: p.id,
        index: i,
    };
    let unverified = ActionOutcome::ExecutedUnverified {
        reason: "recheck pending".into(),
    };

    f.repos
        .journal
        .record_outcome(a(0), &unverified)
        .await
        .unwrap();
    assert_eq!(
        f.repos.plans.actions(p.id).await.unwrap()[0].state,
        ActionState::ExecutedUnverified
    );
    // Idempotent re-record of the same outcome.
    f.repos
        .journal
        .record_outcome(a(0), &unverified)
        .await
        .unwrap();
    // Re-verification may upgrade it...
    f.repos
        .journal
        .record_outcome(a(0), &ActionOutcome::Verified)
        .await
        .unwrap();
    // ...and a final outcome cannot be rewritten.
    let err: PortError = f
        .repos
        .journal
        .record_outcome(a(0), &unverified)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
    assert_eq!(
        f.repos
            .journal
            .record_outcome(a(9), &ActionOutcome::Verified)
            .await
            .unwrap_err()
            .class,
        ErrorClass::NotFound
    );
    assert_eq!(f.repos.plans.actions(p.id).await.unwrap()[1].outcome, None);
}

#[tokio::test]
async fn live_audit_rows_cannot_be_deleted_or_edited() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let i = intent(1, p.id, 0, JournalMode::Live);
    f.repos.journal.record_intent(&i).await.unwrap();
    f.repos.journal.record_result(&result(&i)).await.unwrap();
    let mut raw = f.raw().await;
    for sql in [
        "DELETE FROM journal_results",
        "DELETE FROM journal_intents",
        "UPDATE journal_intents SET step = 'x'",
        "UPDATE journal_results SET outcome = '{}'",
        "DELETE FROM plan_actions",
        "DELETE FROM plans",
        "DELETE FROM approvals",
        "UPDATE approvals SET confirmed_count = 0",
        "UPDATE plans SET body = '{}'",
    ] {
        assert!(
            sqlx::query(sql).execute(&mut raw).await.is_err(),
            "{sql} must be refused"
        );
    }
}

#[tokio::test]
async fn journal_paths_roundtrip_exactly_but_urls_and_secrets_are_redacted() {
    let f = fx().await;
    let p = executing_plan(&f, 7, 1).await;
    let mut i = intent(70, p.id, 0, JournalMode::Live);
    i.target = json!({
        "path": {"root": "media", "rel": "Key: Basic Digest/a.mkv"},
        "note": "https://tracker.example/announce?passkey=pk123456",
    });
    i.observed = json!({
        "path": {"root": "media", "rel": "Key: Basic Digest/a.mkv"},
        "url": "http://t.example/x?apikey=tok98765",
    });
    f.repos.journal.record_intent(&i).await.unwrap();

    let all = f.repos.journal.for_plan(p.id).await.unwrap();
    let got = &all[0].0;
    assert_eq!(got.target["path"], i.target["path"]);
    assert_eq!(got.observed["path"], i.observed["path"]);
    let flat = format!("{} {}", got.target, got.observed);
    for leaked in ["pk123456", "tok98765", "tracker.example", "t.example"] {
        assert!(!flat.contains(leaked), "{leaked} in {flat}");
    }
}
