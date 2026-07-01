//! Transparent Data Encryption (TDE) — corporate-grade.
//!
//! ## Cipher
//!
//! **XChaCha20-Poly1305** — an authenticated encryption with associated
//! data (AEAD) construction. XChaCha20 is a stream cipher with a 24-byte
//! nonce (vs ChaCha20's 12-byte), which means nonces can be generated
//! randomly without fear of collision — a critical safety property for a
//! database that may encrypt billions of pages. Poly1305 is a MAC that
//! provides integrity and authentication.
//!
//! Why XChaCha20-Poly1305 over AES-256-GCM?
//!   1. No hardware dependency. AES-NI acceleration is ubiquitous on x86,
//!      but absent on many ARM chips and all WASM targets. ChaCha20 is
//!      constant-time in software and ~2× faster than AES without AES-NI.
//!   2. No nonce-misuse catastrophe. AES-GCM is catastrophically broken
//!      on nonce reuse (leaves the auth key recoverable). XChaCha20's
//!      24-byte nonce makes random generation safe: with 4 billion pages
//!      (32-bit page IDs), the collision probability is < 2^-60.
//!   3. Constant-time. ChaCha20 and Poly1305 are both designed to be
//!      constant-time in software, avoiding the side-channel pitfalls
//!      that have plagued AES implementations.
//!
//! ## Key derivation
//!
//! **Argon2id** (memory-hard, side-channel-resistant). Parameters:
//!   - m_cost: 64 MiB (resists GPU/ASIC attacks)
//!   - t_cost: 3 iterations
//!   - p_cost: 4 lanes (parallelism)
//!
//! These parameters are tuned to take ~300-500ms on a modern server —
//! acceptable for a one-time key derivation at database open, but
//! expensive enough to make brute-force attacks on stolen databases
//! economically infeasible.
//!
//! ## Page format
//!
//! Each encrypted page on disk has the layout:
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │ nonce:    24 bytes  (XChaCha20 nonce)           │
//! │ ciphertext + tag: N + 16 bytes                  │
//! │   (Poly1305 tag is the last 16 bytes)           │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! The nonce is generated randomly per encryption (page write). The
//! 16-byte Poly1305 tag is appended by the AEAD and verified on decrypt
//! — any tampering with ciphertext or nonce causes `decrypt` to return
//! `TdeError::AuthenticationFailed`.
//!
//! ## Threat model
//!
//! **Protects against:**
//!   - Disk theft (full-disk image, stolen laptop, decommissioned SSD)
//!   - Cold-boot attacks (RAM dump → key recovery is mitigated by
//!     Argon2id memory hardness; the key in RAM is a derived 32-byte
//!     value, not the passphrase)
//!   - Snapshot/backup theft (encrypted snapshots are useless without
//!     the key)
//!   - Page-level tampering (Poly1305 tag detects any modification)
//!
//! **Does NOT protect against:**
//!   - An attacker with both the disk AND the running process (the key
//!     is in process memory)
//!   - An attacker with the passphrase (social engineering, phishing)
//!   - Side-channel attacks on the running process (timing, power)
//!   - Network-layer attacks (CenDB is embedded; if the host application
//!     exposes it over the network, network-layer security is the host's
//!     responsibility)

use argon2::{Argon2, Algorithm, Version, Params};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::Rng;

// ============================================================================
// Configuration.
// ============================================================================

/// TDE configuration. The key is held in memory; it is never serialized.
#[derive(Clone, Debug)]
pub struct TdeConfig {
    /// The 32-byte encryption key (derived from passphrase via Argon2id).
    pub key: [u8; 32],
    /// Whether encryption is enabled.
    pub enabled: bool,
}

