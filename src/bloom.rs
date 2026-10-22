//! Bloom filter with double hashing (Kirsch–Mitzenmacher).

/// 64-bit hash of a key: FNV-1a followed by a SplitMix64 finalizer.
pub fn hash(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 31)
}

/// A built filter: bit array followed by one byte with the probe count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    bits: Vec<u8>,
    k: u8,
}

impl Bloom {
    /// Filter for `hashes` with `bits_per_key` bits each. `bits_per_key == 0`
    /// builds an empty filter that answers "maybe" for everything.
    pub fn build(hashes: &[u64], bits_per_key: usize) -> Self {
        if bits_per_key == 0 {
            return Bloom {
                bits: Vec::new(),
                k: 0,
            };
        }
        // k = ln 2 * bits/key is optimal.
        let k = ((bits_per_key as f64 * 0.69).round() as u8).clamp(1, 30);
        let nbits = (hashes.len() * bits_per_key).max(64);
        let nbytes = nbits.div_ceil(8);
        let nbits = nbytes * 8;
        let mut bits = vec![0u8; nbytes];
        for &h in hashes {
            let (mut a, b) = (h, h.rotate_right(33) | 1);
            for _ in 0..k {
                let bit = (a % nbits as u64) as usize;
                bits[bit / 8] |= 1 << (bit % 8);
                a = a.wrapping_add(b);
            }
        }
        Bloom { bits, k }
    }

    /// False means the key is definitely absent.
    pub fn may_contain(&self, h: u64) -> bool {
        if self.k == 0 || self.bits.is_empty() {
            return true;
        }
        let nbits = (self.bits.len() * 8) as u64;
        let (mut a, b) = (h, h.rotate_right(33) | 1);
        for _ in 0..self.k {
            let bit = (a % nbits) as usize;
            if self.bits[bit / 8] & (1 << (bit % 8)) == 0 {
                return false;
            }
            a = a.wrapping_add(b);
        }
        true
    }

    /// Serialized form.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = self.bits.clone();
        v.push(self.k);
        v
    }

    /// Parse [`Bloom::encode`] output.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&k, bits) = b.split_last()?;
        Some(Bloom {
            bits: bits.to_vec(),
            k,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_low_false_positive_rate() {
        let keys: Vec<u64> = (0..10_000u32)
            .map(|i| hash(format!("key-{i}").as_bytes()))
            .collect();
        let f = Bloom::decode(&Bloom::build(&keys, 10).encode()).unwrap();
        assert!(keys.iter().all(|h| f.may_contain(*h)));
        let fp = (0..100_000u32)
            .filter(|i| f.may_contain(hash(format!("other-{i}").as_bytes())))
            .count();
        // Theory for 10 bits/key and k = 7: about 0.82 %.
        assert!(fp < 1_500, "false positives: {fp} / 100000");
    }

    #[test]
    fn disabled_filter_says_maybe() {
        let f = Bloom::build(&[1, 2, 3], 0);
        assert!(f.may_contain(12345));
    }
}
