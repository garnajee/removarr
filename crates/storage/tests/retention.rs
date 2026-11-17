#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_domain::fs::{IoKind, ScanErrorRecord};
use removarr_domain::gain::GainEstimate;
use removarr_domain::ids::{ActionRef, IntentId, IssueId, RawUuid, SubjectKey};
use removarr_domain::issue::{Issue, IssueCategory, Outcome, Severity, Unit};
use removarr_domain::journal::{IntentRecord, JournalMode, JournalStep, ResultRecord, StepOutcome};
use removarr_domain::path::RootId;
use removarr_domain::ports::{IssueStore, JournalStore, ScanStore};
use removarr_domain::time::Timestamp;
use removarr_storage::PruneReport;
use serde_json::json;

fn intent(n: u128, plan: removarr_domain::ids::PlanId, mode: JournalMode) -> IntentRecord {
    IntentRecord {
        id: IntentId::new(RawUuid::from_u128(0x2000 + n)),
        action: ActionRef { plan, index: 0 },
        step: JournalStep::FsRemoveFile,
        mode,
        target: json!({}),
        observed: json!({}),
        expected_gain: GainEstimate::default(),
        hashes: BTreeSet::new(),
        created_at: Timestamp(1),
    }
}

fn error(rel: &str) -> ScanErrorRecord {
    ScanErrorRecord {
        path: rp(rel),
        io: IoKind::PermissionDenied,
        message: "denied".into(),
    }
}

#[tokio::test]
async fn prune_removes_only_old_derived_data_and_never_audit_evidence() {
    let f = fx().await;
    // Two scans of the same root; the second is the current inventory.
    let s1 = begin(&f, 1).await;
    let s2 = begin(&f, 2).await;
    let mut inv1 = inventory(vec![entry("x", 1, 1), entry("y", 2, 1)], true);
    inv1.errors.push(error("old-error"));
    f.repos.scans.save_inventory(s1.id, &inv1).await.unwrap();
    let mut inv2 = inventory(vec![entry("x", 1, 1)], true);
    inv2.errors.push(error("current-error"));
    f.repos.scans.save_inventory(s2.id, &inv2).await.unwrap(); // y becomes gone

    // Resolved issue.
    let subject = SubjectKey("p:completed:i".into());
    let issue = Issue {
        id: IssueId::derive(IssueCategory::Junk, &subject),
        category: IssueCategory::Junk,
        subject,
        unit: Unit::File(rp("i")),
        severity: Severity::Info,
        outcome: Outcome::KeepManual,
        evidence: vec![],
        blockers: vec![],
        torrent_hashes: BTreeSet::new(),
        logical_bytes: 1,
        potential_bytes: 1,
        age_basis: Timestamp(0),
        first_seen_at: Timestamp(0),
        session: s1.id,
        physical_ids: BTreeSet::new(),
    };
    f.repos
        .issues
        .replace_current(s1.id, &[issue])
        .await
        .unwrap();
    f.repos.issues.replace_current(s2.id, &[]).await.unwrap();

    // Audit evidence: a live intent with a result, and a simulated intent with a result.
    let p = executing_plan(&f, 3, 1).await;
    let live = intent(1, p.id, JournalMode::Live);
    let sim = intent(2, p.id, JournalMode::Simulated);
    for i in [&live, &sim] {
        f.repos.journal.record_intent(i).await.unwrap();
        f.repos
            .journal
            .record_result(&ResultRecord {
                intent_id: i.id,
                outcome: StepOutcome::Simulated,
                at: Timestamp(2),
            })
            .await
            .unwrap();
    }
    f.repos
        .plans
        .record_simulation_run(p.id, Timestamp(1), Timestamp(2))
        .await
        .unwrap();

    let audit_before = f.repos.retention.audit_counts().await.unwrap();
    f.clock.advance_hours(24 * 100);
    let cutoff = Timestamp(f.clock_ms());

    let preview = f.repos.retention.preview(cutoff).await.unwrap();
    assert_eq!(
        preview,
        PruneReport {
            scan_errors: 1,
            protected_zones: 0,
            gone_entries: 1,
            simulated_results: 1,
            simulated_intents: 1,
            simulation_runs: 1,
            resolved_issues: 1,
        }
    );
    // The preview wrote nothing.
    assert_eq!(f.repos.retention.preview(cutoff).await.unwrap(), preview);

    assert_eq!(f.repos.retention.prune(cutoff).await.unwrap(), preview);
    assert_eq!(
        f.repos.retention.prune(cutoff).await.unwrap(),
        PruneReport::default()
    );

    // The current inventory keeps its errors, whatever the cutoff.
    let inv = f
        .repos
        .scans
        .load_inventory(&RootId::completed())
        .await
        .unwrap();
    assert_eq!(inv.errors.len(), 1);
    assert_eq!(inv.errors[0].path, rp("current-error"));
    assert_eq!(inv.entries.len(), 1);

    // Audit evidence is untouched: one live intent/result pair remains, simulated rows are gone.
    let after = f.repos.retention.audit_counts().await.unwrap();
    assert_eq!(after.plans, audit_before.plans);
    assert_eq!(after.plan_actions, audit_before.plan_actions);
    assert_eq!(after.approvals, audit_before.approvals);
    assert_eq!((after.journal_intents, after.journal_results), (1, 1));
    let rows = f.repos.journal.for_plan(p.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.mode, JournalMode::Live);
}

#[tokio::test]
async fn a_cutoff_before_the_data_removes_nothing() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    let mut inv = inventory(vec![entry("x", 1, 1)], true);
    inv.errors.push(error("e"));
    f.repos.scans.save_inventory(s1.id, &inv).await.unwrap();
    assert_eq!(
        f.repos.retention.prune(Timestamp(0)).await.unwrap(),
        PruneReport::default()
    );
}

#[tokio::test]
async fn there_is_no_way_to_delete_audit_rows_even_with_raw_sql() {
    let f = fx().await;
    executing_plan(&f, 1, 1).await;
    let mut raw = f.raw().await;
    for t in [
        "plans",
        "plan_actions",
        "approvals",
        "gains",
        "observed_deltas",
    ] {
        // Empty tables delete zero rows without firing row triggers; plans etc. are populated.
        let r = sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut raw)
            .await;
        if t == "gains" || t == "observed_deltas" {
            assert!(r.is_ok());
        } else {
            assert!(r.is_err(), "{t}");
        }
    }
}
