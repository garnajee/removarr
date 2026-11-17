//! Tier-4 fixtures: a new completed release `New/new.mkv` held by torrent `n`, an old media
//! file `Movies/Rel/a.mkv` that ARR currently serves for episode 11 of series 5, and plan
//! builders for every tier-4 kind.

use std::fs;

use removarr_domain::arr::{
    ArrService, ManualImportRequest, OldFileRecovery, ReleaseRef, ReleaseSearchResult,
};
use removarr_domain::path::RootId;
use removarr_domain::plan::{
    ActionKind, ActionTarget, PlannedAction, Precondition, VerificationStep,
};
use removarr_domain::session::ServiceKind;

use removarr_domain::ports::PlanStore;

use super::arrfake::{decision, episode, media_ref};
use super::{Lab, hash, rp};

pub const OLD_MEDIA: &str = "Movies/Rel/a.mkv";
pub const NEW_SOURCE: &str = "New/new.mkv";

pub fn search_result(guid: &str) -> ReleaseSearchResult {
    ReleaseSearchResult {
        release: ReleaseRef {
            guid: guid.into(),
            indexer_id: 1,
        },
        indexer: "i".into(),
        title: "Rel".into(),
        size: 1,
        seeders: None,
        leechers: None,
        age_days: 1,
        protocol: "torrent".into(),
        quality: "Bluray-1080p".into(),
        custom_formats: vec![],
        cf_score: 10,
        approved: true,
        rejections: vec![],
        download_allowed: true,
    }
}

impl Lab {
    /// Size of the old media file (its attributable gain once freed).
    pub fn sig_len_of_old(&self) -> u64 {
        self.sig(&rp(&self.media, OLD_MEDIA)).size
    }

    /// The new release, its torrent, ARR's state, the preview; a newer stored observation.
    pub async fn t4_setup(&self) {
        fs::create_dir_all(self.completed_path("New")).unwrap();
        fs::create_dir_all(self.media_path("Movies/New")).unwrap();
        fs::write(self.completed_path(NEW_SOURCE), vec![3u8; 8192]).unwrap();
        self.qbit.add(
            'e',
            &RootId::completed(),
            "New",
            &[(NEW_SOURCE, NEW_SOURCE)],
        );
        let src = rp(&RootId::completed(), NEW_SOURCE);
        let file = self.arr.file_entry(1, OLD_MEDIA);
        self.arr.with(|s| {
            s.dest_dir = "Movies/New".into();
            s.episodes = vec![episode(11, Some(1), true)];
            s.files = vec![file];
            s.previews
                .insert(rp(&RootId::completed(), "New"), vec![decision(&src, &[11])]);
        });
        self.store_observation_again().await;
    }

    /// `ReplaceRelease` of `NEW_SOURCE` for episode 11, the old file `OLD_MEDIA` recovered as
    /// `recovery`.
    pub fn replace_action(&self, index: u32, recovery: OldFileRecovery) -> PlannedAction {
        let src = rp(&RootId::completed(), NEW_SOURCE);
        let old = rp(&self.media, OLD_MEDIA);
        let (src_sig, old_sig) = (self.sig(&src), self.sig(&old));
        let m = media_ref(5, &[11]);
        let whole = media_ref(5, &[]);
        let mut a = self.base(
            index,
            ActionKind::ReplaceRelease,
            ActionTarget::Replace {
                request: ManualImportRequest {
                    files: vec![(src.clone(), m.clone())],
                },
            },
        );
        let (pins, _) = self.torrent_pins(&['e']);
        a.preconditions.extend(pins);
        a.preconditions.extend([
            Precondition::ServiceAvailable(ServiceKind::Sonarr),
            Precondition::ArrFingerprint {
                media: whole.clone(),
                fingerprint: self.arr.fingerprint(5),
            },
            Precondition::EntryUnchanged {
                path: src.clone(),
                sig: src_sig,
                compare_nlink: false,
            },
            Precondition::NoScanErrorUnder { path: src.clone() },
            Precondition::SameDevice {
                a: src.clone(),
                b: old.clone(),
            },
            Precondition::EntryUnchanged {
                path: old.clone(),
                sig: old_sig,
                compare_nlink: false,
            },
            Precondition::OldCoverageRecoverable {
                old_files: vec![(old.clone(), old_sig.phys(), recovery.clone())],
            },
            Precondition::NoArrImport {
                media: whole,
                hashes: [hash('e')].into(),
            },
        ]);
        a.verification = vec![
            VerificationStep::ArrCurrentFile {
                media: m,
                path: src.clone(),
            },
            VerificationStep::PathStillHasInode {
                path: src,
                phys: src_sig.phys(),
            },
            VerificationStep::OldFileAccounted {
                path: old,
                recovery,
            },
        ];
        a.torrent_hashes = [hash('e')].into();
        a
    }

