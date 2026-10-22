#![allow(dead_code)]

use cortex9_lsm_engine::rng::Rng;
use cortex9_lsm_engine::{CrashMode, Db, Error, Options, SimVfs};
use std::collections::BTreeMap;

pub type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone)]
pub enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

pub fn apply_model(m: &mut Model, op: &Op) {
    match op {
        Op::Put(k, v) => {
            m.insert(k.clone(), v.clone());
        }
        Op::Del(k) => {
            m.remove(k);
        }
    }
}

pub fn apply_db(db: &mut Db<SimVfs>, op: &Op) -> Result<(), Error> {
    match op {
        Op::Put(k, v) => db.put(k, v),
        Op::Del(k) => db.delete(k),
    }
}

/// Random workload over a small key space so that keys are overwritten
/// and deleted across flushes and levels.
pub fn workload(seed: u64, n: usize) -> Vec<Op> {
    let mut r = Rng::new(seed);
    (0..n)
        .map(|_| {
            let k = format!("key-{:03}", r.below(60)).into_bytes();
            if r.below(4) == 0 {
                Op::Del(k)
            } else {
                let len = 1 + r.below(200) as usize;
                let b = r.below(256) as u8;
                Op::Put(k, vec![b; len])
            }
        })
        .collect()
}

/// Tiny limits so a few hundred writes go through many flushes,
/// compactions into three levels, and manifest rotations.
pub fn opts() -> Options {
    Options {
        memtable_bytes: 1500,
        block_size: 256,
        l0_trigger: 3,
        level1_bytes: 3000,
        level_multiplier: 2,
        target_file_bytes: 1500,
        levels: 4,
        manifest_max_bytes: 600,
        ..Options::default()
    }
}

pub fn contents(db: &mut Db<SimVfs>) -> Model {
    db.range(b"", None).expect("range").into_iter().collect()
}

pub struct Crashed {
    pub vfs: SimVfs,
    pub acked: Model,
    pub inflight: Option<Op>,
    pub done: usize,
}

pub fn run_until_crash(ops: &[Op], crash_after: Option<u64>, o: Options) -> Crashed {
    let mut vfs = SimVfs::new();
    if let Some(n) = crash_after {
        vfs.crash_after(n);
    }
    let mut acked = Model::new();
    let mut db = match Db::open(vfs.clone(), o) {
        Ok(db) => db,
        Err(_) => {
            // Crashed while creating the database: nothing was acknowledged.
            // Re-run the open on our copy to get the same crashed state.
            let _ = Db::open(&mut vfs, o).err();
            return Crashed {
                vfs,
                acked,
                inflight: None,
                done: 0,
            };
        }
    };
    for (i, op) in ops.iter().enumerate() {
        match apply_db(&mut db, op) {
            Ok(()) => apply_model(&mut acked, op),
            Err(_) => {
                return Crashed {
                    vfs: db.into_vfs(),
                    acked,
                    inflight: Some(op.clone()),
                    done: i,
                }
            }
        }
    }
    Crashed {
        vfs: db.into_vfs(),
        acked,
        inflight: None,
        done: ops.len(),
    }
}

pub struct Report {
    pub points: u64,
    pub recoveries: u64,
    pub flushes: u64,
    pub compactions: u64,
    pub rotations: u64,
}

/// Crash at every mutating file-system call of `ops`, under each crash
/// mode, and check that recovery yields the acknowledged state (plus,
/// optionally, the one in-flight write), that the recovered database
/// passes `check`, keeps accepting writes, and survives another crash.
pub fn crash_everywhere(seed: u64, ops: &[Op], o: Options) -> Result<Report, String> {
    let full = run_until_crash(ops, None, o);
    let total = full.vfs.ops();
    let mut db = Db::open(full.vfs.clone(), o).map_err(|e| format!("clean reopen: {e}"))?;
    db.check().map_err(|e| format!("clean reopen: {e}"))?;
    if contents(&mut db) != full.acked {
        return Err(format!("seed {seed}: clean reopen lost data"));
    }
    let mut rng = Rng::new(seed ^ 0xC0FFEE);
    let mut rep = Report {
        points: 0,
        recoveries: 0,
        flushes: 0,
        compactions: 0,
        rotations: 0,
    };
    {
        // Stats of the uninterrupted run.
        let mut d = Db::open(SimVfs::new(), o).unwrap();
        for op in ops {
            apply_db(&mut d, op).unwrap();
        }
        rep.flushes = d.stats.flushes;
        rep.compactions = d.stats.compactions;
        rep.rotations = d.stats.manifest_rotations;
    }
    for point in 0..=total {
        let c = run_until_crash(ops, Some(point), o);
        rep.points += 1;
        for mode in [
            CrashMode::LoseAll,
            CrashMode::KeepAll,
            CrashMode::Torn,
            CrashMode::Torn,
        ] {
            let ctx = format!("seed {seed} point {point}/{total} {mode:?}");
            let vfs = c.vfs.after_crash(mode, &mut rng);
            let mut db = Db::open(vfs, o).map_err(|e| format!("{ctx}: open failed: {e}"))?;
            db.check().map_err(|e| format!("{ctx}: check: {e}"))?;
            let got = contents(&mut db);
            let mut with = c.acked.clone();
            if let Some(op) = &c.inflight {
                apply_model(&mut with, op);
            }
            let mut model = if got == c.acked {
                c.acked.clone()
            } else if got == with {
                with
            } else {
                let lost: Vec<_> = c
                    .acked
                    .iter()
                    .filter(|(k, v)| got.get(*k) != Some(v))
                    .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                    .take(3)
                    .collect();
                let extra: Vec<_> = got
                    .keys()
                    .filter(|k| !with.contains_key(*k))
                    .map(|k| String::from_utf8_lossy(k).into_owned())
                    .take(3)
                    .collect();
                return Err(format!(
                    "{ctx}: recovered {} keys, acked {} (wrong/lost {lost:?}, unexpected {extra:?})",
                    got.len(),
                    c.acked.len()
                ));
            };
            for op in ops.iter().skip(c.done + 1).take(8) {
                apply_db(&mut db, op).map_err(|e| format!("{ctx}: write after recovery: {e}"))?;
                apply_model(&mut model, op);
            }
            let vfs = db.into_vfs().after_crash(CrashMode::LoseAll, &mut rng);
            let mut db = Db::open(vfs, o).map_err(|e| format!("{ctx}: second open: {e}"))?;
            if contents(&mut db) != model {
                return Err(format!("{ctx}: state after continuing differs"));
            }
            rep.recoveries += 1;
        }
    }
    Ok(rep)
}
