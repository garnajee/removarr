//! Explanations of incomplete data, plan versus result differences and remaining actions.
//! Pure functions over already loaded values: no I/O, no prose beyond a short English `text`
//! that exports use (the UI translates `code` + `params`).

use std::collections::BTreeMap;

use crate::model::{
    ActionReportVm, ExplanationVm, GainTotalsVm, PlanGainsVm, RemainingVm, ServiceVm, SessionVm,
};
use crate::redact::sanitize;

#[must_use]
pub fn ex(code: &'static str, params: &[(&str, String)], text: String) -> ExplanationVm {
    ExplanationVm {
        code,
        params: params
            .iter()
            .map(|(k, v)| ((*k).to_string(), sanitize(v)))
            .collect::<BTreeMap<_, _>>(),
        text: sanitize(&text),
    }
}

/// The caveat shown next to every space chart (MASTER section 6).
#[must_use]
pub fn btrfs_caveat() -> ExplanationVm {
    ex(
        "caveat.btrfs_snapshots_may_retain_space",
        &[],
        "Snapshots (for example Btrfs), other writers and pending qBittorrent or ARR work can \
         keep deleted space in use, so the observed free-space delta can be lower than the \
         attributable gain."
            .to_string(),
    )
}

/// Why the data of a scan is not complete or not current.
#[must_use]
pub fn session_explanations(
    session: Option<&SessionVm>,
    issues_session: Option<&str>,
    services: &[ServiceVm],
    incomplete_issues: u64,
) -> Vec<ExplanationVm> {
    let mut out = Vec::new();
    let Some(s) = session else {
        out.push(ex(
            "scan.none",
            &[],
            "No scan has finished yet: nothing below describes the current state.".to_string(),
        ));
        return out;
    };
    let when = s
        .finished_at_iso
        .clone()
        .unwrap_or_else(|| s.started_at_iso.clone());
    match s.status.as_str() {
        "complete" => {}
        "complete_with_errors" => out.push(ex(
            "scan.complete_with_errors",
            &[("session", s.id.clone()), ("errors", s.scan_errors.to_string())],
            format!(
                "The scan of {when} finished with {} error(s): paths under an error are unknown, not clean.",
                s.scan_errors
            ),
        )),
        other => out.push(ex(
            "scan.not_complete",
            &[("session", s.id.clone()), ("status", other.to_string())],
            format!("The scan of {when} ended as '{other}': its data is incomplete and cannot justify any action."),
        )),
    }
    for r in &s.roots {
        if !r.scanned {
            out.push(ex(
                "scan.root_not_scanned",
                &[("root", r.root.clone())],
                format!("Root '{}' was not scanned in this session.", r.root),
            ));
        }
        if r.error_count > 0 {
            out.push(ex(
                "scan.root_errors",
                &[
                    ("root", r.root.clone()),
                    ("count", r.error_count.to_string()),
                ],
                format!("Root '{}' had {} scan error(s).", r.root, r.error_count),
            ));
        }
    }
    if s.stale {
        out.push(ex(
            "scan.stale",
            &[("finished_at", when.clone())],
            format!("The last scan is older than twice the scan interval (finished {when}): this is history, not the current state."),
        ));
    }
    if let Some(i) = issues_session
        && i != s.id
    {
        out.push(ex(
            "issues.from_older_session",
            &[
                ("issues_session", i.to_string()),
                ("last_scan", s.id.clone()),
            ],
            "The issue list comes from an earlier session than the latest scan.".to_string(),
        ));
    }
    for svc in services.iter().filter(|v| v.problem) {
        out.push(ex(
            "service.not_ok",
            &[
                ("service", svc.service.clone()),
                ("state", svc.state_key.to_string()),
                ("observed_at", svc.observed_at_iso.clone()),
            ],
            format!(
                "{} was '{}' when observed at {}: evidence that depends on it is incomplete.",
                svc.service, svc.state_key, svc.observed_at_iso
            ),
        ));
    }
    if incomplete_issues > 0 {
        out.push(ex(
            "issues.incomplete_evidence",
            &[("count", incomplete_issues.to_string())],
            format!("{incomplete_issues} issue(s) rest on incomplete or unknown evidence; they stay informational or blocked."),
        ));
    }
    out
}

/// Inputs of [`plan_differences`].
#[derive(Debug, Clone)]
pub struct PlanFacts<'a> {
    pub status: &'a str,
    pub mode: &'a str,
    pub planned: PlanGainsVm,
    pub recorded: &'a GainTotalsVm,
    pub actions: &'a [ActionReportVm],
    /// Sum of local observed free-space deltas (None when none were measured).
    pub observed_delta: Option<i64>,
    pub unfinished_intents: u64,
}

