//! Sorted string tables.
//!
//! Layout: data blocks, index, bloom filter, 48-byte footer. Every block,
//! the index, the filter and the footer carry their own CRC-32, so a torn
//! or truncated table is detected when it is opened or read.
//!
//! ```text
//! block  := entry* crc32
//! entry  := kind:u8 key:bytes [value:bytes if kind == PUT]
//! index  := n:u32 (last_key:bytes offset:u64 len:u32)* crc32
//! bloom  := bits k:u8 crc32
//! footer := index_off index_len bloom_off bloom_len entries magic:u32 crc32
//! ```

use crate::bloom::{hash, Bloom};
use crate::codec::{Dec, Enc};
use crate::crc::crc32;
use crate::error::{corrupt, Error};
use crate::vfs::Vfs;

/// A key and its value; `None` is a tombstone.
pub type Entry = (Vec<u8>, Option<Vec<u8>>);

const PUT: u8 = 1;
const DEL: u8 = 2;
const MAGIC: u32 = 0x4C53_3943; // "C9SL"
/// Footer size.
pub const FOOTER: usize = 48;

fn seal(mut payload: Vec<u8>) -> Vec<u8> {
    let c = crc32(&payload);
    payload.extend_from_slice(&c.to_le_bytes());
    payload
}

fn unseal<'a>(b: &'a [u8], what: &str) -> Result<&'a [u8], Error> {
    if b.len() < 4 {
        return corrupt(format!("{what}: too short"));
    }
    let (p, c) = b.split_at(b.len() - 4);
    if crc32(p) != u32::from_le_bytes(c.try_into().expect("4 bytes")) {
        return corrupt(format!("{what}: checksum mismatch"));
    }
    Ok(p)
}

/// Serialize sorted, unique `entries` into a table image.
pub fn build(entries: &[Entry], block_size: usize, bits_per_key: usize) -> Vec<u8> {
    debug_assert!(entries.windows(2).all(|w| w[0].0 < w[1].0));
    let mut out = Vec::new();
    let mut index = Enc::default();
    let mut nblocks = 0u32;
    let mut block = Enc::default();
    let mut last: &[u8] = &[];
    let flush = |block: &mut Enc, out: &mut Vec<u8>, index: &mut Enc, last: &[u8]| {
        let off = out.len() as u64;
        let b = seal(std::mem::take(&mut block.0));
        index.bytes(last).u64(off).u32(b.len() as u32);
        out.extend(b);
    };
    for (k, v) in entries {
        match v {
            Some(v) => block.u8(PUT).bytes(k).bytes(v),
            None => block.u8(DEL).bytes(k),
        };
        last = k;
        if block.0.len() >= block_size {
            flush(&mut block, &mut out, &mut index, last);
            nblocks += 1;
        }
    }
    if !block.0.is_empty() {
        flush(&mut block, &mut out, &mut index, last);
        nblocks += 1;
    }
    let mut idx = Enc::default();
    idx.u32(nblocks);
    idx.0.extend(index.0);
    let idx = seal(idx.0);
    let index_off = out.len() as u64;
    out.extend_from_slice(&idx);
    let hashes: Vec<u64> = entries.iter().map(|(k, _)| hash(k)).collect();
    let bloom = seal(Bloom::build(&hashes, bits_per_key).encode());
    let bloom_off = out.len() as u64;
    out.extend_from_slice(&bloom);
    let mut f = Enc::default();
    f.u64(index_off)
        .u64(idx.len() as u64)
        .u64(bloom_off)
        .u64(bloom.len() as u64)
        .u64(entries.len() as u64)
        .u32(MAGIC);
    out.extend(seal(f.0));
    out
}

#[derive(Debug, Clone)]
struct BlockHandle {
    last: Vec<u8>,
    off: u64,
    len: u32,
}

/// An open table: index and filter in memory, blocks read on demand.
#[derive(Debug, Clone)]
pub struct Table {
    index: Vec<BlockHandle>,
    bloom: Bloom,
    /// Number of entries (including tombstones).
    pub entries: u64,
    /// File size in bytes.
    pub size: u64,
}

/// Result of a point lookup in one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The filter ruled the key out without reading a block.
    Filtered,
    /// A block was read and the key is not in it.
    Absent,
    /// The key is in the table (`None` = tombstone).
    Found(Option<Vec<u8>>),
}

fn decode_block(b: &[u8], out: &mut Vec<Entry>) -> Result<(), Error> {
    let p = unseal(b, "block")?;
    let mut d = Dec::new(p);
    while d.remaining() > 0 {
        let bad = || Error::Corruption("block: bad entry".into());
        let kind = d.u8().ok_or_else(bad)?;
        let k = d.bytes().ok_or_else(bad)?.to_vec();
        let v = match kind {
            PUT => Some(d.bytes().ok_or_else(bad)?.to_vec()),
            DEL => None,
            _ => return Err(bad()),
        };
        out.push((k, v));
    }
    Ok(())
}

