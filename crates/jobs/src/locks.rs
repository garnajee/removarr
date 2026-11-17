//! Lock scopes (architecture 8.3, ADR-0007). One keyed `RwLock` table covers root path scopes,
//! media identities, torrent hashes and action types. Keys are acquired in sorted order, so
//! two jobs asking for overlapping sets can never deadlock. Observe jobs take shared locks,
//! execution takes exclusive locks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use removarr_domain::arr::ArrMediaRef;
use removarr_domain::path::RootId;
use removarr_domain::plan::{ActionKind, ActionPlan};
use removarr_domain::torrent::TorrentHash;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;

/// The set of keys a job needs. A key in both sets is taken exclusively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub shared: BTreeSet<String>,
    pub exclusive: BTreeSet<String>,
}

impl Scope {
    #[must_use]
    pub fn root_key(root: &RootId) -> String {
        format!("root:{}", root.as_str())
    }

    #[must_use]
    pub fn media_key(media: &ArrMediaRef) -> String {
        format!("media:{:?}:{}", media.service, media.media_id).to_lowercase()
    }

    #[must_use]
    pub fn hash_key(hash: &TorrentHash) -> String {
        format!("hash:{}", hash.as_str())
    }

    #[must_use]
    pub fn action_key(kind: ActionKind) -> String {
        format!("action:{kind:?}").to_lowercase()
    }

    /// Shared read access to roots (scan, evaluate, verify, simulation).
    #[must_use]
    pub fn shared_roots<'a>(roots: impl IntoIterator<Item = &'a RootId>) -> Self {
        Self {
            shared: roots.into_iter().map(Self::root_key).collect(),
            exclusive: BTreeSet::new(),
        }
    }

    /// What executing `plan` needs. A live run is exclusive on every root in
    /// `plan.lock_roots`, on every ARR media identity the plan names (one importer per media,
    /// INV-16), on every affected torrent hash and on every action type it runs. A simulation
    /// only reads, so it takes the roots shared.
    #[must_use]
    pub fn for_plan(plan: &ActionPlan, simulate: bool) -> Self {
        if simulate {
            return Self::shared_roots(plan.lock_roots.iter());
        }
        let mut s = Self::default();
        s.exclusive
            .extend(plan.lock_roots.iter().map(Self::root_key));
        s.exclusive
            .extend(plan.arr_snapshots.iter().map(|r| Self::media_key(&r.media)));
        for a in &plan.actions {
            s.exclusive.insert(Self::action_key(a.kind));
            s.exclusive
                .extend(a.torrent_hashes.iter().map(Self::hash_key));
        }
        s
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.exclusive.is_empty()
    }

    /// True when two jobs holding these scopes cannot run at the same time.
    #[must_use]
    pub fn conflicts_with(&self, other: &Scope) -> bool {
        self.exclusive
            .iter()
            .any(|k| other.exclusive.contains(k) || other.shared.contains(k))
            || other.exclusive.iter().any(|k| self.shared.contains(k))
    }

    fn keys(&self) -> BTreeMap<&str, bool> {
        let mut m: BTreeMap<&str, bool> = self.shared.iter().map(|k| (k.as_str(), false)).collect();
        for k in &self.exclusive {
            m.insert(k.as_str(), true);
        }
        m
    }
}

/// The caller was cancelled while waiting for a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCancelled;

#[derive(Debug)]
enum Held {
    Read(#[allow(dead_code)] OwnedRwLockReadGuard<()>),
    Write(#[allow(dead_code)] OwnedRwLockWriteGuard<()>),
}

/// Releases every lock on drop.
#[derive(Debug, Default)]
pub struct LockGuard {
    held: Vec<Held>,
}

impl LockGuard {
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Locks {
    table: Mutex<BTreeMap<String, Arc<RwLock<()>>>>,
}

impl Locks {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_for(&self, key: &str) -> Arc<RwLock<()>> {
        let mut t = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(t.entry(key.to_string()).or_default())
    }

    /// Waits for every key of `scope`, in sorted order. Cancellation while waiting releases
    /// what was acquired so far and returns `LockCancelled`.
    pub async fn acquire(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<LockGuard, LockCancelled> {
        let mut guard = LockGuard::default();
        for (key, exclusive) in scope.keys() {
            let lock = self.lock_for(key);
            let held = if exclusive {
                tokio::select! {
                    g = lock.write_owned() => Held::Write(g),
                    () = cancel.cancelled() => return Err(LockCancelled),
                }
            } else {
                tokio::select! {
                    g = lock.read_owned() => Held::Read(g),
                    () = cancel.cancelled() => return Err(LockCancelled),
                }
            };
            guard.held.push(held);
        }
        Ok(guard)
    }

    /// Takes the locks only if all are free right now.
    #[must_use]
    pub fn try_acquire(&self, scope: &Scope) -> Option<LockGuard> {
        let mut guard = LockGuard::default();
        for (key, exclusive) in scope.keys() {
            let lock = self.lock_for(key);
            let held = if exclusive {
                Held::Write(lock.try_write_owned().ok()?)
            } else {
                Held::Read(lock.try_read_owned().ok()?)
            };
            guard.held.push(held);
        }
        Some(guard)
    }
}
