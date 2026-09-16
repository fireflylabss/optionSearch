//! Core record types and the byte-signature used to prefilter the scan.

use std::path::PathBuf;
use std::time::Duration;

pub mod flags {
    pub const DIR: u8 = 1 << 0;
    pub const SYMLINK: u8 = 1 << 1;
    /// Tombstone: slot is logically removed and waiting for compaction.
    pub const DEAD: u8 = 1 << 2;
    pub const HIDDEN: u8 = 1 << 3;
}

/// One indexed filesystem entry.
///
/// The in-memory index stores these fields as separate columns (see
/// [`crate::index::Index`]) so the hot search loop only streams the few bytes it
/// needs. This struct is the logical view used by insert/load APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub dir: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u8,
    pub size: u64,
    pub mtime: i64,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    #[inline]
    pub fn is_dead(&self) -> bool {
        self.flags & flags::DEAD != 0
    }
}

/// A search result, with the path materialized.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: u32,
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub score: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<Hit>,
    /// Number of entries that matched, before the top-K cut.
    pub total: usize,
    /// True when `total` exceeded the requested limit.
    pub truncated: bool,
    /// Number of live entries considered.
    pub scanned: usize,
    pub elapsed: Duration,
}

/// Maps a byte to one of the 32 single-byte signature bits. Case is folded for
/// ASCII letters so the filter stays valid for case-insensitive matching.
const fn build_bit_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut b = 0usize;
    while b < 256 {
        let c = (b as u8) | 0x20;
        t[b] = match c {
            b'a'..=b'z' => c - b'a',
            b'0'..=b'9' => 26 + (c - b'0') % 3,
            b'.' => 29,
            b'_' | b'-' | b'/' | b' ' => 30,
            _ => 31,
        };
        b += 1;
    }
    t
}

static BIT_TABLE: [u8; 256] = build_bit_table();

#[inline]
fn pair_key(a: u8, b: u8) -> u32 {
    (((a | 0x20) as u32) << 8) | ((b | 0x20) as u32)
}

#[inline]
fn bigram_bit(a: u8, b: u8) -> u32 {
    32 + (pair_key(a, b).wrapping_mul(0x9E37_79B1) >> 27)
}

/// 64-bit bloom filter over a name: the low 32 bits mark which bytes occur, the
/// high 32 bits which adjacent byte pairs occur.
///
/// Any substring's signature is a subset of the name's, so a candidate can only
/// contain a query term if `sig & term_sig == term_sig`. The bigram half is what
/// makes short common terms cheap: `ss` rejects most names without ever reading
/// them, where a byte-only filter would let every name containing an `s`
/// through and pay a cache miss for each.
#[inline]
pub fn signature(bytes: &[u8]) -> u64 {
    let mut sig = 0u64;
    let mut prev = 0u8;
    for (i, &b) in bytes.iter().enumerate() {
        sig |= 1u64 << BIT_TABLE[b as usize];
        if i > 0 {
            sig |= 1u64 << bigram_bit(prev, b);
        }
        prev = b;
    }
    sig
}

/// Second, independent bigram bloom filter.
///
/// Names average ~21 bytes, which nearly saturates 32 bigram buckets on its own;
/// a second set of buckets with a different hash roughly squares the rejection
/// rate for two- and three-character terms, which is where the scan would
/// otherwise spend its time reading names it is about to discard.
#[inline]
pub fn signature2(bytes: &[u8]) -> u32 {
    let mut sig = 0u32;
    let mut prev = 0u8;
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            sig |= 1u32 << (pair_key(prev, b).wrapping_mul(0x85EB_CA6B) >> 27);
        }
        prev = b;
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_case_insensitive() {
        assert_eq!(signature(b"Cargo.TOML"), signature(b"cargo.toml"));
    }

    #[test]
    fn signature_superset_property_holds_for_substrings() {
        let name = signature(b"report_2024_final.pdf");
        for term in [&b"pdf"[..], b"2024", b"report", b"_final."] {
            let t = signature(term);
            assert_eq!(name & t, t, "term {:?} must pass the prefilter", term);
        }
    }

    #[test]
    fn signature_rejects_absent_bytes() {
        let name = signature(b"abc");
        let t = signature(b"z");
        assert_ne!(name & t, t);
    }

    #[test]
    fn bigrams_reject_reordered_bytes() {
        let name = signature(b"song list");
        let t = signature(b"ss");
        assert_ne!(name & t, t, "no adjacent ss in the name");
        assert_eq!(name & signature(b"s"), signature(b"s"));
    }

    #[test]
    fn substring_property_holds_for_random_names() {
        let alphabet = b"abcdefgHIJ._-019";
        let mut state = 0x12345678u32;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for _ in 0..2000 {
            let len = 4 + (rand() % 24) as usize;
            let name: Vec<u8> = (0..len)
                .map(|_| alphabet[(rand() % alphabet.len() as u32) as usize])
                .collect();
            let a = (rand() as usize) % len;
            let b = a + 1 + (rand() as usize) % (len - a);
            let sub = &name[a..b];
            let (ns, ss) = (signature(&name), signature(sub));
            assert_eq!(ns & ss, ss, "{:?} in {:?}", sub, name);
            let (n2, s2) = (signature2(&name), signature2(sub));
            assert_eq!(n2 & s2, s2, "{:?} in {:?}", sub, name);
        }
    }
}
