//! Kill a real writer process with SIGKILL mid-stream and recover.
//! (A process crash keeps the OS page cache; power loss is covered by the
//! simulated file-system tests.)
#![cfg(unix)]

use cortex9_lsm_engine::{Db, FsVfs, Options};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn recovers_after_sigkill() {
    for round in 0..3u64 {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_lsm-writer"))
            .arg(dir.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let target = 500 + round * 700;
        let mut acked = 0u64;
        for l in lines.by_ref() {
            acked = l.unwrap().parse().unwrap();
            if acked >= target {
                break;
            }
        }
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();
        let mut db = Db::open(FsVfs::open(dir.path()).unwrap(), Options::default()).unwrap();
        let n = db.check().unwrap() as u64;
        // Every acknowledged key is present, and the keys form a prefix.
        assert!(n > acked, "round {round}: {n} keys, acked up to {acked}");
        for i in [0, acked / 2, acked, n - 1] {
            assert_eq!(
                db.get(format!("key-{i:06}").as_bytes()).unwrap(),
                Some(vec![(i % 251) as u8; 200])
            );
        }
        assert_eq!(db.get(format!("key-{n:06}").as_bytes()).unwrap(), None);
    }
}