    /// `DeleteMediaFile` of the old media file, after the replacement at `dep`.
    pub fn delete_media_action(&self, index: u32, dep: u32) -> PlannedAction {
        let old = rp(&self.media, OLD_MEDIA);
        let sig = self.sig(&old);
        let mut a = self.base(
            index,
            ActionKind::DeleteMediaFile,
            ActionTarget::Path { path: old.clone() },
        );
        a.preconditions.push(Precondition::EntryUnchanged {
            path: old.clone(),
            sig,
            compare_nlink: true,
        });
        a.verification = vec![VerificationStep::Absent { path: old }];
        a.expected_gain = removarr_domain::gain::GainEstimate {
            logical: sig.size,
            attributable: if sig.nlink == 1 { sig.size } else { 0 },
            added: 0,
        };
        a.depends_on = vec![dep];
        a
    }

    pub fn grab_action(&self, index: u32, guid: &str) -> PlannedAction {
        let m = media_ref(5, &[11]);
        let release = ReleaseRef {
            guid: guid.into(),
            indexer_id: 1,
        };
        let mut a = self.base(
            index,
            ActionKind::ArrGrabRelease,
            ActionTarget::Release {
                media: m,
                release: release.clone(),
            },
        );
        a.preconditions.extend([
            Precondition::ServiceAvailable(ServiceKind::Sonarr),
            Precondition::ArrFingerprint {
                media: media_ref(5, &[]),
                fingerprint: self.arr.fingerprint(5),
            },
        ]);
        a.verification = vec![VerificationStep::ArrQueueHasRelease {
            service: ArrService::Sonarr,
            release,
        }];
        a
    }

    pub fn queue_action(&self, index: u32, id: i64) -> PlannedAction {
        let mut a = self.base(
            index,
            ActionKind::ArrRemoveQueueItem,
            ActionTarget::Queue {
                service: ArrService::Sonarr,
                id,
            },
        );
        a.preconditions
            .push(Precondition::ServiceAvailable(ServiceKind::Sonarr));
        a.verification = vec![VerificationStep::ArrQueueItemAbsent {
            service: ArrService::Sonarr,
            id,
        }];
        a
    }

    pub fn monitor_action(&self, index: u32, monitored: bool) -> PlannedAction {
        let m = media_ref(5, &[11]);
        let mut a = self.base(
            index,
            ActionKind::ArrSetMonitored,
            ActionTarget::Monitor {
                media: m.clone(),
                monitored,
            },
        );
        a.preconditions
            .push(Precondition::ServiceAvailable(ServiceKind::Sonarr));
        a.verification = vec![VerificationStep::ArrMonitored {
            media: m,
            monitored,
        }];
        a
    }
}

impl Lab {
    /// Like `ticket`, but acknowledging `unrecoverable` old files in the approval.
    pub async fn ticket_ack(
        &self,
        plan: removarr_domain::plan::HashedPlan,
        simulate: bool,
        unrecoverable: Vec<removarr_domain::path::RootedPath>,
    ) -> removarr_workflows::plans::ExecutionTicket {
        use removarr_domain::plan::{ApprovalRequest, PlanStatus};
        let (p, h) = plan.clone().into_parts();
        self.repos.plans.insert(&p, &h).await.unwrap();
        let req = ApprovalRequest {
            plan_hash: h.clone(),
            confirmed_count: p.required_count(),
            confirmed_torrent_removals: p.required_torrent_removals(),
            acknowledged_samples: p.samples.clone(),
            acknowledged_unrecoverable: unrecoverable,
        };
        let approval = plan.approve(&req, super::NOW).unwrap();
        self.repos.plans.approve(&approval).await.unwrap();
        removarr_workflows::plans::ExecutionTicket {
            plan,
            status: PlanStatus::Approved,
            approval: Some(approval),
            simulate,
        }
    }
}