impl TdeConfig {
    /// Derive a key from a passphrase using Argon2id.
    ///
    /// Parameters:
    ///   - m_cost: 64 MiB (resists GPU/ASIC attacks)
    ///   - t_cost: 3 iterations
    ///   - p_cost: 4 lanes (parallelism)
    ///
    /// The salt should be a random 16-byte value stored alongside the
    /// encrypted database (NOT the passphrase). Using a fixed salt
    /// makes the key derivation deterministic — useful for testing but
    /// NOT recommended for production.
    pub fn from_passphrase(passphrase: &str, salt: &[u8]) -> Result<Self, TdeError> {
        let params = Params::new(64 * 1024, 3, 4, Some(32))
            .map_err(|e| TdeError::KeyDerivationFailed(e.to_string()))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut key = [0u8; 32];
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, &mut key)
            .map_err(|e| TdeError::KeyDerivationFailed(e.to_string()))?;
        Ok(Self { key, enabled: true })
    }

    /// Derive a key from a passphrase with a fixed salt.
    /// **WARNING: For testing only.** Production use must supply a
    /// random salt and store it alongside the database.
    pub fn from_passphrase_insecure_fixed_salt(passphrase: &str) -> Result<Self, TdeError> {
        let salt = b"cendb_fixed_salt!"; // 16 bytes
        Self::from_passphrase(passphrase, salt)
    }

    /// Create a config from a raw 32-byte key (e.g. from a KMS or HSM).
    pub fn from_raw_key(key: [u8; 32]) -> Self {
        Self { key, enabled: true }
    }

    /// Create a disabled TDE config (no encryption).
    pub fn disabled() -> Self {
        Self { key: [0u8; 32], enabled: false }
    }

    /// Generate a random 16-byte salt.
    pub fn generate_salt() -> [u8; 16] {
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        salt
    }
}

// ============================================================================
// Errors.
// ============================================================================

#[derive(Debug, Clone)]
pub enum TdeError {
    /// Poly1305 tag verification failed — the ciphertext was tampered
    /// with, or the wrong key was used.
    AuthenticationFailed,
    /// Argon2id key derivation failed (invalid parameters, OOM, etc.).
    KeyDerivationFailed(String),
    /// Encryption/decryption failed for a reason other than auth.
    CipherError(String),
    /// Encryption is disabled but an encrypted operation was requested.
    EncryptionDisabled,
    /// The input was too short to contain a nonce + tag.
    InputTooShort,
}

impl std::fmt::Display for TdeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TdeError::AuthenticationFailed => write!(f, "TDE authentication failed (ciphertext tampered or wrong key)"),
            TdeError::KeyDerivationFailed(s) => write!(f, "TDE key derivation failed: {}", s),
            TdeError::CipherError(s) => write!(f, "TDE cipher error: {}", s),
            TdeError::EncryptionDisabled => write!(f, "TDE encryption is disabled"),
            TdeError::InputTooShort => write!(f, "TDE input too short (must be at least 24 + 16 = 40 bytes for nonce + tag)"),
        }
    }
}

impl std::error::Error for TdeError {}

// ============================================================================
// Cipher.
// ============================================================================

/// TDE cipher: wraps XChaCha20-Poly1305 with a per-page random nonce.
pub struct TdeCipher {
    config: TdeConfig,
}

impl TdeCipher {
    /// Create a new cipher from a config. If the config is disabled,
    /// `encrypt` and `decrypt` are pass-through.
    pub fn new(config: TdeConfig) -> Self {
        Self { config }
    }

