//! Little-endian encoding helpers and the checksummed record framing
//! shared by the WAL and the manifest: `crc32 | len | payload`, where the
//! CRC covers `len` and `payload`.

use crate::crc::crc32;

/// Append-only byte encoder.
#[derive(Debug, Default, Clone)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    /// `u8`.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    /// `u32` LE.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// `u64` LE.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
}

/// Bounds-checked decoder; every getter returns `None` past the end.
#[derive(Debug, Clone)]
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    /// Over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    /// `u8`.
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    /// `u32` LE.
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    /// `u64` LE.
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Frame one record.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let c = crc32(&out[4..]);
    out[..4].copy_from_slice(&c.to_le_bytes());
    out
}

/// Decode records from the start of `buf` up to the first truncated or
/// corrupt one. Returns the payloads and the length of the valid prefix.
pub fn unframe(buf: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut out = Vec::new();
    let mut pos = 0;
    while buf.len() - pos >= 8 {
        let c = u32::from_le_bytes(buf[pos..pos + 4].try_into().expect("4 bytes"));
        let n = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().expect("4 bytes")) as usize;
        let Some(end) = (pos + 8).checked_add(n).filter(|e| *e <= buf.len()) else {
            break;
        };
        if crc32(&buf[pos + 4..end]) != c {
            break;
        }
        out.push(&buf[pos + 8..end]);
        pos = end;
    }
    (out, pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_and_stop_at_damage() {
        let mut log = Vec::new();
        for p in [&b"one"[..], b"", b"three"] {
            log.extend(frame(p));
        }
        let good = log.len();
        log.extend(frame(b"four"));
        let (recs, n) = unframe(&log);
        assert_eq!(recs, vec![&b"one"[..], b"", b"three", b"four"]);
        assert_eq!(n, log.len());
        // Flip a payload byte of the last record.
        let last = log.len() - 1;
        log[last] ^= 1;
        let (recs, n) = unframe(&log);
        assert_eq!(recs.len(), 3);
        assert_eq!(n, good);
        // Truncated tail.
        let (recs, _) = unframe(&log[..good + 5]);
        assert_eq!(recs.len(), 3);
        // Zeroed tail (a lost sector) is not a valid empty record.
        let mut z = log[..good].to_vec();
        z.extend([0u8; 16]);
        assert_eq!(unframe(&z).0.len(), 3);
    }

    #[test]
    fn decoder_is_bounds_checked() {
        let mut e = Enc::default();
        e.u8(1).u32(2).u64(3).bytes(b"xy");
        let mut d = Dec::new(&e.0);
        assert_eq!(d.u8(), Some(1));
        assert_eq!(d.u32(), Some(2));
        assert_eq!(d.u64(), Some(3));
        assert_eq!(d.bytes(), Some(&b"xy"[..]));
        assert_eq!(d.u8(), None);
        let mut d = Dec::new(&[255, 255, 255, 255]);
        assert_eq!(d.bytes(), None);
    }
}
