//! The engine: WAL + memtable, flushes to level 0, leveled compaction,
//! manifest, and recovery.
//!
//! Durability protocol (each step is a crash point the tests exercise):
//!
//! - **Write**: append a framed record to the active WAL, `fsync` it, then
//!   update the memtable. The call returns only after the `fsync`.
//! - **Flush**: write the memtable as a new table, `fsync` it, create the
//!   next WAL, `fsync` the directory, append an edit `{add L0 table,
//!   log_number = new WAL}` to the manifest and `fsync` it. Only then is
//!   the old WAL removed.
//! - **Compaction**: write and `fsync` the outputs, `fsync` the directory,
//!   commit `{delete inputs, add outputs}` to the manifest, then remove the
//!   inputs.
//! - **Manifest rotation / open**: write a full snapshot to a new
//!   `MANIFEST-N`, `fsync` it and the directory, then remove the old one.
//!   Recovery picks the highest-numbered manifest whose first record is a
//!   valid snapshot.
//! - **Recovery**: load the manifest, replay every WAL numbered at or above
//!   `log_number` up to its first damaged record, flush what was replayed,
//!   start a fresh WAL and manifest, and delete every file the new
//!   manifest does not reference.

use crate::bloom::hash;
use crate::codec::{frame, unframe, Dec, Enc};
use crate::error::{corrupt, Error};
use crate::sst::{self, Entry, Lookup, Table};
use crate::version::{Edit, FileMeta, Version};
use crate::vfs::Vfs;
use std::collections::{BTreeMap, HashMap};

/// Largest key accepted.
pub const MAX_KEY: usize = 4096;
/// Largest value accepted.
pub const MAX_VALUE: usize = 1 << 20;

/// Deliberate protocol bugs, used as negative controls by the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bugs {
    /// Acknowledge writes without `fsync` on the WAL.
    pub skip_wal_sync: bool,
    /// Commit tables to the manifest without `fsync`ing them.
    pub skip_table_sync: bool,
    /// Never `fsync` the directory.
    pub skip_dir_sync: bool,
    /// Remove compaction inputs before the manifest commit.
    pub early_delete: bool,
    /// Drop tombstones in every compaction, even when older values for the
    /// key may live in deeper levels.
    pub drop_tombstones_early: bool,
}

/// Tuning knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Flush the memtable when it holds about this many bytes.
    pub memtable_bytes: usize,
    /// Target uncompressed data block size.
    pub block_size: usize,
    /// Bloom filter bits per key (0 disables filters).
    pub bloom_bits_per_key: usize,
    /// Compact level 0 into level 1 at this many level-0 tables.
    pub l0_trigger: usize,
    /// Size limit of level 1; each deeper level is `level_multiplier` larger.
    pub level1_bytes: u64,
    /// Growth factor between levels.
    pub level_multiplier: u64,
    /// Split compaction output at about this size.
    pub target_file_bytes: usize,
    /// Number of levels (the last one has no size limit).
    pub levels: usize,
    /// Rotate the manifest past this size.
    pub manifest_max_bytes: u64,
    /// `fsync` the WAL on every write. Off trades durability of the last
    /// writes for throughput; recovery stays consistent either way.
    pub sync_writes: bool,
    /// Injected bugs (all off by default).
    pub bugs: Bugs,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_bytes: 1 << 20,
            block_size: 4096,
            bloom_bits_per_key: 10,
            l0_trigger: 4,
            level1_bytes: 8 << 20,
            level_multiplier: 10,
            target_file_bytes: 2 << 20,
            levels: 5,
            manifest_max_bytes: 64 << 10,
            sync_writes: true,
            bugs: Bugs::default(),
        }
    }
}

/// Counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Memtable flushes.
    pub flushes: u64,
    /// Compactions run.
    pub compactions: u64,
    /// Bytes read by compactions.
    pub compaction_bytes_in: u64,
    /// Bytes written by compactions.
    pub compaction_bytes_out: u64,
    /// Bytes written to tables by flushes.
    pub flush_bytes: u64,
    /// Bytes appended to WALs.
    pub wal_bytes: u64,
    /// Table probes skipped by a bloom filter.
    pub bloom_skips: u64,
    /// Table probes that read a block and did not find the key.
    pub wasted_reads: u64,
    /// WAL records replayed by the last open.
    pub replayed: u64,
    /// Manifest rotations.
    pub manifest_rotations: u64,
    /// Files removed by recovery because nothing referenced them.
    pub orphans_removed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Wal,
    Table,
    Manifest,
}

fn name(kind: Kind, num: u64) -> String {
    match kind {
        Kind::Wal => format!("{num:06}.wal"),
        Kind::Table => format!("{num:06}.sst"),
        Kind::Manifest => format!("MANIFEST-{num:06}"),
    }
}

fn parse_name(n: &str) -> Option<(Kind, u64)> {
    if let Some(s) = n.strip_prefix("MANIFEST-") {
        return Some((Kind::Manifest, s.parse().ok()?));
    }
    let (num, ext) = n.split_once('.')?;
    let kind = match ext {
        "wal" => Kind::Wal,
        "sst" => Kind::Table,
        _ => return None,
    };
    Some((kind, num.parse().ok()?))
}

const PUT: u8 = 1;
const DEL: u8 = 2;

/// A live key and its value.
pub type Pair = (Vec<u8>, Vec<u8>);

/// Entry tagged with its source priority (lower = newer).
type Ranked = (Vec<u8>, (usize, u64), Option<Vec<u8>>);

struct Job {
    level: usize,
    inputs: Vec<(usize, FileMeta)>,
}

/// An LSM-tree database over a [`Vfs`]. Single writer; not `Sync`.
pub struct Db<V: Vfs> {
    vfs: V,
    o: Options,
    v: Version,
    tables: HashMap<u64, Table>,
    mem: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    mem_bytes: usize,
    wal: String,
    manifest: String,
    manifest_size: u64,
    compact_ptr: Vec<Vec<u8>>,
    poisoned: bool,
    /// Counters since open.
    pub stats: Stats,
}

