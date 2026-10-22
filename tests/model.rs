//! Property test: the engine behaves like a `BTreeMap` across random
//! puts, deletes, point and range reads, flushes, clean reopens and
//! power cuts (with every write synced, a power cut loses nothing).

mod common;
use common::opts;
use cortex9_lsm_engine::rng::Rng;
use cortex9_lsm_engine::{Bugs, CrashMode, Db, Options, SimVfs};
use proptest::prelude::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
enum Step {
    Put(u16, u8, u16),
    Del(u16),
    Get(u16),
    Range(u16, u16),
    Flush,
    Reopen,
    PowerCut(u64),
}

fn key(k: u16) -> Vec<u8> {
    format!("k{k:04}").into_bytes()
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        30 => (0..300u16, any::<u8>(), 0..300u16).prop_map(|(k, b, n)| Step::Put(k, b, n)),
        10 => (0..300u16).prop_map(Step::Del),
        10 => (0..300u16).prop_map(Step::Get),
        3 => (0..300u16, 0..300u16).prop_map(|(a, b)| Step::Range(a.min(b), a.max(b))),
        1 => Just(Step::Flush),
        1 => Just(Step::Reopen),
        1 => any::<u64>().prop_map(Step::PowerCut),
    ]
}

fn run(steps: &[Step], o: Options) -> Result<(), TestCaseError> {
    let mut db = Db::open(SimVfs::new(), o).unwrap();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for s in steps {
        match s {
            Step::Put(k, b, n) => {
                let v = vec![*b; *n as usize];
                db.put(&key(*k), &v).unwrap();
                m.insert(key(*k), v);
            }
            Step::Del(k) => {
                db.delete(&key(*k)).unwrap();
                m.remove(&key(*k));
            }
            Step::Get(k) => {
                prop_assert_eq!(db.get(&key(*k)).unwrap(), m.get(&key(*k)).cloned());
            }
            Step::Range(a, b) => {
                let got = db.range(&key(*a), Some(&key(*b))).unwrap();
                let want: Vec<_> = m
                    .range(key(*a)..key(*b))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                prop_assert_eq!(got, want);
            }
            Step::Flush => db.flush().unwrap(),
            Step::Reopen => db = Db::open(db.into_vfs(), o).unwrap(),
            Step::PowerCut(seed) => {
                let vfs = db
                    .into_vfs()
                    .after_crash(CrashMode::Torn, &mut Rng::new(*seed));
                db = Db::open(vfs, o).unwrap();
            }
        }
    }
    db.check().unwrap();
    let all: BTreeMap<_, _> = db.range(b"", None).unwrap().into_iter().collect();
    prop_assert_eq!(all, m);
    Ok(())
}

fn cases() -> u32 {
    if cfg!(debug_assertions) {
        64
    } else {
        512
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn matches_btreemap(steps in proptest::collection::vec(step(), 1..400)) {
        run(&steps, opts())?;
    }

    #[test]
    fn matches_btreemap_without_bloom_filters(steps in proptest::collection::vec(step(), 1..200)) {
        run(&steps, Options { bloom_bits_per_key: 0, ..opts() })?;
    }
}

#[test]
fn tombstone_bug_is_caught_without_crashes() {
    // Negative control for the model test itself.
    let o = Options {
        bugs: Bugs {
            drop_tombstones_early: true,
            ..Bugs::default()
        },
        ..opts()
    };
    let mut found = false;
    for seed in 0..20u64 {
        let mut r = Rng::new(seed);
        let steps: Vec<Step> = (0..400)
            .map(|_| match r.below(5) {
                0 => Step::Del(r.below(80) as u16),
                1 => Step::Get(r.below(80) as u16),
                _ => Step::Put(r.below(80) as u16, r.below(256) as u8, r.below(120) as u16),
            })
            .collect();
        let r = std::panic::catch_unwind(|| run(&steps, o));
        if !matches!(r, Ok(Ok(()))) {
            found = true;
            break;
        }
    }
    assert!(found);
}

#[test]
fn bloom_filters_skip_most_absent_probes() {
    let o = Options {
        memtable_bytes: 64 << 10,
        l0_trigger: 100,
        ..Options::default()
    };
    let mut db = Db::open(SimVfs::new(), o).unwrap();
    for i in 0..20_000u32 {
        db.put(format!("present-{i:06}").as_bytes(), &[1; 32])
            .unwrap();
    }
    db.flush().unwrap();
    let tables: usize = db.level_counts().iter().sum();
    assert!(tables >= 10, "{tables} tables");
    for i in 0..5_000u32 {
        // Inside every table's key range, so only the filter can skip it.
        assert_eq!(db.get(format!("present-{i:06}x").as_bytes()).unwrap(), None);
    }
    let s = &db.stats;
    let probes = s.bloom_skips + s.wasted_reads;
    println!(
        "{tables} tables, {probes} probes of absent keys: {} skipped by filter, {} read a block",
        s.bloom_skips, s.wasted_reads
    );
    assert!(
        s.wasted_reads * 50 < probes,
        "more than 2% of probes read a block"
    );
}
