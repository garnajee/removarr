#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Hand-built observations for the classifier tests (no filesystem, no network).

pub mod fakes;

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::{
    ArrFileRef, ArrLibraryEntry, ArrMediaRef, ArrQueueItem, ArrService, MediaIdentity, MediaKind,
};
use removarr_domain::evidence::{ContentProof, IdentityProof, MatchCandidate};
use removarr_domain::fs::{
    EntryKind, EntrySignature, FsEntry, IoKind, ProtectedReason, ProtectedZone, RootInventory,
    ScanErrorRecord,
};
use removarr_domain::ids::{RawUuid, SessionId};
use removarr_domain::issue::{BlockerCode, Issue, IssueCategory};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::session::{ServiceState, SessionStatus};
use removarr_domain::time::{MtimeNs, Timestamp};
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, TorrentState,
};
use removarr_workflows::issues::{
    ArrObservation, Classified, IssueSettings, Observation, QbitObservation, classify,
};

pub const DAY_MS: i64 = 86_400_000;
pub const NOW: Timestamp = Timestamp(1_000 * DAY_MS);
pub const OWNER: (u32, u32) = (1000, 1000);

pub fn completed() -> RootId {
    RootId::completed()
}
pub fn incomplete() -> RootId {
    RootId::incomplete()
}
pub fn media() -> RootId {
    RootId::media("/srv/media")
}
pub fn session() -> SessionId {
    SessionId::new(RawUuid::from_u128(7))
}
pub fn rp(root: &RootId, rel: &str) -> RootedPath {
    RootedPath::new(root.clone(), RelPath::new(rel).unwrap())
}
/// A 40-hex hash built from one character (hex digits repeat; other characters use their code).
pub fn hash(c: char) -> TorrentHash {
    let unit = if c.is_ascii_hexdigit() {
        c.to_ascii_lowercase().to_string().repeat(2)
    } else {
        format!("{:02x}", c as u32)
    };
    TorrentHash::new(unit.repeat(20)).unwrap()
}

/// Subject key of a torrent issue.
pub fn tid(c: char) -> String {
    format!("t:{}", hash(c).as_str())
}

/// Inventory builder. Every file is 72 hours old unless `mtime_age_days` says otherwise.
pub struct Inv {
    pub inv: RootInventory,
}

impl Inv {
    pub fn new(root: RootId) -> Self {
        Self {
            inv: RootInventory {
                root: root.clone(),
                root_sig: Some(sig(EntryKind::Dir, 1, 1, 0, 1, 0o755)),
                entries: vec![],
                errors: vec![],
                protected: vec![],
                finished: true,
            },
        }
    }

    pub fn file(self, rel: &str, ino: u64, size: u64) -> Self {
        self.entry(
            rel,
            sig(EntryKind::File, 1, ino, size, 1, 0o644),
            size.div_ceil(4096) * 4096,
        )
    }

    pub fn file_nlink(self, rel: &str, ino: u64, size: u64, nlink: u64) -> Self {
        self.entry(
            rel,
            sig(EntryKind::File, 1, ino, size, nlink, 0o644),
            size.div_ceil(4096) * 4096,
        )
    }

    pub fn file_dev(self, rel: &str, dev: u64, ino: u64, size: u64) -> Self {
        self.entry(
            rel,
            sig(EntryKind::File, dev, ino, size, 1, 0o644),
            size.div_ceil(4096) * 4096,
        )
    }

    pub fn dir(self, rel: &str, ino: u64) -> Self {
        self.entry(rel, sig(EntryKind::Dir, 1, ino, 0, 2, 0o755), 0)
    }

    pub fn dir_mode(self, rel: &str, ino: u64, mode: u32) -> Self {
        self.entry(rel, sig(EntryKind::Dir, 1, ino, 0, 2, mode), 0)
    }

    pub fn symlink(self, rel: &str, ino: u64) -> Self {
        let mut e = FsEntry {
            path: rp(&self.inv.root, rel),
            sig: sig(EntryKind::Symlink, 1, ino, 12, 1, 0o777),
            allocated: 0,
            symlink_target: Some("../outside/secret".into()),
        };
        e.sig.mode = 0o777;
        self.push(e)
    }