/// Differences between what the plan promised and what happened.
#[must_use]
pub fn plan_differences(f: &PlanFacts<'_>) -> Vec<ExplanationVm> {
    let mut out = Vec::new();
    if f.mode == "dry_run" {
        out.push(ex(
            "plan.dry_run",
            &[],
            "This plan is a dry run: nothing was changed and no space was freed.".to_string(),
        ));
        return out;
    }
    if matches!(f.status, "draft" | "approved" | "executing") {
        out.push(ex(
            "plan.not_finished",
            &[("status", f.status.to_string())],
            format!("The plan is '{}': results are not final.", f.status),
        ));
    }
    if f.unfinished_intents > 0 {
        out.push(ex(
            "plan.unfinished_intents",
            &[("count", f.unfinished_intents.to_string())],
            format!(
                "{} journaled step(s) have no result yet: recovery must resolve them.",
                f.unfinished_intents
            ),
        ));
    }
    for a in f.actions {
        if !matches!(a.state.as_str(), "verified" | "pending") {
            out.push(ex(
                "action.not_verified",
                &[
                    ("index", a.index.to_string()),
                    ("kind", a.kind.clone()),
                    ("outcome", a.outcome.key.clone()),
                ],
                format!(
                    "Action {} ({}) ended as '{}', not as verified.",
                    a.index, a.kind, a.outcome.key
                ),
            ));
        }
        if a.state == "verified" && a.expected_attributable > 0 && a.recorded_attributable == 0 {
            out.push(ex(
                "gain.not_recorded",
                &[("index", a.index.to_string())],
                format!("Action {} was verified but no attributable gain was recorded (the inode may still be held elsewhere).", a.index),
            ));
        } else if a.state == "verified" && a.recorded_attributable != a.expected_attributable {
            out.push(ex(
                "gain.differs_from_plan",
                &[
                    ("index", a.index.to_string()),
                    ("expected", a.expected_attributable.to_string()),
                    ("recorded", a.recorded_attributable.to_string()),
                ],
                format!(
                    "Action {} expected {} attributable byte(s) and recorded {}.",
                    a.index, a.expected_attributable, a.recorded_attributable
                ),
            ));
        }
    }
    if f.recorded.attributable_bytes != f.planned.attributable && !f.actions.is_empty() {
        out.push(ex(
            "gain.total_differs_from_plan",
            &[
                ("planned", f.planned.attributable.to_string()),
                ("recorded", f.recorded.attributable_bytes.to_string()),
            ],
            format!(
                "The plan expected {} attributable byte(s); {} were recorded.",
                f.planned.attributable, f.recorded.attributable_bytes
            ),
        ));
    }
    if f.recorded.added_bytes != f.planned.added {
        out.push(ex(
            "cost.differs_from_plan",
            &[
                ("planned", f.planned.added.to_string()),
                ("recorded", f.recorded.added_bytes.to_string()),
            ],
            format!(
                "Added cost was estimated at {} byte(s) and recorded as {}.",
                f.planned.added, f.recorded.added_bytes
            ),
        ));
    }
    if let Some(obs) = f.observed_delta {
        let attributable = i64::try_from(f.recorded.attributable_bytes).unwrap_or(i64::MAX);
        if obs != attributable {
            out.push(ex(
                "observed.differs_from_attributable",
                &[("observed", obs.to_string()), ("attributable", attributable.to_string())],
                format!(
                    "The observed free-space delta ({obs} bytes) differs from the attributable gain \
                     ({attributable} bytes); snapshots and other writers can explain this."
                ),
            ));
        }
    } else if f.status != "draft" {
        out.push(ex(
            "observed.missing",
            &[],
            "No free-space delta was measured for this plan.".to_string(),
        ));
    }
    out
}

