//! Append-only byte arena for file and directory names.

/// Names longer than this are truncated on insert; `u16` lengths keep the
/// columnar index small and no real filesystem allows longer components anyway
/// (`NAME_MAX` is 255 on Linux).
pub const MAX_NAME_LEN: usize = u16::MAX as usize;

#[derive(Debug, Default, Clone)]
pub struct Arena {
    buf: Vec<u8>,
}

impl Arena {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
        }
    }

    /// Appends `name` and returns its `(offset, len)` slot.
    pub fn push(&mut self, name: &[u8]) -> (u32, u16) {
        let name = if name.len() > MAX_NAME_LEN {
            &name[..MAX_NAME_LEN]
        } else {
            name
        };
        let off = self.buf.len();
        assert!(off <= u32::MAX as usize, "name arena exceeded 4 GiB");
        self.buf.extend_from_slice(name);
        (off as u32, name.len() as u16)
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }

    #[inline]
    pub fn get(&self, off: u32, len: u16) -> &[u8] {
        let start = off as usize;
        &self.buf[start..start + len as usize]
    }

    #[inline]
    pub fn get_str(&self, off: u32, len: u16) -> &str {
        // Names come from the OS as arbitrary bytes; callers that need text use
        // this lossy-free fast path and fall back to `String::from_utf8_lossy`.
        std::str::from_utf8(self.get(off, len)).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_get_roundtrip() {
        let mut a = Arena::new();
        let (o1, l1) = a.push(b"hello");
        let (o2, l2) = a.push(b"world!");
        assert_eq!(a.get(o1, l1), b"hello");
        assert_eq!(a.get(o2, l2), b"world!");
        assert_eq!(a.len(), 11);
    }

    #[test]
    fn empty_name_is_valid() {
        let mut a = Arena::new();
        let (o, l) = a.push(b"");
        assert_eq!(l, 0);
        assert_eq!(a.get(o, l), b"");
    }

    #[test]
    fn long_names_are_truncated() {
        let mut a = Arena::new();
        let long = vec![b'x'; MAX_NAME_LEN + 100];
        let (o, l) = a.push(&long);
        assert_eq!(l as usize, MAX_NAME_LEN);
        assert_eq!(a.get(o, l).len(), MAX_NAME_LEN);
    }

    #[test]
    fn get_str_handles_invalid_utf8() {
        let mut a = Arena::new();
        let (o, l) = a.push(&[0xff, 0xfe]);
        assert_eq!(a.get_str(o, l), "");
    }
}