impl Table {
    /// Open `name`, validating footer, index and filter.
    pub fn open(vfs: &mut dyn Vfs, name: &str) -> Result<Table, Error> {
        let size = vfs.size(name)?;
        if (size as usize) < FOOTER {
            return corrupt(format!("{name}: too short"));
        }
        let fb = vfs.read(name, size - FOOTER as u64, FOOTER)?;
        let f = unseal(&fb, name)?;
        let mut d = Dec::new(f);
        let (io, il, bo, bl, n, magic) = (
            d.u64().unwrap_or(0),
            d.u64().unwrap_or(0),
            d.u64().unwrap_or(0),
            d.u64().unwrap_or(0),
            d.u64().unwrap_or(0),
            d.u32().unwrap_or(0),
        );
        if magic != MAGIC || io + il != bo || bo + bl != size - FOOTER as u64 {
            return corrupt(format!("{name}: bad footer"));
        }
        let ib = vfs.read(name, io, il as usize)?;
        let ip = unseal(&ib, "index")?;
        let mut d = Dec::new(ip);
        let bad = || Error::Corruption(format!("{name}: bad index"));
        let nb = d.u32().ok_or_else(bad)?;
        let mut index = Vec::with_capacity(nb as usize);
        for _ in 0..nb {
            let last = d.bytes().ok_or_else(bad)?.to_vec();
            let off = d.u64().ok_or_else(bad)?;
            let len = d.u32().ok_or_else(bad)?;
            if off + len as u64 > io {
                return Err(bad());
            }
            index.push(BlockHandle { last, off, len });
        }
        let bb = vfs.read(name, bo, bl as usize)?;
        let bloom = Bloom::decode(unseal(&bb, "bloom")?).ok_or_else(bad)?;
        Ok(Table {
            index,
            bloom,
            entries: n,
            size,
        })
    }

    fn block(&self, vfs: &mut dyn Vfs, name: &str, i: usize) -> Result<Vec<Entry>, Error> {
        let h = &self.index[i];
        let b = vfs.read(name, h.off, h.len as usize)?;
        let mut out = Vec::new();
        decode_block(&b, &mut out)?;
        Ok(out)
    }

    /// Point lookup; `h` is [`hash`] of `key`.
    pub fn get(&self, vfs: &mut dyn Vfs, name: &str, key: &[u8], h: u64) -> Result<Lookup, Error> {
        if !self.bloom.may_contain(h) {
            return Ok(Lookup::Filtered);
        }
        let i = self.index.partition_point(|b| b.last.as_slice() < key);
        if i == self.index.len() {
            return Ok(Lookup::Absent);
        }
        let block = self.block(vfs, name, i)?;
        Ok(
            match block.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                Ok(j) => Lookup::Found(block[j].1.clone()),
                Err(_) => Lookup::Absent,
            },
        )
    }

    /// Every entry with `start <= key < end` (`end = None` is unbounded),
    /// reading only the blocks that can hold such keys.
    pub fn range(
        &self,
        vfs: &mut dyn Vfs,
        name: &str,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Vec<Entry>, Error> {
        let mut out = Vec::new();
        let first = self.index.partition_point(|b| b.last.as_slice() < start);
        for i in first..self.index.len() {
            let mut b = Vec::new();
            let h = &self.index[i];
            decode_block(&vfs.read(name, h.off, h.len as usize)?, &mut b)?;
            let mut past_end = false;
            for (k, v) in b {
                if end.is_some_and(|e| k.as_slice() >= e) {
                    past_end = true;
                    break;
                }
                if k.as_slice() >= start {
                    out.push((k, v));
                }
            }
            if past_end {
                break;
            }
        }
        Ok(out)
    }

    /// Every entry, verifying order and block checksums.
    pub fn scan(&self, vfs: &mut dyn Vfs, name: &str) -> Result<Vec<Entry>, Error> {
        let mut out: Vec<Entry> = Vec::new();
        for i in 0..self.index.len() {
            let b = self.block(vfs, name, i)?;
            if b.last().map(|e| &e.0) != Some(&self.index[i].last) {
                return corrupt(format!("{name}: index key mismatch"));
            }
            out.extend(b);
        }
        if !out.windows(2).all(|w| w[0].0 < w[1].0) || out.len() as u64 != self.entries {
            return corrupt(format!("{name}: entries out of order or miscounted"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::SimVfs;

    fn sample(n: u32) -> Vec<Entry> {
        (0..n)
            .map(|i| {
                let k = format!("k{i:05}").into_bytes();
                let v = if i % 7 == 0 {
                    None
                } else {
                    Some(vec![i as u8; (i % 50) as usize])
                };
                (k, v)
            })
            .collect()
    }

    fn write(v: &mut SimVfs, name: &str, img: &[u8]) {
        v.create(name).unwrap();
        v.append(name, img).unwrap();
    }

    #[test]
    fn roundtrip_get_range_scan() {
        let e = sample(2000);
        let mut v = SimVfs::new();
        write(&mut v, "t", &build(&e, 512, 10));
        let t = Table::open(&mut v, "t").unwrap();
        assert!(t.index.len() > 10);
        assert_eq!(t.scan(&mut v, "t").unwrap(), e);
        for (k, val) in e.iter().step_by(13) {
            assert_eq!(
                t.get(&mut v, "t", k, hash(k)).unwrap(),
                Lookup::Found(val.clone())
            );
        }
        let miss = b"k99999x";
        assert_ne!(
            t.get(&mut v, "t", miss, hash(miss)).unwrap(),
            Lookup::Found(None)
        );
        let r = t.range(&mut v, "t", b"k00100", Some(b"k00200")).unwrap();
        assert_eq!(r, e[100..200].to_vec());
    }

    #[test]
    fn empty_table_is_valid() {
        let mut v = SimVfs::new();
        write(&mut v, "t", &build(&[], 512, 10));
        let t = Table::open(&mut v, "t").unwrap();
        assert!(t.scan(&mut v, "t").unwrap().is_empty());
    }

    #[test]
    fn any_flipped_byte_is_detected() {
        let e = sample(300);
        let img = build(&e, 256, 10);
        for i in (0..img.len()).step_by(7) {
            let mut bad = img.clone();
            bad[i] ^= 0x40;
            let mut v = SimVfs::new();
            write(&mut v, "t", &bad);
            let r = Table::open(&mut v, "t").and_then(|t| t.scan(&mut v, "t"));
            assert!(
                matches!(r, Err(Error::Corruption(_))),
                "byte {i} flipped went unnoticed"
            );
        }
    }
}
