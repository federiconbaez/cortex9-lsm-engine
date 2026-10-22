//! Crash at every write point, under lose-all, keep-all and torn
//! (random subset + torn sectors) crash modes, and check recovery.

mod common;
use common::*;
use cortex9_lsm_engine::{Bugs, Options};

fn seeds() -> u64 {
    if cfg!(debug_assertions) {
        4
    } else {
        30
    }
}

#[test]
fn recovers_at_every_crash_point() {
    let (mut points, mut recs, mut fl, mut co, mut rot) = (0, 0, 0, 0, 0);
    for seed in 0..seeds() {
        let ops = workload(seed, 150);
        let r = crash_everywhere(seed, &ops, opts()).unwrap_or_else(|e| panic!("{e}"));
        points += r.points;
        recs += r.recoveries;
        fl += r.flushes;
        co += r.compactions;
        rot += r.rotations;
    }
    println!(
        "{} workloads: {points} crash points, {recs} recoveries checked; \
         per uninterrupted run totals: {fl} flushes, {co} compactions, {rot} manifest rotations",
        seeds()
    );
    assert!(
        fl > 0 && co > 0 && rot > 0,
        "workload must exercise flush, compaction and rotation"
    );
}

/// Each injected bug must be caught by the same harness: proof that the
/// crash test is not vacuous.
fn caught(bugs: Bugs) -> bool {
    let o = Options { bugs, ..opts() };
    (0..6).any(|seed| crash_everywhere(seed, &workload(seed, 150), o).is_err())
}

#[test]
fn negative_control_skip_wal_sync() {
    assert!(caught(Bugs {
        skip_wal_sync: true,
        ..Bugs::default()
    }));
}

#[test]
fn negative_control_skip_table_sync() {
    assert!(caught(Bugs {
        skip_table_sync: true,
        ..Bugs::default()
    }));
}

#[test]
fn negative_control_skip_dir_sync() {
    assert!(caught(Bugs {
        skip_dir_sync: true,
        ..Bugs::default()
    }));
}

#[test]
fn negative_control_early_delete() {
    assert!(caught(Bugs {
        early_delete: true,
        ..Bugs::default()
    }));
}

#[test]
fn negative_control_drop_tombstones_early() {
    assert!(caught(Bugs {
        drop_tombstones_early: true,
        ..Bugs::default()
    }));
}