    pub fn entry(self, rel: &str, s: EntrySignature, allocated: u64) -> Self {
        let e = FsEntry {
            path: rp(&self.inv.root, rel),
            sig: s,
            allocated,
            symlink_target: None,
        };
        self.push(e)
    }

    fn push(mut self, e: FsEntry) -> Self {
        self.inv.entries.push(e);
        self
    }

    /// Edits the signature of an existing entry.
    pub fn with(mut self, rel: &str, f: impl FnOnce(&mut EntrySignature)) -> Self {
        let e = self
            .inv
            .entries
            .iter_mut()
            .find(|e| e.path.rel.as_str() == rel)
            .unwrap_or_else(|| panic!("no entry {rel}"));
        f(&mut e.sig);
        self
    }

    pub fn error(mut self, rel: &str) -> Self {
        self.inv.errors.push(ScanErrorRecord {
            path: rp(&self.inv.root, rel),
            io: IoKind::PermissionDenied,
            message: "denied".into(),
        });
        self
    }

    pub fn zone(mut self, rel: &str, reason: ProtectedReason) -> Self {
        self.inv.protected.push(ProtectedZone {
            path: rp(&self.inv.root, rel),
            reason,
        });
        self
    }

    pub fn unfinished(mut self) -> Self {
        self.inv.finished = false;
        self
    }

    pub fn build(mut self) -> RootInventory {
        self.inv.entries.sort_by(|a, b| a.path.cmp(&b.path));
        self.inv
    }
}

pub fn sig(
    kind: EntryKind,
    dev: u64,
    ino: u64,
    size: u64,
    nlink: u64,
    mode: u32,
) -> EntrySignature {
    EntrySignature {
        kind,
        dev,
        ino,
        size,
        nlink,
        // 72 hours before NOW.
        mtime_ns: MtimeNs((NOW.0 - 3 * DAY_MS) * 1_000_000),
        mode,
        uid: OWNER.0,
        gid: OWNER.1,
    }
}

fn mapped(root: &RootId, rel: &str) -> MappedPath {
    MappedPath::Mapped {
        raw: format!("/data/{}/{rel}", root.as_str()),
        path: rp(root, rel),
    }
}

/// Torrent builder: files are `(name inside the torrent, rel path under the root)`.
pub struct Tor {
    pub info: TorrentInfo,
    pub files: Vec<TorrentFile>,
}

impl Tor {
    pub fn new(
        h: char,
        root: &RootId,
        state: &str,
        progress: f64,
        content: &str,
        files: &[(&str, &str)],
    ) -> Self {
        let save_rel = RelPath::new(content)
            .unwrap()
            .parent()
            .unwrap_or_else(RelPath::root);
        let info = TorrentInfo {
            hash: hash(h),
            name: format!("torrent-{h}"),
            state: TorrentState::parse(state),
            progress,
            added_on: Timestamp(NOW.0 - 10 * DAY_MS),
            completion_on: (progress >= 1.0).then_some(Timestamp(NOW.0 - 9 * DAY_MS)),
            category: String::new(),
            size: 1000,
            save_path: MappedPath::Mapped {
                raw: "/data".into(),
                path: RootedPath::new(root.clone(), save_rel),
            },
            download_path: None,
            content_path: mapped(root, content),
        };
        let files = files
            .iter()
            .enumerate()
            .map(|(i, (name, rel))| TorrentFile {
                index: u32::try_from(i).unwrap(),
                name: (*name).to_string(),
                size: 1000,
                priority: 1,
                progress,
                candidates: vec![mapped(root, rel)],
            })
            .collect();
        Self { info, files }
    }

    pub fn added_days_ago(mut self, d: i64) -> Self {
        self.info.added_on = Timestamp(NOW.0 - d * DAY_MS);
        self
    }

    /// The torrent's files lie outside every mapping rule.
    pub fn unmapped_files(mut self) -> Self {
        for f in &mut self.files {
            f.candidates = vec![MappedPath::NoRule {
                raw: "/srv/other/file".into(),
            }];
        }
        self
    }

    pub fn raw_content(mut self, m: MappedPath) -> Self {
        self.info.content_path = m;
        self
    }

    pub fn save_path(mut self, m: MappedPath) -> Self {
        self.info.save_path = m;
        self
    }
}

