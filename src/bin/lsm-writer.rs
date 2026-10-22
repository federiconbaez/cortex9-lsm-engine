//! Writes `key-000000`, `key-000001`, ... to a database directory until
//! killed, printing each acknowledged index. Used by the SIGKILL test.

use cortex9_lsm_engine::{Db, FsVfs, Options};
use std::io::Write;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: lsm-writer <dir>");
    let o = Options {
        memtable_bytes: 64 << 10,
        level1_bytes: 256 << 10,
        target_file_bytes: 64 << 10,
        ..Options::default()
    };
    let vfs = FsVfs::open(std::path::Path::new(&dir)).expect("open dir");
    let mut db = Db::open(vfs, o).expect("open db");
    let mut out = std::io::stdout().lock();
    let mut i = 0u64;
    loop {
        db.put(format!("key-{i:06}").as_bytes(), &[(i % 251) as u8; 200])
            .expect("put");
        writeln!(out, "{i}").expect("stdout");
        out.flush().expect("flush");
        i += 1;
    }
}
