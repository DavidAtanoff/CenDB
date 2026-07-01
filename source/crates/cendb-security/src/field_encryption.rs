//! Field-level (per-column) encryption.
//!
//! ## Overview
//!
//! The crate's TDE module encrypts entire pages with a single key. That is
//! efficient for bulk I/O but provides no granularity: a single compromised
//! page key exposes every column on the page.
//!
//! Field-level encryption complements TDE by allowing different columns to
//! be encrypted under different keys. This is the model used by:
//!
//!   * **Microsoft SQL Server Always Encrypted** — column master keys live
//!     in an external key store; column encryption keys are derived per
//!     column and stored encrypted in the database catalog.
//!   * **MongoDB Client-Side Field-Level Encryption (CSFLE)** — every
//!     sensitive field is encrypted on the client before being sent over
//!     the wire.
//!   * **AWS DynamoDB Encryption Client** — per-attribute encryption with
//!     per-attribute material.
//!
//! The threat model is "defense in depth": if a DBA with read access to a
//! production snapshot does not hold the per-column keys, the protected
//! columns remain ciphertext. (TDE alone protects against disk theft but
//! not against someone with the running page key.)
//!
//! ## Cipher
//!
//! **XChaCha20-Poly1305**, the same AEAD used by `tde::TdeCipher`. We
//! reuse it deliberately:
//!
//!   1. Constant-time in software — no AES-NI dependency, no timing leaks.
//!   2. 24-byte nonce — random generation is collision-safe across
//!      billions of column values without a counter (a database can write
//!      far more field values than pages, so the larger nonce space is
//!      important here).
//!   3. AEAD — every ciphertext carries a Poly1305 tag, so any tampering
//!      (including truncation, byte-flips, or nonce reuse by an attacker
//!      who can write the file) is detected on decrypt.
//!
//! ## Wire format
//!
//! Each encrypted field is laid out as:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ nonce:  24 bytes  (random XChaCha20 nonce)              │
//! │ ciphertext + tag:  N + 16 bytes                        │
//! │   (Poly1305 tag is the last 16 bytes)                  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Identical to TDE's per-page layout, so the same parse / format code
//! could be shared. We keep a private copy here so that field encryption
//! stays decoupled from the page-level TDE module.
//!
//! ## Pass-through behaviour
//!
//! Columns that are NOT in the [`FieldEncryptionConfig`] mapping are
//! returned as plaintext byte-for-byte. This lets callers mix encrypted
//! and cleartext columns in a single row without special-casing.

use cendb_core::{CenError, CenResult, CenStatus};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::Rng;
use std::collections::HashMap;

// ============================================================================
// Configuration.
// ============================================================================

/// Per-column encryption key mapping.
///
/// Each entry maps a column name to a 32-byte XChaCha20-Poly1305 key.
/// Columns not present in the map are passed through as plaintext by
/// [`FieldEncryptor`].
///
/// The map is cheap to clone (32 bytes per entry) so callers can hold a
/// copy per reader/writer without contending on a shared reference.
#[derive(Clone, Debug, Default)]
pub struct FieldEncryptionConfig {
    keys: HashMap<String, [u8; 32]>,
}

impl FieldEncryptionConfig {
    /// Create an empty config (no columns encrypted).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an encryption key for a column. Replaces any prior key
    /// for the same column name.
    pub fn set_column_key(&mut self, column: impl Into<String>, key: [u8; 32]) {
        self.keys.insert(column.into(), key);
    }

    /// Look up the key for a column, if any.
    pub fn key_for(&self, column: &str) -> Option<&[u8; 32]> {
        self.keys.get(column)
    }

    /// Whether any columns are configured.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Number of columns configured.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Iterate over `(column_name, key)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &[u8; 32])> {
        self.keys.iter()
    }

    /// Generate a fresh random 32-byte key suitable for XChaCha20-Poly1305.
    pub fn generate_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        key
    }
}

// ============================================================================
// Encryptor.
// ============================================================================

/// Field-level encryptor.
///
/// Holds an immutable [`FieldEncryptionConfig`] and provides
/// column-scoped encryption / decryption plus row-level helpers that
/// preserve plaintext for un-configured columns.
pub struct FieldEncryptor {
    config: FieldEncryptionConfig,
}

impl FieldEncryptor {
    /// Build an encryptor from a config.
    pub fn new(config: FieldEncryptionConfig) -> Self {
        Self { config }
    }

    /// Borrow the underlying config.
    pub fn config(&self) -> &FieldEncryptionConfig {
        &self.config
    }