impl<V: Vfs> Db<V> {
    /// Open or create a database, running recovery.
    pub fn open(mut vfs: V, o: Options) -> Result<Self, Error> {
        assert!(o.levels >= 2 && o.levels < 256 && o.l0_trigger >= 1);
        let names = vfs.list()?;
        let mut max_num = 0;
        let mut manifests = Vec::new();
        let mut wals = Vec::new();
        for n in &names {
            if let Some((k, num)) = parse_name(n) {
                max_num = max_num.max(num);
                match k {
                    Kind::Manifest => manifests.push(num),
                    Kind::Wal => wals.push(num),
                    Kind::Table => {}
                }
            }
        }
        manifests.sort_unstable_by(|a, b| b.cmp(a));
        let mut v = Version::empty(o.levels);
        for m in manifests {
            let bytes = vfs.read_all(&name(Kind::Manifest, m))?;
            let (recs, _) = unframe(&bytes);
            let Some(first) = recs.first() else { continue };
            match Edit::decode(first) {
                Ok((e, true)) => {
                    v.apply(&e)?;
                    for r in &recs[1..] {
                        let (e, snap) = Edit::decode(r)?;
                        if snap {
                            return corrupt("manifest: snapshot after the first record");
                        }
                        v.apply(&e)?;
                    }
                    break;
                }
                _ => continue,
            }
        }
        v.check()?;
        v.next_file = v.next_file.max(max_num + 1);
        let mut db = Db {
            vfs,
            o,
            compact_ptr: vec![Vec::new(); o.levels],
            v,
            tables: HashMap::new(),
            mem: BTreeMap::new(),
            mem_bytes: 0,
            wal: String::new(),
            manifest: String::new(),
            manifest_size: 0,
            poisoned: false,
            stats: Stats::default(),
        };
        let live: Vec<u64> = db.v.live().map(|f| f.num).collect();
        for num in live {
            let t = Table::open(&mut db.vfs, &name(Kind::Table, num)).map_err(|e| {
                Error::Corruption(format!("table {num} in manifest unreadable: {e}"))
            })?;
            db.tables.insert(num, t);
        }
        wals.sort_unstable();
        for w in wals.into_iter().filter(|w| *w >= db.v.log_number) {
            let bytes = db.vfs.read_all(&name(Kind::Wal, w))?;
            for r in unframe(&bytes).0 {
                let mut d = Dec::new(r);
                let bad = || Error::Corruption("wal: bad record".into());
                let kind = d.u8().ok_or_else(bad)?;
                let k = d.bytes().ok_or_else(bad)?.to_vec();
                let val = match kind {
                    PUT => Some(d.bytes().ok_or_else(bad)?.to_vec()),
                    DEL => None,
                    _ => return Err(bad()),
                };
                db.mem.insert(k, val);
                db.stats.replayed += 1;
            }
        }
        let mut edit_added = Vec::new();
        if !db.mem.is_empty() {
            let entries: Vec<Entry> = std::mem::take(&mut db.mem).into_iter().collect();
            edit_added.push((0, db.write_table(&entries)?));
        }
        let wal_num = db.alloc();
        db.wal = name(Kind::Wal, wal_num);
        db.vfs.create(&db.wal)?;
        for (l, f) in &edit_added {
            db.v.levels[*l].push(f.clone());
        }
        db.v.log_number = wal_num;
        db.write_snapshot()?;
        // Everything not referenced by the new manifest is garbage.
        let live: std::collections::HashSet<String> =
            db.v.live()
                .map(|f| name(Kind::Table, f.num))
                .chain([db.wal.clone(), db.manifest.clone()])
                .collect();
        for n in db.vfs.list()? {
            if parse_name(&n).is_some() && !live.contains(&n) {
                db.vfs.remove(&n)?;
                db.stats.orphans_removed += 1;
            }
        }
        db.maybe_compact()?;
        Ok(db)
    }

    fn alloc(&mut self) -> u64 {
        let n = self.v.next_file;
        self.v.next_file += 1;
        n
    }

    fn sync_dir(&mut self) -> Result<(), Error> {
        if !self.o.bugs.skip_dir_sync {
            self.vfs.sync_dir()?;
        }
        Ok(())
    }

    /// Write a full snapshot of `self.v` to a new manifest and retire the old one.
    fn write_snapshot(&mut self) -> Result<(), Error> {
        let num = self.alloc();
        let new = name(Kind::Manifest, num);
        let rec = frame(&self.v.snapshot().encode(true));
        self.vfs.create(&new)?;
        self.vfs.append(&new, &rec)?;
        self.vfs.sync(&new)?;
        self.sync_dir()?;
        let old = std::mem::replace(&mut self.manifest, new);
        if !old.is_empty() {
            self.vfs.remove(&old)?;
            self.stats.manifest_rotations += 1;
        }
        self.manifest_size = rec.len() as u64;
        Ok(())
    }

    /// Commit an edit: the commit point of flushes and compactions.
    fn log_edit(&mut self, mut e: Edit) -> Result<(), Error> {
        e.next_file = self.v.next_file;
        let mut next = self.v.clone();
        next.apply(&e)?;
        if self.manifest_size > self.o.manifest_max_bytes {
            let old = std::mem::replace(&mut self.v, next);
            if let Err(err) = self.write_snapshot() {
                self.v = old;
                return Err(err);
            }
        } else {
            let rec = frame(&e.encode(false));
            self.vfs.append(&self.manifest, &rec)?;
            self.vfs.sync(&self.manifest)?;
            self.manifest_size += rec.len() as u64;
            self.v = next;
        }
        Ok(())
    }

