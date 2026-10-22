//! Real files: data survives reopen through flushes, compactions and WAL
//! replay, and recovery leaves only live files behind.

use cortex9_lsm_engine::{Db, FsVfs, Options};

#[test]
fn survives_reopen_on_real_files() {
    let dir = tempfile::tempdir().unwrap();
    let o = Options {
        memtable_bytes: 32 << 10,
        level1_bytes: 128 << 10,
        target_file_bytes: 32 << 10,
        sync_writes: false,
        ..Options::default()
    };
    {
        let mut db = Db::open(FsVfs::open(dir.path()).unwrap(), o).unwrap();
        for i in 0..5000u32 {
            db.put(format!("k{i:06}").as_bytes(), &[i as u8; 64])
                .unwrap();
        }
        for i in (0..5000u32).step_by(3) {
            db.delete(format!("k{i:06}").as_bytes()).unwrap();
        }
        assert!(db.stats.flushes > 0 && db.stats.compactions > 0);
        // Dropped without a flush: the WAL tail is replayed on open.
    }
    let mut db = Db::open(FsVfs::open(dir.path()).unwrap(), o).unwrap();
    assert!(db.stats.replayed > 0);
    assert_eq!(db.check().unwrap(), 5000 - 1667);
    assert_eq!(db.get(b"k000001").unwrap(), Some(vec![1u8; 64]));
    assert_eq!(db.get(b"k000003").unwrap(), None);
    let files = std::fs::read_dir(dir.path()).unwrap().count();
    let tables: usize = db.level_counts().iter().sum();
    assert_eq!(
        files,
        tables + 2,
        "only live tables, one WAL and one manifest"
    );
}