impl Lab {
    /// A second episode (12): `New/new2.mkv` for the old `Movies/Rel/b.mkv` (ARR file 2).
    pub async fn t4_setup_two(&self) {
        self.t4_setup().await;
        fs::write(self.completed_path("New/new2.mkv"), vec![4u8; 4096]).unwrap();
        self.qbit
            .add_file('e', "New/new2.mkv", &RootId::completed(), "New/new2.mkv");
        let src2 = rp(&RootId::completed(), "New/new2.mkv");
        let file2 = self.arr.file_entry(2, "Movies/Rel/b.mkv");
        self.arr.with(|s| {
            s.episodes.push(episode(12, Some(2), true));
            s.files.push(file2);
            s.previews
                .get_mut(&rp(&RootId::completed(), "New"))
                .unwrap()
                .push(decision(&src2, &[12]));
        });
        self.store_observation_third().await;
    }

    /// One `ReplaceRelease` of several sources: `(source rel, episode, old media rel)`, every
    /// old file recovered through the recycle bin.
    pub fn replace_multi(&self, index: u32, items: &[(&str, i64, &str)]) -> PlannedAction {
        let whole = media_ref(5, &[]);
        let mut files = Vec::new();
        let mut a = self.base(
            index,
            ActionKind::ReplaceRelease,
            ActionTarget::Replace {
                request: ManualImportRequest { files: vec![] },
            },
        );
        let (pins, _) = self.torrent_pins(&['e']);
        a.preconditions.extend(pins);
        a.preconditions.extend([
            Precondition::ServiceAvailable(ServiceKind::Sonarr),
            Precondition::ArrFingerprint {
                media: whole.clone(),
                fingerprint: self.arr.fingerprint(5),
            },
            Precondition::NoArrImport {
                media: whole,
                hashes: [hash('e')].into(),
            },
        ]);
        let mut old_files = Vec::new();
        for (src_rel, ep, old_rel) in items {
            let src = rp(&RootId::completed(), src_rel);
            let old = rp(&self.media, old_rel);
            let (ss, os) = (self.sig(&src), self.sig(&old));
            let m = media_ref(5, &[*ep]);
            files.push((src.clone(), m.clone()));
            a.preconditions.extend([
                Precondition::EntryUnchanged {
                    path: src.clone(),
                    sig: ss,
                    compare_nlink: false,
                },
                Precondition::NoScanErrorUnder { path: src.clone() },
                Precondition::SameDevice {
                    a: src.clone(),
                    b: old.clone(),
                },
                Precondition::EntryUnchanged {
                    path: old.clone(),
                    sig: os,
                    compare_nlink: false,
                },
            ]);
            old_files.push((old.clone(), os.phys(), OldFileRecovery::RecycleBin));
            a.verification.extend([
                VerificationStep::ArrCurrentFile {
                    media: m,
                    path: src.clone(),
                },
                VerificationStep::PathStillHasInode {
                    path: src,
                    phys: ss.phys(),
                },
                VerificationStep::OldFileAccounted {
                    path: old,
                    recovery: OldFileRecovery::RecycleBin,
                },
            ]);
        }
        a.preconditions
            .push(Precondition::OldCoverageRecoverable { old_files });
        a.target = ActionTarget::Replace {
            request: ManualImportRequest { files },
        };
        a.torrent_hashes = [hash('e')].into();
        a
    }

    /// `DeleteMediaFile` of `rel` after the replacement at `dep`.
    pub fn delete_media_of(&self, index: u32, dep: u32, rel: &str) -> PlannedAction {
        let old = rp(&self.media, rel);
        let sig = self.sig(&old);
        let mut a = self.base(
            index,
            ActionKind::DeleteMediaFile,
            ActionTarget::Path { path: old.clone() },
        );
        a.preconditions.push(Precondition::EntryUnchanged {
            path: old.clone(),
            sig,
            compare_nlink: true,
        });
        a.verification = vec![VerificationStep::Absent { path: old }];
        a.expected_gain = removarr_domain::gain::GainEstimate {
            logical: sig.size,
            attributable: if sig.nlink == 1 { sig.size } else { 0 },
            added: 0,
        };
        a.depends_on = vec![dep];
        a
    }

    /// An import that is already correct: ARR's current file for episode 11 is a hardlink of
    /// the completed file, and ARR's queue holds a stale item (id 7) of torrent `e`.
    pub async fn t4_proven_queue(&self) {
        fs::hard_link(
            self.completed_path(NEW_SOURCE),
            self.media_path("Movies/New/new.mkv"),
        )
        .unwrap();
        let file = self.arr.file_entry(1, "Movies/New/new.mkv");
        self.arr.with(|s| {
            s.files = vec![file];
            s.episodes[0].file_id = Some(1);
            s.queue = vec![super::arrfake::queue_item(
                7,
                Some(hash('e')),
                "failed",
                Some(media_ref(5, &[11])),
            )];
        });
        self.store_observation_third().await;
    }
}