    fn write_table(&mut self, entries: &[Entry]) -> Result<FileMeta, Error> {
        let num = self.alloc();
        let n = name(Kind::Table, num);
        let img = sst::build(entries, self.o.block_size, self.o.bloom_bits_per_key);
        self.vfs.create(&n)?;
        self.vfs.append(&n, &img)?;
        if !self.o.bugs.skip_table_sync {
            self.vfs.sync(&n)?;
        }
        let t = Table::open(&mut self.vfs, &n)?;
        self.tables.insert(num, t);
        Ok(FileMeta {
            num,
            size: img.len() as u64,
            smallest: entries.first().map(|e| e.0.clone()).unwrap_or_default(),
            largest: entries.last().map(|e| e.0.clone()).unwrap_or_default(),
        })
    }

    fn guard<T>(&mut self, r: Result<T, Error>) -> Result<T, Error> {
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn write(&mut self, key: &[u8], val: Option<&[u8]>) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if key.len() > MAX_KEY || val.is_some_and(|v| v.len() > MAX_VALUE) {
            return Err(Error::TooLarge);
        }
        let mut e = Enc::default();
        match val {
            Some(v) => e.u8(PUT).bytes(key).bytes(v),
            None => e.u8(DEL).bytes(key),
        };
        let rec = frame(&e.0);
        let r = self.vfs.append(&self.wal, &rec).map_err(Error::from);
        self.guard(r)?;
        if self.o.sync_writes && !self.o.bugs.skip_wal_sync {
            let r = self.vfs.sync(&self.wal).map_err(Error::from);
            self.guard(r)?;
        }
        self.stats.wal_bytes += rec.len() as u64;
        self.mem_bytes += key.len() + val.map_or(0, |v| v.len()) + 24;
        self.mem.insert(key.to_vec(), val.map(|v| v.to_vec()));
        if self.mem_bytes >= self.o.memtable_bytes {
            let r = self.flush().and_then(|_| self.maybe_compact());
            self.guard(r)?;
        }
        Ok(())
    }

    /// Insert or overwrite. Durable when it returns `Ok` (with `sync_writes`).
    pub fn put(&mut self, key: &[u8], val: &[u8]) -> Result<(), Error> {
        self.write(key, Some(val))
    }

    /// Delete (writes a tombstone).
    pub fn delete(&mut self, key: &[u8]) -> Result<(), Error> {
        self.write(key, None)
    }

