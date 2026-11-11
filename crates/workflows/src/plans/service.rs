//! One-action service commands: ARR queue removal and monitoring (tier 4), service rescans
//! (tier 2). Live only: a dry-run process cannot plan them (`DryRunOnly`). Each carries the
//! service re-read that verifies it (domain patch 2).

use removarr_domain::arr::{ArrMediaRef, ArrService};
use removarr_domain::evidence::EvidenceLevel;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue};
use removarr_domain::plan::{ActionKind, ActionTarget, Precondition, VerificationStep};
use removarr_domain::session::ServiceKind;

use super::build::{Draft, UnitPlan, UnitResult, issue_completeness};
use super::facts::{Facts, arr_kind};
use super::request::{MediaAction, PlanError};

fn service_blockers(f: &Facts<'_>, kind: ServiceKind) -> Vec<Blocker> {
    if !f.service_configured(kind) {
        vec![Blocker::new(
            BlockerCode::ServiceNotConfigured(kind),
            format!("{} is not configured", kind.as_str()),
        )]
    } else if !f.service_ok(kind) {
        vec![Blocker::new(
            BlockerCode::ServiceUnavailable(kind),
            format!("{} was not available in the latest session", kind.as_str()),
        )]
    } else {
        vec![]
    }
}

fn one(d: Draft<'_>) -> UnitPlan<'_> {
    UnitPlan {
        drafts: vec![d],
        ..UnitPlan::default()
    }
}

/// `ArrRemoveQueueItem` from an `arr_queue` issue (`q:<service>:<id>`). The issue's
/// `QueueItemHidesError` stays a blocker unless the item was proven stale (INV-39).
pub(crate) fn remove_queue_item<'a>(f: &Facts<'a>, issue: &Issue) -> UnitResult<'a> {
    let parsed = issue
        .subject
        .as_str()
        .strip_prefix("q:")
        .and_then(|r| r.split_once(':'))
        .and_then(|(s, id)| {
            let service = match s {
                "sonarr" => ArrService::Sonarr,
                "radarr" => ArrService::Radarr,
                _ => return None,
            };
            Some((service, id.parse::<i64>().ok()?))
        });
    let Some((service, id)) = parsed else {
        return Err(vec![Blocker::new(
            BlockerCode::ArrAmbiguous,
            "the queue subject cannot be read",
        )]);
    };
    let kind = arr_kind(service);
    let b = service_blockers(f, kind);
    if !b.is_empty() {
        return Err(b);
    }
    let mut d = Draft::new(
        ActionKind::ArrRemoveQueueItem,
        ActionTarget::Queue { service, id },
    );
    d.pre = vec![Precondition::ServiceAvailable(kind)];
    d.verify = vec![VerificationStep::ArrQueueItemAbsent { service, id }];
    d.hashes.clone_from(&issue.torrent_hashes);
    d.level = EvidenceLevel::None;
    d.completeness = issue_completeness(issue);
    Ok(one(d))
}

/// `ArrSetMonitored` (CP1-F05). The runner journals the previous value (CTX-06).
pub(crate) fn media_action<'a>(
    f: &Facts<'a>,
    media: &ArrMediaRef,
    action: MediaAction,
) -> (SubjectKey, ActionKind, UnitResult<'a>) {
    let subject = SubjectKey(format!("m:{}:{}", media.service.as_str(), media.media_id));
    let MediaAction::SetMonitored { monitored } = action;
    let kind = arr_kind(media.service);
    let mut b = service_blockers(f, kind);
    match f.input.arr.get(&(media.service, media.media_id)) {
        Some(Ok(_)) => {}
        Some(Err(e)) => b.push(Blocker::new(
            BlockerCode::ServiceUnavailable(kind),
            format!("ARR read failed ({:?})", e.class),
        )),
        None => b.push(Blocker::new(
            BlockerCode::ArrMissing,
            "the media was not read from ARR",
        )),
    }
    let result = if b.is_empty() {
        let mut d = Draft::new(
            ActionKind::ArrSetMonitored,
            ActionTarget::Monitor {
                media: media.clone(),
                monitored,
            },
        );
        d.pre = vec![Precondition::ServiceAvailable(kind)];
        d.verify = vec![VerificationStep::ArrMonitored {
            media: media.clone(),
            monitored,
        }];
        Ok(one(d))
    } else {
        Err(b)
    };
    (subject, ActionKind::ArrSetMonitored, result)
}

/// `ArrRescan` / `JellyfinRefresh` (CP1-F13).
pub(crate) fn rescan<'a>(
    f: &Facts<'a>,
    service: ServiceKind,
    media: Option<&ArrMediaRef>,
) -> Result<(SubjectKey, ActionKind, UnitResult<'a>), PlanError> {
    let subject = SubjectKey(format!("s:{}", service.as_str()));
    let (kind, target, verify) = match (service, media) {
        (ServiceKind::Jellyfin, None) => (
            ActionKind::JellyfinRefresh,
            ActionTarget::JellyfinRefresh { item_id: None },
            VerificationStep::JellyfinRefreshed { item_id: None },
        ),
        (ServiceKind::Sonarr | ServiceKind::Radarr, m) => {
            let arr = if service == ServiceKind::Sonarr {
                ArrService::Sonarr
            } else {
                ArrService::Radarr
            };
            if m.is_some_and(|m| m.service != arr) {
                return Err(PlanError::InvalidRequest(
                    "the media belongs to another service".into(),
                ));
            }
            (
                ActionKind::ArrRescan,
                ActionTarget::Rescan {
                    service: arr,
                    media: m.cloned(),
                },
                VerificationStep::ArrCommandCompleted {
                    service: arr,
                    media: m.cloned(),
                },
            )
        }
        _ => {
            return Err(PlanError::InvalidRequest(format!(
                "{} has no service-side rescan",
                service.as_str()
            )));
        }
    };
    let b = service_blockers(f, service);
    let result = if b.is_empty() {
        let mut d = Draft::new(kind, target);
        d.pre = vec![Precondition::ServiceAvailable(service)];
        d.verify = vec![verify];
        Ok(one(d))
    } else {
        Err(b)
    };
    Ok((subject, kind, result))
}