    /// Encrypt a single field. If the column is not configured, the
    /// plaintext is returned untouched (pass-through). Otherwise the
    /// plaintext is encrypted with XChaCha20-Poly1305 under the column's
    /// key and a fresh random nonce, returning `nonce || ciphertext+tag`.
    pub fn encrypt_field(&self, column: &str, plaintext: &[u8]) -> CenResult<Vec<u8>> {
        let key = match self.config.key_for(column) {
            Some(k) => k,
            None => return Ok(plaintext.to_vec()),
        };
        let cipher = XChaCha20Poly1305::new(key.into());
        let mut nonce_bytes = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::try_from(&nonce_bytes[..])
            .map_err(|e| CenError::internal(format!("field_encryption: bad nonce length: {}", e)))?;
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| CenError::internal(format!("field_encryption: encrypt failed: {}", e)))?;
        let mut out = Vec::with_capacity(24 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a single field. If the column is not configured, the
    /// input is returned untouched. Otherwise the input is parsed as
    /// `nonce || ciphertext+tag` and decrypted under the column's key.
    pub fn decrypt_field(&self, column: &str, ciphertext: &[u8]) -> CenResult<Vec<u8>> {
        let key = match self.config.key_for(column) {
            Some(k) => k,
            None => return Ok(ciphertext.to_vec()),
        };
        if ciphertext.len() < 24 + 16 {
            return Err(CenError::new(
                CenStatus::ErrCorrupt,
                format!(
                    "field_encryption: ciphertext for column {:?} too short ({} < 40)",
                    column,
                    ciphertext.len()
                ),
            ));
        }
        let cipher = XChaCha20Poly1305::new(key.into());
        let nonce = XNonce::try_from(&ciphertext[..24]).map_err(|e| {
            CenError::internal(format!("field_encryption: bad nonce slice: {}", e))
        })?;
        cipher
            .decrypt(&nonce, &ciphertext[24..])
            .map_err(|_| {
                CenError::new(
                    CenStatus::ErrCorrupt,
                    format!(
                        "field_encryption: authentication failed for column {:?} (tampered ciphertext or wrong key)",
                        column
                    ),
                )
            })
    }

    /// Encrypt every configured column in a row. Columns not in the
    /// config are passed through untouched. Columns missing from the
    /// input row are simply not emitted (no synthetic empty values).
    pub fn encrypt_row(
        &self,
        row: &HashMap<String, Vec<u8>>,
    ) -> CenResult<HashMap<String, Vec<u8>>> {
        let mut out = HashMap::with_capacity(row.len());
        for (col, val) in row {
            out.insert(col.clone(), self.encrypt_field(col, val)?);
        }
        Ok(out)
    }

    /// Decrypt every configured column in a row. Columns not in the
    /// config are passed through untouched.
    pub fn decrypt_row(
        &self,
        row: &HashMap<String, Vec<u8>>,
    ) -> CenResult<HashMap<String, Vec<u8>>> {
        let mut out = HashMap::with_capacity(row.len());
        for (col, val) in row {
            out.insert(col.clone(), self.decrypt_field(col, val)?);
        }
        Ok(out)
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(cols: &[(&str, [u8; 32])]) -> FieldEncryptionConfig {
        let mut c = FieldEncryptionConfig::new();
        for (name, key) in cols {
            c.set_column_key(*name, *key);
        }
        c
    }

    #[test]
    fn round_trip_single_column() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));
        let pt = b"123-45-6789";
        let ct = enc.encrypt_field("ssn", pt).unwrap();
        assert_ne!(&ct[..], pt); // actually encrypted
        assert!(ct.len() >= 24 + 16 + pt.len());
        let back = enc.decrypt_field("ssn", &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn column_isolation_keys_dont_cross() {
        // Different columns have different keys; the key for column A
        // cannot decrypt ciphertext produced for column B.
        let key_a = FieldEncryptionConfig::generate_key();
        let key_b = FieldEncryptionConfig::generate_key();
        assert_ne!(key_a, key_b);

        let enc_a = FieldEncryptor::new(make_config(&[("a", key_a)]));
        let enc_b = FieldEncryptor::new(make_config(&[("b", key_b)]));
        let enc_both =
            FieldEncryptor::new(make_config(&[("a", key_a), ("b", key_b)]));

        let pt = b"top secret";
        let ct_a = enc_both.encrypt_field("a", pt).unwrap();

        // Trying to decrypt column A's ciphertext as if it were column B
        // (i.e. under key B) must fail authentication.
        let wrong = enc_b.decrypt_field("b", &ct_a);
        assert!(wrong.is_err(), "decrypting under the wrong key should fail");
        // And the encryptor-with-both-keys should fail to decrypt it
        // when asked to treat it as column B.
        let also_wrong = enc_both.decrypt_field("b", &ct_a);
        assert!(also_wrong.is_err());

        // But decrypting it as column A under the same config works.
        let ok = enc_both.decrypt_field("a", &ct_a).unwrap();
        assert_eq!(ok, pt);

        // And the single-column-A encryptor can decrypt it too.
        let ok2 = enc_a.decrypt_field("a", &ct_a).unwrap();
        assert_eq!(ok2, pt);
    }

    #[test]
    fn missing_column_passthrough_encrypt() {
        // Columns not in the config pass through as plaintext.
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));
        let pt = b"public data";
        let ct = enc.encrypt_field("notes", pt).unwrap();
        assert_eq!(ct, pt); // byte-identical
    }

    #[test]
    fn missing_column_passthrough_decrypt() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));
        let pt = b"public data";
        let back = enc.decrypt_field("notes", pt).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("x", key)]));
        let ct = enc.encrypt_field("x", b"").unwrap();
        // 24-byte nonce + 16-byte tag = 40 bytes minimum.
        assert_eq!(ct.len(), 40);
        let back = enc.decrypt_field("x", &ct).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn empty_config_is_passthrough_for_everything() {
        let enc = FieldEncryptor::new(FieldEncryptionConfig::new());
        let pt = b"anything goes";
        let ct = enc.encrypt_field("any_column", pt).unwrap();
        assert_eq!(ct, pt);
        let back = enc.decrypt_field("any_column", &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn row_round_trip_preserves_unconfigured_columns() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key), ("email", key)]));

        let mut row = HashMap::new();
        row.insert("ssn".to_string(), b"123-45-6789".to_vec());
        row.insert("email".to_string(), b"alice@example.com".to_vec());
        row.insert("name".to_string(), b"Alice".to_vec()); // not configured
        row.insert("age".to_string(), b"30".to_vec()); // not configured

        let enc_row = enc.encrypt_row(&row).unwrap();

        // Configured columns are actually encrypted.
        assert_ne!(enc_row["ssn"], row["ssn"]);
        assert_ne!(enc_row["email"], row["email"]);
        assert!(enc_row["ssn"].len() > row["ssn"].len() + 24);
        assert!(enc_row["email"].len() > row["email"].len() + 24);

        // Un-configured columns are byte-identical.
        assert_eq!(enc_row["name"], row["name"]);
        assert_eq!(enc_row["age"], row["age"]);

        // Round trip recovers the original row exactly.
        let dec_row = enc.decrypt_row(&enc_row).unwrap();
        assert_eq!(dec_row, row);
    }

    #[test]
    fn row_encrypt_only_configured_columns() {
        // If only "ssn" is configured, "name" should pass through
        // unchanged even on encrypt.
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));

        let mut row = HashMap::new();
        row.insert("ssn".to_string(), b"123-45-6789".to_vec());
        row.insert("name".to_string(), b"Alice".to_vec());

        let enc_row = enc.encrypt_row(&row).unwrap();
        assert_ne!(enc_row["ssn"], row["ssn"]);
        assert_eq!(enc_row["name"], row["name"]);
    }

    #[test]
    fn tampered_ciphertext_fails_auth() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));
        let mut ct = enc.encrypt_field("ssn", b"secret").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let r = enc.decrypt_field("ssn", &ct);
        assert!(r.is_err());
    }

    #[test]
    fn truncated_ciphertext_returns_error() {
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("ssn", key)]));
        // 10 bytes is shorter than the 40-byte minimum.
        let r = enc.decrypt_field("ssn", &[0u8; 10]);
        assert!(r.is_err());
    }

    #[test]
    fn distinct_nonces_per_encryption() {
        // Same plaintext encrypted twice must produce different nonces
        // (random generation), and both must decrypt back to the plaintext.
        let key = FieldEncryptionConfig::generate_key();
        let enc = FieldEncryptor::new(make_config(&[("x", key)]));
        let pt = b"same value";
        let ct1 = enc.encrypt_field("x", pt).unwrap();
        let ct2 = enc.encrypt_field("x", pt).unwrap();
        assert_ne!(&ct1[..24], &ct2[..24]);
        assert_ne!(&ct1[24..], &ct2[24..]);
        assert_eq!(enc.decrypt_field("x", &ct1).unwrap(), pt);
        assert_eq!(enc.decrypt_field("x", &ct2).unwrap(), pt);
    }

    #[test]
    fn config_is_empty_and_len_helpers() {
        let mut c = FieldEncryptionConfig::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        c.set_column_key("a", [0u8; 32]);
        assert!(!c.is_empty());
        assert_eq!(c.len(), 1);
        c.set_column_key("b", [1u8; 32]);
        assert_eq!(c.len(), 2);
        // Re-setting an existing key replaces, doesn't add.
        c.set_column_key("a", [2u8; 32]);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn generate_key_is_random() {
        let k1 = FieldEncryptionConfig::generate_key();
        let k2 = FieldEncryptionConfig::generate_key();
        assert_ne!(k1, k2);
    }
}
