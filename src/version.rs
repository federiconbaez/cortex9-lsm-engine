//! The set of live tables per level, and the manifest records that
//! describe how it changes.

use crate::codec::{Dec, Enc};
use crate::error::{corrupt, Error};

/// One table in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// File number (`NNNNNN.sst`).
    pub num: u64,
    /// Size in bytes.
    pub size: u64,
    /// Smallest key.
    pub smallest: Vec<u8>,
    /// Largest key.
    pub largest: Vec<u8>,
}

impl FileMeta {
    /// Whether `[lo, hi]` intersects this file's key range.
    pub fn overlaps(&self, lo: &[u8], hi: &[u8]) -> bool {
        self.smallest.as_slice() <= hi && lo <= self.largest.as_slice()
    }
}

/// Live state recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `levels[0]` may overlap and is ordered by file number (oldest
    /// first); deeper levels are sorted by key and disjoint.
    pub levels: Vec<Vec<FileMeta>>,
    /// WAL files with a number below this are obsolete.
    pub log_number: u64,
    /// Next file number to hand out.
    pub next_file: u64,
}

/// A change to a [`Version`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    /// Replaces [`Version::log_number`].
    pub log_number: u64,
    /// Replaces [`Version::next_file`].
    pub next_file: u64,
    /// `(level, file number)` removed.
    pub deleted: Vec<(usize, u64)>,
    /// `(level, file)` added.
    pub added: Vec<(usize, FileMeta)>,
}

const SNAPSHOT: u8 = 1;
const EDIT: u8 = 2;

impl Version {
    /// Empty tree with `levels` levels.
    pub fn empty(levels: usize) -> Self {
        Version {
            levels: vec![Vec::new(); levels],
            log_number: 0,
            next_file: 1,
        }
    }

    /// Apply an edit, keeping the level orderings.
    pub fn apply(&mut self, e: &Edit) -> Result<(), Error> {
        self.log_number = e.log_number;
        self.next_file = self.next_file.max(e.next_file);
        for (l, num) in &e.deleted {
            let lv = self
                .levels
                .get_mut(*l)
                .ok_or_else(|| Error::Corruption(format!("edit: level {l}")))?;
            let before = lv.len();
            lv.retain(|f| f.num != *num);
            if lv.len() == before {
                return corrupt(format!("edit deletes unknown file {num} at L{l}"));
            }
        }
        for (l, f) in &e.added {
            let lv = self
                .levels
                .get_mut(*l)
                .ok_or_else(|| Error::Corruption(format!("edit: level {l}")))?;
            lv.push(f.clone());
            if *l == 0 {
                lv.sort_by_key(|f| f.num);
            } else {
                lv.sort_by(|a, b| a.smallest.cmp(&b.smallest));
            }
        }
        Ok(())
    }

    /// Edit that rebuilds this version from an empty one.
    pub fn snapshot(&self) -> Edit {
        Edit {
            log_number: self.log_number,
            next_file: self.next_file,
            deleted: Vec::new(),
            added: self
                .levels
                .iter()
                .enumerate()
                .flat_map(|(l, fs)| fs.iter().map(move |f| (l, f.clone())))
                .collect(),
        }
    }

    /// Every live table number.
    pub fn live(&self) -> impl Iterator<Item = &FileMeta> {
        self.levels.iter().flatten()
    }

    /// Structural invariants: deeper levels sorted and disjoint.
    pub fn check(&self) -> Result<(), Error> {
        for (l, fs) in self.levels.iter().enumerate().skip(1) {
            for w in fs.windows(2) {
                if w[0].largest >= w[1].smallest {
                    return corrupt(format!("L{l}: files {} and {} overlap", w[0].num, w[1].num));
                }
            }
        }
        for f in self.live() {
            if f.smallest > f.largest || f.num >= self.next_file {
                return corrupt(format!("file {}: bad metadata", f.num));
            }
        }
        Ok(())
    }
}

impl Edit {
    /// Manifest record payload; `snapshot` marks a self-contained record.
    pub fn encode(&self, snapshot: bool) -> Vec<u8> {
        let mut e = Enc::default();
        e.u8(if snapshot { SNAPSHOT } else { EDIT })
            .u64(self.log_number)
            .u64(self.next_file)
            .u32(self.deleted.len() as u32);
        for (l, n) in &self.deleted {
            e.u8(*l as u8).u64(*n);
        }
        e.u32(self.added.len() as u32);
        for (l, f) in &self.added {
            e.u8(*l as u8)
                .u64(f.num)
                .u64(f.size)
                .bytes(&f.smallest)
                .bytes(&f.largest);
        }
        e.0
    }

    /// Parse; returns the edit and whether it is a snapshot.
    pub fn decode(b: &[u8]) -> Result<(Edit, bool), Error> {
        let bad = || Error::Corruption("manifest: bad record".into());
        let mut d = Dec::new(b);
        let snap = match d.u8().ok_or_else(bad)? {
            SNAPSHOT => true,
            EDIT => false,
            _ => return Err(bad()),
        };
        let mut e = Edit {
            log_number: d.u64().ok_or_else(bad)?,
            next_file: d.u64().ok_or_else(bad)?,
            ..Edit::default()
        };
        for _ in 0..d.u32().ok_or_else(bad)? {
            e.deleted
                .push((d.u8().ok_or_else(bad)? as usize, d.u64().ok_or_else(bad)?));
        }
        for _ in 0..d.u32().ok_or_else(bad)? {
            let l = d.u8().ok_or_else(bad)? as usize;
            let f = FileMeta {
                num: d.u64().ok_or_else(bad)?,
                size: d.u64().ok_or_else(bad)?,
                smallest: d.bytes().ok_or_else(bad)?.to_vec(),
                largest: d.bytes().ok_or_else(bad)?.to_vec(),
            };
            e.added.push((l, f));
        }
        if d.remaining() != 0 {
            return Err(bad());
        }
        Ok((e, snap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(num: u64, a: &str, b: &str) -> FileMeta {
        FileMeta {
            num,
            size: 100,
            smallest: a.into(),
            largest: b.into(),
        }
    }

    #[test]
    fn edits_roundtrip_and_apply() {
        let mut v = Version::empty(3);
        let e = Edit {
            log_number: 4,
            next_file: 9,
            deleted: vec![],
            added: vec![
                (0, f(5, "a", "z")),
                (1, f(7, "m", "p")),
                (1, f(6, "a", "c")),
            ],
        };
        let (d, snap) = Edit::decode(&e.encode(false)).unwrap();
        assert!(!snap);
        assert_eq!(d, e);
        v.apply(&e).unwrap();
        assert_eq!(v.levels[1][0].num, 6, "L1 sorted by key");
        v.check().unwrap();
        let mut w = Version::empty(3);
        w.apply(&v.snapshot()).unwrap();
        assert_eq!(w, v);
        let del = Edit {
            log_number: 4,
            next_file: 9,
            deleted: vec![(1, 99)],
            added: vec![],
        };
        assert!(v.apply(&del).is_err());
    }

    #[test]
    fn overlapping_deeper_level_is_rejected() {
        let mut v = Version::empty(3);
        v.next_file = 10;
        v.levels[1] = vec![f(1, "a", "f"), f(2, "f", "k")];
        assert!(v.check().is_err());
    }
}