    /// Flush the memtable to a level-0 table now (no-op when empty).
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.mem.is_empty() {
            return Ok(());
        }
        let entries: Vec<Entry> = self
            .mem
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let meta = self.write_table(&entries)?;
        self.stats.flush_bytes += meta.size;
        let wal_num = self.alloc();
        let new_wal = name(Kind::Wal, wal_num);
        self.vfs.create(&new_wal)?;
        self.sync_dir()?;
        self.log_edit(Edit {
            log_number: wal_num,
            added: vec![(0, meta)],
            ..Edit::default()
        })?;
        let old = std::mem::replace(&mut self.wal, new_wal);
        self.mem.clear();
        self.mem_bytes = 0;
        self.stats.flushes += 1;
        self.vfs.remove(&old)?;
        Ok(())
    }

    fn level_limit(&self, l: usize) -> u64 {
        let mut lim = self.o.level1_bytes;
        for _ in 1..l {
            lim = lim.saturating_mul(self.o.level_multiplier);
        }
        lim
    }

    fn pick(&self) -> Option<Job> {
        let l0 = &self.v.levels[0];
        if l0.len() >= self.o.l0_trigger {
            let lo = l0.iter().map(|f| &f.smallest).min()?.clone();
            let hi = l0.iter().map(|f| &f.largest).max()?.clone();
            let mut inputs: Vec<_> = l0.iter().map(|f| (0, f.clone())).collect();
            inputs.extend(
                self.v.levels[1]
                    .iter()
                    .filter(|f| f.overlaps(&lo, &hi))
                    .map(|f| (1, f.clone())),
            );
            return Some(Job { level: 1, inputs });
        }
        for l in 1..self.o.levels - 1 {
            let bytes: u64 = self.v.levels[l].iter().map(|f| f.size).sum();
            if bytes <= self.level_limit(l) {
                continue;
            }
            let fs = &self.v.levels[l];
            let ptr = &self.compact_ptr[l];
            let f = fs
                .iter()
                .find(|f| f.smallest > *ptr)
                .unwrap_or(&fs[0])
                .clone();
            let mut inputs = vec![(l, f.clone())];
            inputs.extend(
                self.v.levels[l + 1]
                    .iter()
                    .filter(|g| g.overlaps(&f.smallest, &f.largest))
                    .map(|g| (l + 1, g.clone())),
            );
            return Some(Job {
                level: l + 1,
                inputs,
            });
        }
        None
    }

    /// Whether a key could exist in a level below `level`.
    fn deeper_may_have(&self, level: usize, key: &[u8]) -> bool {
        self.v.levels[level + 1..]
            .iter()
            .flatten()
            .any(|f| f.overlaps(key, key))
    }

    /// Run compactions until no level is over its limit.
    pub fn maybe_compact(&mut self) -> Result<(), Error> {
        while let Some(job) = self.pick() {
            self.compact(job)?;
        }
        Ok(())
    }

    fn compact(&mut self, job: Job) -> Result<(), Error> {
        // Priority: lower = newer. L0 by descending file number, then levels.
        let mut all: Vec<Ranked> = Vec::new();
        for (l, f) in &job.inputs {
            let rank = if *l == 0 { u64::MAX - f.num } else { 0 };
            let t = self.tables[&f.num].clone();
            for (k, v) in t.scan(&mut self.vfs, &name(Kind::Table, f.num))? {
                all.push((k, (*l, rank), v));
            }
            self.stats.compaction_bytes_in += f.size;
        }
        all.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        all.dedup_by(|later, first| later.0 == first.0);
        let mut outs: Vec<Vec<Entry>> = vec![Vec::new()];
        let mut bytes = 0;
        for (k, _, v) in all {
            if v.is_none()
                && (self.o.bugs.drop_tombstones_early || !self.deeper_may_have(job.level, &k))
            {
                continue;
            }
            if bytes >= self.o.target_file_bytes {
                outs.push(Vec::new());
                bytes = 0;
            }
            bytes += k.len() + v.as_ref().map_or(0, |v| v.len()) + 10;
            outs.last_mut().expect("nonempty").push((k, v));
        }
        let mut added = Vec::new();
        for out in outs.iter().filter(|o| !o.is_empty()) {
            let m = self.write_table(out)?;
            self.stats.compaction_bytes_out += m.size;
            added.push((job.level, m));
        }
        self.sync_dir()?;
        if self.o.bugs.early_delete {
            for (_, f) in &job.inputs {
                self.vfs.remove(&name(Kind::Table, f.num))?;
            }
        }
        let deleted: Vec<_> = job.inputs.iter().map(|(l, f)| (*l, f.num)).collect();
        self.log_edit(Edit {
            log_number: self.v.log_number,
            deleted,
            added,
            ..Edit::default()
        })?;
        if let Some((l, f)) = job.inputs.first() {
            if *l > 0 {
                self.compact_ptr[*l] = f.largest.clone();
            }
        }
        for (_, f) in &job.inputs {
            self.tables.remove(&f.num);
            if !self.o.bugs.early_delete {
                self.vfs.remove(&name(Kind::Table, f.num))?;
            }
        }
        self.stats.compactions += 1;
        Ok(())
    }

    /// Tables a lookup for `key` would probe, newest first.
    fn candidates(&self, key: &[u8]) -> Vec<u64> {
        let mut out: Vec<u64> = self.v.levels[0]
            .iter()
            .rev()
            .filter(|f| f.overlaps(key, key))
            .map(|f| f.num)
            .collect();
        for fs in &self.v.levels[1..] {
            let i = fs.partition_point(|f| f.largest.as_slice() < key);
            if let Some(f) = fs.get(i).filter(|f| f.smallest.as_slice() <= key) {
                out.push(f.num);
            }
        }
        out
    }

    /// Point lookup.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if let Some(v) = self.mem.get(key) {
            return Ok(v.clone());
        }
        let h = hash(key);
        for num in self.candidates(key) {
            let t = &self.tables[&num];
            match t.get(&mut self.vfs, &name(Kind::Table, num), key, h)? {
                Lookup::Found(v) => return Ok(v),
                Lookup::Filtered => self.stats.bloom_skips += 1,
                Lookup::Absent => self.stats.wasted_reads += 1,
            }
        }
        Ok(None)
    }

    /// Live pairs with `start <= key < end` (`end = None`: unbounded), in order.
    pub fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Pair>, Error> {
        let hi_ok = |k: &[u8]| end.map_or(true, |e| k < e);
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        // Oldest first, so newer sources overwrite.
        let mut order: Vec<u64> = Vec::new();
        for fs in self.v.levels[1..].iter().rev() {
            order.extend(fs.iter().map(|f| f.num));
        }
        order.extend(self.v.levels[0].iter().map(|f| f.num));
        let metas: HashMap<u64, FileMeta> = self.v.live().map(|f| (f.num, f.clone())).collect();
        for num in order {
            let m = &metas[&num];
            if m.largest.as_slice() < start || !hi_ok(&m.smallest) {
                continue;
            }
            let t = &self.tables[&num];
            for (k, v) in t.range(&mut self.vfs, &name(Kind::Table, num), start, end)? {
                merged.insert(k, v);
            }
        }
        for (k, v) in self.mem.range(start.to_vec()..) {
            if !hi_ok(k) {
                break;
            }
            merged.insert(k.clone(), v.clone());
        }
        Ok(merged
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
    }

    /// Verify every invariant and every table byte; returns the number of
    /// live keys.
    pub fn check(&mut self) -> Result<usize, Error> {
        self.v.check()?;
        let on_disk: std::collections::HashSet<String> = self.vfs.list()?.into_iter().collect();
        let metas: Vec<FileMeta> = self.v.live().cloned().collect();
        for m in &metas {
            let n = name(Kind::Table, m.num);
            if !on_disk.contains(&n) {
                return corrupt(format!("{n} missing"));
            }
            let e = self.tables[&m.num].scan(&mut self.vfs, &n)?;
            if e.first().map(|x| &x.0) != Some(&m.smallest)
                || e.last().map(|x| &x.0) != Some(&m.largest)
                || self.tables[&m.num].size != m.size
            {
                return corrupt(format!("{n}: metadata disagrees with contents"));
            }
        }
        if !on_disk.contains(&self.wal) || !on_disk.contains(&self.manifest) {
            return corrupt("active WAL or manifest missing");
        }
        Ok(self.range(b"", None)?.len())
    }

    /// Number of tables per level.
    pub fn level_counts(&self) -> Vec<usize> {
        self.v.levels.iter().map(|l| l.len()).collect()
    }

    /// Bytes per level.
    pub fn level_bytes(&self) -> Vec<u64> {
        self.v
            .levels
            .iter()
            .map(|l| l.iter().map(|f| f.size).sum())
            .collect()
    }

    /// Give back the storage.
    pub fn into_vfs(self) -> V {
        self.vfs
    }

    /// Borrow the storage.
    pub fn vfs_mut(&mut self) -> &mut V {
        &mut self.vfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_roundtrip() {
        for k in [Kind::Wal, Kind::Table, Kind::Manifest] {
            assert_eq!(parse_name(&name(k, 42)), Some((k, 42)));
        }
        assert_eq!(parse_name("LOCK"), None);
        assert_eq!(parse_name("x.tmp"), None);
    }
}