pub fn snapshot(major: u8, tors: Vec<Tor>) -> TorrentSnapshot {
    let mut files = BTreeMap::new();
    let mut torrents = Vec::new();
    for t in tors {
        files.insert(t.info.hash.clone(), t.files);
        torrents.push(t.info);
    }
    TorrentSnapshot {
        taken_at: NOW,
        version: format!("v{major}.0.0"),
        major,
        torrents,
        files,
    }
}

pub fn candidate(
    a: RootedPath,
    b: RootedPath,
    sig_a: EntrySignature,
    sig_b: EntrySignature,
    content: ContentProof,
) -> MatchCandidate {
    MatchCandidate {
        a,
        b,
        sig_a,
        sig_b,
        identity: IdentityProof::NameParse { score_milli: 1000 },
        content,
        reasons: vec!["same_name_and_size".into()],
        session: session(),
    }
}

/// A complete observation with a qBittorrent snapshot; tests tweak fields before classifying.
pub fn obs(invs: Vec<RootInventory>, qbit: QbitObservation) -> Observation {
    let mut roots: Vec<RootId> = invs.iter().map(|i| i.root.clone()).collect();
    roots.sort();
    Observation {
        session: session(),
        session_status: SessionStatus::Complete,
        observed_at: NOW,
        now: NOW,
        configured_roots: roots,
        inventories: invs,
        qbit,
        arr: vec![],
        matches: vec![],
        ignored: BTreeSet::new(),
    }
}

pub fn settings() -> IssueSettings {
    let mut s = IssueSettings::new(OWNER);
    s.video_extensions = [".mkv".to_string()].into_iter().collect();
    s
}

pub fn run(o: &Observation) -> Classified {
    classify(o, &settings()).expect("classifies")
}

pub fn of(c: &Classified, cat: IssueCategory) -> Vec<&Issue> {
    c.issues.iter().filter(|i| i.category == cat).collect()
}

pub fn one<'a>(c: &'a Classified, cat: IssueCategory, subject: &str) -> &'a Issue {
    c.issues
        .iter()
        .find(|i| i.category == cat && i.subject.as_str() == subject)
        .unwrap_or_else(|| {
            panic!(
                "no {cat:?} issue {subject}; have {:?}",
                c.issues
                    .iter()
                    .map(|i| (i.category.as_str(), i.subject.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

pub fn has(i: &Issue, code: &BlockerCode) -> bool {
    i.blockers.iter().any(|b| &b.code == code)
}

pub fn subjects(c: &Classified, cat: IssueCategory) -> Vec<String> {
    of(c, cat)
        .iter()
        .map(|i| i.subject.as_str().to_string())
        .collect()
}

pub fn arr_ok(
    service: ArrService,
    library: Vec<ArrLibraryEntry>,
    queue: Vec<ArrQueueItem>,
) -> ArrObservation {
    ArrObservation {
        service,
        state: ServiceState::Ok {
            version: "5".into(),
        },
        captured_at: NOW,
        library,
        queue,
        history: vec![],
    }
}

pub fn library_entry(service: ArrService, media_id: i64, path: RootedPath) -> ArrLibraryEntry {
    let m = ArrMediaRef {
        service,
        media_id,
        episode_ids: vec![],
    };
    ArrLibraryEntry {
        media: m.clone(),
        identity: MediaIdentity {
            kind: MediaKind::Movie,
            arr: Some(m),
            external_ids: BTreeMap::new(),
            title: "T".into(),
            year: Some(2020),
            season: None,
            episode: None,
            absolute: None,
        },
        file: ArrFileRef {
            file_id: 1,
            path: MappedPath::Mapped {
                raw: "/m".into(),
                path,
            },
            size: 1,
            quality: "q".into(),
            custom_formats: vec![],
            cf_score: 0,
            cutoff_not_met: false,
            languages: vec![],
        },
        monitored: true,
    }
}

pub fn queue_item(
    id: i64,
    hash_c: char,
    media_id: i64,
    service: ArrService,
    status: &str,
    state: &str,
) -> ArrQueueItem {
    ArrQueueItem {
        id,
        download_id: Some(hash(hash_c)),
        media: Some(ArrMediaRef {
            service,
            media_id,
            episode_ids: vec![],
        }),
        status: status.into(),
        tracked_state: state.into(),
        messages: vec!["secret message".into()],
    }
}
