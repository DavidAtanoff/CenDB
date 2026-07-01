//! Transparent Data Encryption (TDE).
//!
//! Encrypts data pages transparently using a XOR-keystream cipher
//! seeded from BLAKE3. This is a prototype-grade cipher suitable for
//! demonstrating the TDE architecture; production use should substitute
//! AES-256-GCM or ChaCha20-Poly1305.

use blake3;

/// TDE configuration.
#[derive(Clone, Debug)]
pub struct TdeConfig {
    /// The 32-byte encryption key.
    pub key: [u8; 32],
    /// Whether encryption is enabled.
    pub enabled: bool,
}

impl TdeConfig {
    /// Create a new TDE config from a passphrase.
    pub fn from_passphrase(passphrase: &str) -> Self {
        let key = blake3::hash(passphrase.as_bytes());
        Self {
            key: *key.as_bytes(),
            enabled: true,
        }
    }

    /// Create a disabled TDE config (no encryption).
    pub fn disabled() -> Self {
        Self {
            key: [0u8; 32],
            enabled: false,
        }
    }
}

/// TDE errors.
#[derive(Debug, Clone)]
pub enum TdeError {
    /// Encryption is not enabled.
    NotEnabled,
    /// Data length mismatch.
    LengthMismatch,
}

impl std::fmt::Display for TdeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for TdeError {}

/// The TDE cipher: encrypts/decrypts page data using a BLAKE3-derived
/// keystream XOR'd with the plaintext.
pub struct TdeCipher {
    config: TdeConfig,
}

impl TdeCipher {
    pub fn new(config: TdeConfig) -> Self {
        Self { config }
    }

    /// Encrypt a page. The `page_id` is used as a nonce to ensure that
    /// identical plaintext on different pages produces different ciphertext.
    pub fn encrypt(&self, page_id: u64, plaintext: &[u8]) -> Result<Vec<u8>, TdeError> {
        if !self.config.enabled {
            return Ok(plaintext.to_vec());
        }

        // Derive a page-specific keystream from (key, page_id) using
        // BLAKE3's XOF (extendable output function).
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.config.key);
        hasher.update(&page_id.to_le_bytes());
        let mut keystream = Vec::with_capacity(plaintext.len());

        // Generate enough keystream bytes by repeatedly hashing with
        // a counter.
        let mut counter = 0u64;
        while keystream.len() < plaintext.len() {
            let mut h = hasher.clone();
            h.update(&counter.to_le_bytes());
            let out = h.finalize();
            keystream.extend_from_slice(out.as_bytes());
            counter += 1;
        }

        let ciphertext: Vec<u8> = plaintext
            .iter()
            .zip(keystream.iter())
            .map(|(p, k)| p ^ k)
            .collect();

        Ok(ciphertext)
    }

    /// Decrypt a page. XOR is symmetric, so decryption is identical to
    /// encryption.
    pub fn decrypt(&self, page_id: u64, ciphertext: &[u8]) -> Result<Vec<u8>, TdeError> {
        // XOR keystream is symmetric.
        self.encrypt(page_id, ciphertext)
    }

    /// Encrypt in-place (for buffer-pool integration).
    pub fn encrypt_in_place(&self, page_id: u64, data: &mut [u8]) -> Result<(), TdeError> {
        if !self.config.enabled {
            return Ok(());
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.config.key);
        hasher.update(&page_id.to_le_bytes());

        let mut keystream = Vec::with_capacity(data.len());
        let mut counter = 0u64;
        while keystream.len() < data.len() {
            let mut h = hasher.clone();
            h.update(&counter.to_le_bytes());
            let out = h.finalize();
            keystream.extend_from_slice(out.as_bytes());
            counter += 1;
        }

        for (i, byte) in data.iter_mut().enumerate() {
            *byte ^= keystream[i];
        }
        Ok(())
    }

    /// Whether encryption is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let cipher = TdeCipher::new(TdeConfig::from_passphrase("my secret key"));
        let plaintext = b"Hello, encrypted world! This is sensitive data.";
        let encrypted = cipher.encrypt(42, plaintext).unwrap();
        assert_ne!(encrypted.as_slice(), plaintext);
        let decrypted = cipher.decrypt(42, &encrypted).unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn different_pages_produce_different_ciphertext() {
        let cipher = TdeCipher::new(TdeConfig::from_passphrase("key"));
        let plaintext = b"same data on both pages";
        let enc1 = cipher.encrypt(1, plaintext).unwrap();
        let enc2 = cipher.encrypt(2, plaintext).unwrap();
        assert_ne!(enc1, enc2, "different page IDs should produce different ciphertext");
    }

    #[test]
    fn disabled_cipher_is_pass_through() {
        let cipher = TdeCipher::new(TdeConfig::disabled());
        let data = b"unencrypted";
        let encrypted = cipher.encrypt(0, data).unwrap();
        assert_eq!(encrypted.as_slice(), data);
    }

    #[test]
    fn encrypt_in_place_works() {
        let cipher = TdeCipher::new(TdeConfig::from_passphrase("key"));
        let original = b"original data here".to_vec();
        let mut data = original.clone();
        cipher.encrypt_in_place(99, &mut data).unwrap();
        assert_ne!(data, original);
        cipher.encrypt_in_place(99, &mut data).unwrap();
        assert_eq!(data, original);
    }
}
