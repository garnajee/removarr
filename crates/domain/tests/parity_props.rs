//! Property tests over the public domain API (path confinement, canonical JSON, gain accounting).

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use removarr_domain::canonical::{canonical_json_string, sha256_hex};
use removarr_domain::fs::PhysicalId;
use removarr_domain::gain::{
    GainKind, GainRecord, aggregate_unique, estimate_reclaimable, gain_id,
};
use removarr_domain::ids::{ActionRef, PlanId, RawUuid};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::policy::is_sample;
use removarr_domain::time::{MtimeNs, Timestamp};

fn component() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _()\\[\\]-][A-Za-z0-9 _.()\\[\\]-]{0,11}"
        .prop_filter("not dot", |s| s != "." && s != "..")
}

fn rel_string() -> impl Strategy<Value = String> {
    prop::collection::vec(component(), 0..6).prop_map(|v| v.join("/"))
}

proptest! {
    #[test]
    fn rel_path_accepts_clean_paths_and_round_trips(s in rel_string()) {
        let p = RelPath::new(s.clone()).expect("clean path accepted");
        prop_assert_eq!(p.as_str(), s.as_str());
        let json = serde_json::to_string(&p).unwrap();
        let back: RelPath = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, p);
    }

    #[test]
    fn rel_path_never_accepts_escapes(prefix in rel_string(), suffix in rel_string()) {
        for bad in ["..", "."] {
            let joined = [prefix.as_str(), bad, suffix.as_str()]
                .iter()
                .filter(|x| !x.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("/");
            prop_assert!(RelPath::new(joined.clone()).is_err(), "accepted {}", joined);
        }
        let bad = format!("/{prefix}x", prefix = prefix);
        prop_assert!(RelPath::new(bad).is_err());
        let bad = format!("{prefix}x/", prefix = prefix);
        prop_assert!(RelPath::new(bad).is_err());
        let bad = format!("{prefix}x//y", prefix = prefix);
        prop_assert!(RelPath::new(bad).is_err());
        let bad = format!("{prefix}x\0y", prefix = prefix);
        prop_assert!(RelPath::new(bad).is_err());
        let json = serde_json::to_string(&format!("{prefix}x/../y")).unwrap();
        prop_assert!(serde_json::from_str::<RelPath>(&json).is_err());
    }

    #[test]
    fn join_then_parent_is_identity(s in rel_string(), c in component()) {
        let base = RelPath::new(s).unwrap();
        let child = base.join(&c).unwrap();
        prop_assert_eq!(child.parent().unwrap(), base.clone());
        prop_assert_eq!(child.file_name().unwrap(), c.as_str());
        prop_assert!(child.starts_with(&base));
        prop_assert!(!base.starts_with(&child));
        prop_assert!(child.starts_with(&RelPath::root()));
    }

    #[test]
    fn starts_with_is_component_wise(a in component(), b in component()) {
        let pa = RelPath::new(a.clone()).unwrap();
        let pb = RelPath::new(format!("{a}{b}")).unwrap();
        // "ab" is never below "a" merely because of a string prefix.
        prop_assert!(!pb.starts_with(&pa));
    }

    #[test]
    fn media_root_ids_are_valid_and_stable(path in "[ -~]{1,40}") {
        let id = RootId::media(&path);
        prop_assert_eq!(&id, &RootId::media(&path));
        prop_assert!(RootId::new(id.as_str()).is_ok());
        let rp = RootedPath::new(id, RelPath::root());
        let json = serde_json::to_string(&rp).unwrap();
        prop_assert_eq!(serde_json::from_str::<RootedPath>(&json).unwrap(), rp);
    }

    #[test]
    fn sample_detection_ignores_case(s in rel_string()) {
        let p = RelPath::new(s.clone()).unwrap();
        let upper = RelPath::new(s.to_uppercase()).unwrap();
        prop_assert_eq!(is_sample(&p), is_sample(&upper));
    }

    #[test]
    fn canonical_json_ignores_insertion_order(
        entries in prop::collection::vec(("[a-z]{1,6}", any::<i32>()), 0..12)
    ) {
        let forward: BTreeMap<_, _> = entries.iter().cloned().collect();
        let mut reversed = serde_json::Map::new();
        for (k, v) in forward.iter().rev() {
            reversed.insert(k.clone(), serde_json::json!(v));
        }
        let a = canonical_json_string(&forward).unwrap();
        let b = canonical_json_string(&serde_json::Value::Object(reversed)).unwrap();
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(sha256_hex(a.as_bytes()).len(), 64);
    }

    #[test]
    fn reclaimable_counts_each_inode_once_and_only_when_fully_selected(
        inodes in prop::collection::btree_map(1u64..50, (1u32..4, 1u64..4, 0u64..1_000_000), 0..20),
        held in prop::collection::btree_set(1u64..50, 0..10),
    ) {
        let selected: BTreeMap<PhysicalId, (u32, u64, u64)> = inodes
            .iter()
            .map(|(i, v)| (PhysicalId { dev: 1, ino: *i }, *v))
            .collect();
        let held: BTreeSet<PhysicalId> =
            held.iter().map(|i| PhysicalId { dev: 1, ino: *i }).collect();
        let got = estimate_reclaimable(&selected, &held);
        let expected: u64 = selected
            .iter()
            .filter(|(p, (l, n, _))| !held.contains(*p) && u64::from(*l) >= *n)
            .map(|(_, (_, _, a))| *a)
            .sum();
        prop_assert_eq!(got, expected);
        let total: u64 = selected.values().map(|v| v.2).sum();
        prop_assert!(got <= total);
        let all: BTreeSet<PhysicalId> = selected.keys().copied().collect();
        prop_assert_eq!(estimate_reclaimable(&selected, &all), 0);
    }

    #[test]
    fn aggregate_unique_is_idempotent_under_duplication(
        recs in prop::collection::vec((1u64..20, 0u64..1_000_000, any::<bool>()), 0..15)
    ) {
        let plan = PlanId::new(RawUuid::from_u128(7));
        let records: Vec<GainRecord> = recs
            .iter()
            .enumerate()
            .map(|(i, (ino, bytes, added))| {
                let phys = PhysicalId { dev: 1, ino: *ino };
                GainRecord {
                    gain_id: gain_id(phys, *bytes, MtimeNs(0)),
                    action: ActionRef { plan, index: u32::try_from(i).unwrap() },
                    kind: if *added { GainKind::AddedCost } else { GainKind::AttributableDeletion },
                    phys,
                    bytes: *bytes,
                    recorded_at: Timestamp(0),
                }
            })
            .collect();
        let once = aggregate_unique(&records);
        let mut doubled = records.clone();
        doubled.extend(records.iter().cloned());
        let twice = aggregate_unique(&doubled);
        prop_assert_eq!(once.attributable, twice.attributable);
        prop_assert_eq!(once.added, twice.added);
        prop_assert_eq!(once.net, twice.net);
        prop_assert_eq!(twice.duplicates, once.duplicates + u64::try_from(records.len()).unwrap());
    }

    #[test]
    fn gain_id_is_a_pure_function_of_its_inputs(
        dev in any::<u64>(), ino in any::<u64>(), size in any::<u64>(), m in any::<i64>()
    ) {
        let p = PhysicalId { dev, ino };
        let a = gain_id(p, size, MtimeNs(m));
        prop_assert_eq!(&a, &gain_id(p, size, MtimeNs(m)));
        prop_assert_eq!(a.0.len(), 32);
        let other = gain_id(PhysicalId { dev, ino: ino.wrapping_add(1) }, size, MtimeNs(m));
        prop_assert_ne!(&a, &other);
    }

    #[test]
    fn age_days_is_monotonic_in_now(a in any::<i64>(), b in any::<i64>(), d in 0i64..1_000_000_000) {
        let t = Timestamp(a);
        let n1 = Timestamp(b);
        let n2 = n1.plus_millis(d);
        prop_assert!(t.age_days(n2) >= t.age_days(n1));
    }
}
