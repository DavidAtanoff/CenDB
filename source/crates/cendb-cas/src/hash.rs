//! BLAKE3 hashing for content-addressable storage.

use std::fmt;

/// A 32-byte BLAKE3 digest. This is the content address used by the
/// [`BlobStore`](crate::BlobStore).
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Compute the BLAKE3 hash of `data`.
    pub fn of(data: &[u8]) -> Self {
        let h = blake3::hash(data);
        Self(*h.as_bytes())
    }

    /// The raw 32 bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hexadecimal representation (64 chars).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// Parse a 64-char hex string into a `Hash`.
    pub fn from_hex(s: &str) -> Result<Self, &'static str> {
        if s.len() != 64 {
            return Err("hash must be 64 hex chars");
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            let hi = hex_val(s.as_bytes()[i * 2])?;
            let lo = hex_val(s.as_bytes()[i * 2 + 1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }

    /// A slice view suitable for use as an ART key.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

fn hex_val(c: u8) -> Result<u8, &'static str> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err("invalid hex char"),
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_empty() {
        let h = Hash::of(b"");
        // BLAKE3 of empty input is a known constant.
        assert_eq!(
            h.to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hash_of_data() {
        let h1 = Hash::of(b"hello world");
        let h2 = Hash::of(b"hello world");
        assert_eq!(h1, h2);

        let h3 = Hash::of(b"hello World");
        assert_ne!(h1, h3);
    }

    #[test]
    fn hex_roundtrip() {
        let h = Hash::of(b"test data");
        let hex = h.to_hex();
        let h2 = Hash::from_hex(&hex).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn hash_is_32_bytes() {
        let h = Hash::of(b"anything");
        assert_eq!(h.as_bytes().len(), 32);
    }

    #[test]
    fn large_data_hashes_fast() {
        // 8 MB of data (simulating a 4K image).
        let data = vec![0xABu8; 8 * 1024 * 1024];
        let start = std::time::Instant::now();
        let h = Hash::of(&data);
        let elapsed = start.elapsed();
        println!(
            "[large_data_hashes_fast] 8MB hashed in {:?} ({:.0} MB/s)",
            elapsed,
            8.0 / elapsed.as_secs_f64()
        );
        assert_eq!(h.as_bytes().len(), 32);
    }
}