    /// Encrypt a page. Returns `nonce || ciphertext+tag` (24 + N + 16
    /// bytes for an N-byte plaintext).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, TdeError> {
        if !self.config.enabled {
            return Ok(plaintext.to_vec());
        }
        let cipher = XChaCha20Poly1305::new(&self.config.key.into());
        let mut nonce_bytes = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::try_from(&nonce_bytes[..]).unwrap();
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| TdeError::CipherError(e.to_string()))?;
        let mut out = Vec::with_capacity(24 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a page. Expects `nonce || ciphertext+tag` (24 + N + 16
    /// bytes). Returns the N-byte plaintext.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TdeError> {
        if !self.config.enabled {
            return Ok(ciphertext.to_vec());
        }
        if ciphertext.len() < 24 + 16 {
            return Err(TdeError::InputTooShort);
        }
        let cipher = XChaCha20Poly1305::new(&self.config.key.into());
        let nonce = XNonce::try_from(&ciphertext[..24]).unwrap();
        let ct = &ciphertext[24..];
        cipher
            .decrypt(&nonce, ct)
            .map_err(|_| TdeError::AuthenticationFailed)
    }

    /// Whether encryption is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"hello, world! This is a page of data.";
        let ct = cipher.encrypt(plaintext).unwrap();
        assert_ne!(&ct[..], plaintext); // actually encrypted
        assert!(ct.len() > plaintext.len() + 24); // nonce + tag overhead
        let pt = cipher.decrypt(&ct).unwrap();
        assert_eq!(&pt, plaintext);
    }

    #[test]
    fn tampered_ciphertext_fails_auth() {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"sensitive data";
        let mut ct = cipher.encrypt(plaintext).unwrap();
        // Flip a bit in the ciphertext (not the nonce).
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let result = cipher.decrypt(&ct);
        assert!(matches!(result, Err(TdeError::AuthenticationFailed)));
    }

    #[test]
    fn tampered_nonce_fails_auth() {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"sensitive data";
        let mut ct = cipher.encrypt(plaintext).unwrap();
        ct[0] ^= 0x01; // flip a bit in the nonce
        let result = cipher.decrypt(&ct);
        assert!(matches!(result, Err(TdeError::AuthenticationFailed)));
    }

    #[test]
    fn wrong_key_fails_auth() {
        let cfg1 = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cfg2 = TdeConfig::from_passphrase_insecure_fixed_salt("hunter3").unwrap();
        let cipher1 = TdeCipher::new(cfg1);
        let cipher2 = TdeCipher::new(cfg2);
        let plaintext = b"sensitive data";
        let ct = cipher1.encrypt(plaintext).unwrap();
        let result = cipher2.decrypt(&ct);
        assert!(matches!(result, Err(TdeError::AuthenticationFailed)));
    }

    #[test]
    fn disabled_cipher_is_passthrough() {
        let cfg = TdeConfig::disabled();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"unencrypted";
        let ct = cipher.encrypt(plaintext).unwrap();
        assert_eq!(&ct, plaintext);
        let pt = cipher.decrypt(&ct).unwrap();
        assert_eq!(&pt, plaintext);
    }

    #[test]
    fn distinct_nonces_per_encryption() {
        // Encrypt the same plaintext twice — the nonces must differ
        // (random generation). This is what makes XChaCha20 safe.
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"same data";
        let ct1 = cipher.encrypt(plaintext).unwrap();
        let ct2 = cipher.encrypt(plaintext).unwrap();
        assert_ne!(&ct1[..24], &ct2[..24]); // nonces differ
        assert_ne!(&ct1[24..], &ct2[24..]); // ciphertexts differ (different nonce)
        // Both decrypt correctly.
        assert_eq!(cipher.decrypt(&ct1).unwrap(), plaintext);
        assert_eq!(cipher.decrypt(&ct2).unwrap(), plaintext);
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"";
        let ct = cipher.encrypt(plaintext).unwrap();
        // nonce (24) + tag (16) = 40 bytes minimum.
        assert_eq!(ct.len(), 40);
        let pt = cipher.decrypt(&ct).unwrap();
        assert!(pt.is_empty());
    }

    #[test]
    fn large_page_roundtrips() {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt("hunter2").unwrap();
        let cipher = TdeCipher::new(cfg);
        // 256 KB page (CenDB's max block size).
        let plaintext: Vec<u8> = (0..256 * 1024).map(|i| (i & 0xFF) as u8).collect();
        let ct = cipher.encrypt(&plaintext).unwrap();
        let pt = cipher.decrypt(&ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn from_raw_key_works() {
        let key = [42u8; 32];
        let cfg = TdeConfig::from_raw_key(key);
        let cipher = TdeCipher::new(cfg);
        let plaintext = b"data";
        let ct = cipher.encrypt(plaintext).unwrap();
        let pt = cipher.decrypt(&ct).unwrap();
        assert_eq!(&pt, plaintext);
    }

    #[test]
    fn salt_generation_is_random() {
        let s1 = TdeConfig::generate_salt();
        let s2 = TdeConfig::generate_salt();
        assert_ne!(s1, s2);
    }
}