/// Actions that still need something, with the next step.
#[must_use]
pub fn remaining_actions(actions: &[ActionReportVm], plan_mode: &str) -> Vec<RemainingVm> {
    if plan_mode == "dry_run" {
        return Vec::new();
    }
    actions
        .iter()
        .filter_map(|a| {
            let (next, text) = match a.state.as_str() {
                "verified" => return None,
                "pending" => ("run_or_discard", "Not executed yet: run the plan or discard it."),
                "running" | "verifying" | "compensating" => (
                    "wait_or_recover",
                    "Still in progress or interrupted: wait, or let recovery resolve it.",
                ),
                "executed_unverified" => (
                    "verify_manually",
                    "Executed but not proven: check the result by hand before relying on it.",
                ),
                "failed_needs_review" => (
                    "review_manually",
                    "Failed in a way that needs a human review of the journal.",
                ),
                _ => (
                    "rescan_and_replan",
                    "Nothing (or nothing lasting) was done: rescan and plan again if the issue is still present.",
                ),
            };
            Some(RemainingVm {
                index: a.index,
                kind: a.kind.clone(),
                state: a.state.clone(),
                next_step: next,
                text: text.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::{OutcomeVm, SessionRootVm};

    fn action(index: u32, state: &str, exp: u64, rec: u64) -> ActionReportVm {
        ActionReportVm {
            index,
            kind: "k".into(),
            state: state.into(),
            outcome: OutcomeVm {
                key: state.into(),
                success: state == "verified",
                needs_attention: false,
                detail: None,
            },
            expected_logical: exp,
            expected_attributable: exp,
            expected_added: 0,
            recorded_attributable: rec,
            recorded_added: 0,
            gain_ids: vec![],
            updated_at: 0,
            updated_at_iso: String::new(),
        }
    }

    fn totals(a: u64) -> GainTotalsVm {
        GainTotalsVm {
            attributable_bytes: a,
            added_bytes: 0,
            net_bytes: a as i64,
            unique_gain_ids: 1,
            duplicate_gain_ids_ignored: 0,
        }
    }

    fn session(status: &str, stale: bool) -> SessionVm {
        SessionVm {
            id: "S1".into(),
            started_at: 1,
            started_at_iso: "t0".into(),
            finished_at: Some(2),
            finished_at_iso: Some("t1".into()),
            status: status.into(),
            complete: status == "complete",
            age_ms: Some(5),
            stale,
            entries_scanned: 1,
            scan_errors: 2,
            duration_ms: 1,
            roots: vec![
                SessionRootVm {
                    root: "completed".into(),
                    scanned: false,
                    error_count: 0,
                    protected_count: 0,
                },
                SessionRootVm {
                    root: "media-aaaaaaaa".into(),
                    scanned: true,
                    error_count: 3,
                    protected_count: 0,
                },
            ],
        }
    }

    #[test]
    fn incomplete_session_is_explained_per_cause() {
        let e = session_explanations(Some(&session("cancelled", true)), Some("S0"), &[], 4);
        let codes: Vec<_> = e.iter().map(|x| x.code).collect();
        for c in [
            "scan.not_complete",
            "scan.root_not_scanned",
            "scan.root_errors",
            "scan.stale",
            "issues.from_older_session",
            "issues.incomplete_evidence",
        ] {
            assert!(codes.contains(&c), "{c} in {codes:?}");
        }
        assert_eq!(
            session_explanations(None, None, &[], 0)[0].code,
            "scan.none"
        );
        assert!(
            session_explanations(
                Some(&{
                    let mut s = session("complete", false);
                    s.roots.clear();
                    s
                }),
                Some("S1"),
                &[],
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn plan_differences_cover_gain_observed_and_status() {
        let planned = PlanGainsVm {
            logical: 10,
            attributable: 10,
            added: 0,
            net: 10,
        };
        let actions = [action(0, "verified", 10, 4), action(1, "failed", 0, 0)];
        let t = totals(4);
        let d = plan_differences(&PlanFacts {
            status: "partially_executed",
            mode: "live",
            planned,
            recorded: &t,
            actions: &actions,
            observed_delta: Some(1),
            unfinished_intents: 1,
        });
        let codes: Vec<_> = d.iter().map(|x| x.code).collect();
        for c in [
            "plan.unfinished_intents",
            "action.not_verified",
            "gain.differs_from_plan",
            "gain.total_differs_from_plan",
            "observed.differs_from_attributable",
        ] {
            assert!(codes.contains(&c), "{c} in {codes:?}");
        }
        let dry = plan_differences(&PlanFacts {
            status: "executed",
            mode: "dry_run",
            planned,
            recorded: &t,
            actions: &actions,
            observed_delta: None,
            unfinished_intents: 0,
        });
        assert_eq!(dry.len(), 1);
        assert_eq!(dry[0].code, "plan.dry_run");
    }

    #[test]
    fn remaining_lists_every_unfinished_action_with_a_next_step() {
        let actions = [
            action(0, "verified", 0, 0),
            action(1, "pending", 0, 0),
            action(2, "executed_unverified", 0, 0),
            action(3, "failed_needs_review", 0, 0),
            action(4, "compensated", 0, 0),
            action(5, "running", 0, 0),
        ];
        let r = remaining_actions(&actions, "live");
        let steps: Vec<_> = r.iter().map(|x| (x.index, x.next_step)).collect();
        assert_eq!(
            steps,
            [
                (1, "run_or_discard"),
                (2, "verify_manually"),
                (3, "review_manually"),
                (4, "rescan_and_replan"),
                (5, "wait_or_recover")
            ]
        );
        assert!(remaining_actions(&actions, "dry_run").is_empty());
    }
}
