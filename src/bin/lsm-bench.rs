//! Rough single-thread numbers on real files. Not a rigorous benchmark:
//! one run, OS page cache warm, no compression.
//!
//! Usage: lsm-bench <dir> [keys]

use cortex9_lsm_engine::rng::Rng;
use cortex9_lsm_engine::{Db, FsVfs, Options};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("usage: lsm-bench <dir> [keys]"));
    let n: u64 = args.next().map_or(200_000, |s| s.parse().expect("keys"));
    let _ = std::fs::remove_dir_all(&dir);
    let o = Options {
        sync_writes: false,
        ..Options::default()
    };
    let mut db = Db::open(FsVfs::open(&dir).expect("dir"), o).expect("open");
    let mut r = Rng::new(7);
    let val = [0xABu8; 100];
    let t = Instant::now();
    for _ in 0..n {
        let k = r.below(n);
        db.put(format!("user{k:012}").as_bytes(), &val)
            .expect("put");
    }
    db.flush().expect("flush");
    let dt = t.elapsed().as_secs_f64();
    let user = n * (16 + 100);
    let s = db.stats.clone();
    let written = s.wal_bytes + s.flush_bytes + s.compaction_bytes_out;
    println!("puts (no fsync): {n} in {dt:.2} s = {:.0}/s", n as f64 / dt);
    println!(
        "write amplification (wal + flush + compaction bytes / user bytes): {:.2}",
        written as f64 / user as f64
    );
    println!(
        "flushes {}, compactions {}, tables per level {:?}",
        s.flushes,
        s.compactions,
        db.level_counts()
    );
    let t = Instant::now();
    let mut hits = 0;
    for _ in 0..n / 2 {
        let k = r.below(n);
        hits += db
            .get(format!("user{k:012}").as_bytes())
            .expect("get")
            .is_some() as u64;
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "random gets: {} in {dt:.2} s = {:.0}/s ({hits} hits)",
        n / 2,
        (n / 2) as f64 / dt
    );
    let before = db.stats.clone();
    let t = Instant::now();
    for i in 0..n / 2 {
        db.get(format!("user{i:012}-absent").as_bytes())
            .expect("get");
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "absent gets: {} in {dt:.2} s = {:.0}/s ({} probes skipped by bloom, {} read a block)",
        n / 2,
        (n / 2) as f64 / dt,
        db.stats.bloom_skips - before.bloom_skips,
        db.stats.wasted_reads - before.wasted_reads
    );
    let t = Instant::now();
    let all = db.range(b"", None).expect("range").len();
    println!(
        "full scan: {all} keys in {:.2} s",
        t.elapsed().as_secs_f64()
    );
    drop(db);
    let o = Options::default();
    let sync_dir = dir.join("synced");
    let mut db = Db::open(FsVfs::open(&sync_dir).expect("dir"), o).expect("open");
    let m = 2000u64;
    let t = Instant::now();
    for i in 0..m {
        db.put(format!("user{i:012}").as_bytes(), &val)
            .expect("put");
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "puts (fsync each): {m} in {dt:.2} s = {:.0}/s",
        m as f64 / dt
    );
}
