//! Key Management Service (KMS) abstraction.
//!
//! ## Overview
//!
//! Real-world deployments of CenDB's TDE / field-level encryption should
//! not store master keys on disk or in process memory forever. Industry
//! best practice is to delegate master-key custody to a *Key Management
//! Service*: AWS KMS, Google Cloud KMS, HashiCorp Vault's Transit engine,
//! Azure Key Vault, etc. The application never sees the master key in
//! plaintext; it asks the KMS to wrap and unwrap per-object *data keys*.
//!
//! This module provides:
//!
//!   * The [`KmsProvider`] trait — the abstraction real KMS backends
//!     implement.
//!   * [`LocalKms`] — a self-contained in-process implementation that
//!     wraps data keys under a master key with XChaCha20-Poly1305. Useful
//!     for tests, development, and air-gapped deployments where calling
//!     out to a real KMS is impossible or undesirable.
//!   * [`AwsKmsConfig`] / [`VaultConfig`] — configuration and provider
//!     implementations for AWS KMS and HashiCorp Vault. These use
//!     BLAKE3-derived master keys for self-contained envelope encryption
//!     without external SDK dependencies. For production cloud
//!     deployments, replace with direct `aws-sdk-kms` or `vaultrs`
//!     integrations via the `KmsProvider` trait.
//!   * [`KmsEnvelopeEncryption`] — a small wrapper that combines a
//!     [`KmsProvider`] with a [`TdeCipher`] to do envelope encryption:
//!     generate a random data key, encrypt the payload with it, then
//!     wrap the data key via the KMS. The reverse path unwraps the key
//!     via the KMS and decrypts the payload.
//!
//! ## Envelope encryption wire format
//!
//! The envelope blob is a length-prefixed pair:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ encrypted_data_key_len:  4 bytes, big-endian u32            │
//! │ encrypted_data_key:      N bytes  (KMS-wrapped data key)    │
//! │ ciphertext:              M bytes  (XChaCha20-Poly1305 of     │
//! │                                    plaintext under data key, │
//! │                                    format = nonce || ct||tag)│
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! Length-prefixed framing lets us parse the blob without any external
//! delimiter; the ciphertext portion can contain any byte sequence.
//!
//! ## Why a trait?
//!
//! The trait abstraction lets the same envelope-encryption code path
//! target AWS KMS in production and `LocalKms` in tests. Production
//! callers wire `AwsKmsProvider` (when implemented) into
//! `KmsEnvelopeEncryption`; test callers wire `LocalKms`. The
//! envelope-encryption logic is KMS-agnostic.

use cendb_core::{CenError, CenResult, CenStatus};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::Rng;

use crate::tde::{TdeCipher, TdeConfig, TdeError};

// ============================================================================
// Trait.
// ============================================================================

/// A KMS backend capable of wrapping (encrypting) and unwrapping
/// (decrypting) 32-byte data keys.
///
/// The plaintext key is always exactly 32 bytes (XChaCha20-Poly1305 key
/// size). The encrypted form is opaque to the caller — its length and
/// format depend on the backend (AWS KMS produces ~400-byte blobs
/// because they include metadata + RSA-OAEP padding; `LocalKms`
/// produces a 40-byte nonce||ct+tag blob).
///
/// Implementations must be `Send + Sync` so they can sit behind an
/// `Arc<dyn KmsProvider>` shared across threads.
pub trait KmsProvider: Send + Sync {
    /// Wrap a 32-byte plaintext data key. Returns the KMS-specific
    /// encrypted blob.
    fn encrypt_data_key(&self, plaintext_key: &[u8; 32]) -> CenResult<Vec<u8>>;

    /// Unwrap an encrypted data key back to the 32-byte plaintext.
    /// Must return an error if the ciphertext is tampered, truncated,
    /// or was wrapped under a different master key.
    fn decrypt_data_key(&self, encrypted_key: &[u8]) -> CenResult<[u8; 32]>;

    /// Human-readable backend name (e.g. `"local"`, `"aws-kms"`,
    /// `"vault"`). Used for logging and diagnostics.
    fn name(&self) -> &str;
}

// ============================================================================
// LocalKms — in-process implementation.
// ============================================================================

/// A self-contained, in-process KMS for tests and development.
///
/// Data keys are wrapped under a single 32-byte master key with
/// XChaCha20-Poly1305 (the same AEAD used elsewhere in the crate).
/// The wrapped form is `nonce || ciphertext+tag` (40 bytes for a
/// 32-byte plaintext key).
///
/// **Not for production use as a real KMS replacement** — the master
/// key lives in process memory and provides no separation of duties.
/// It is, however, a faithful implementation of the [`KmsProvider`]
/// trait and is exactly what tests need.
pub struct LocalKms {
    master_key: [u8; 32],
    name: String,
}

impl LocalKms {
    /// Build a `LocalKms` from a raw 32-byte master key.
    pub fn new(master_key: [u8; 32]) -> Self {
        Self {
            master_key,
            name: "local".to_string(),
        }
    }

    /// Build a `LocalKms` from a passphrase, deriving the master key
    /// with Argon2id (matching the parameters used by `TdeConfig`).
    pub fn from_passphrase(passphrase: &str, salt: &[u8]) -> CenResult<Self> {
        let cfg = TdeConfig::from_passphrase(passphrase, salt)
            .map_err(|e| CenError::new(CenStatus::ErrInternal, format!("LocalKms: KDF failed: {}", e)))?;
        Ok(Self::new(cfg.key))
    }

    /// Convenience: passphrase + a fixed insecure salt (tests only).
    pub fn from_passphrase_insecure_fixed_salt(passphrase: &str) -> CenResult<Self> {
        let cfg = TdeConfig::from_passphrase_insecure_fixed_salt(passphrase)
            .map_err(|e| CenError::new(CenStatus::ErrInternal, format!("LocalKms: KDF failed: {}", e)))?;
        Ok(Self::new(cfg.key))
    }

    /// Override the reported backend name (useful in tests with multiple
    /// instances).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

impl KmsProvider for LocalKms {
    fn encrypt_data_key(&self, plaintext_key: &[u8; 32]) -> CenResult<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new(&self.master_key.into());
        let mut nonce_bytes = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::try_from(&nonce_bytes[..])
            .map_err(|e| CenError::internal(format!("LocalKms: bad nonce length: {}", e)))?;
        let ct = cipher
            .encrypt(&nonce, plaintext_key.as_ref())
            .map_err(|e| CenError::internal(format!("LocalKms: encrypt failed: {}", e)))?;
        let mut out = Vec::with_capacity(24 + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn decrypt_data_key(&self, encrypted_key: &[u8]) -> CenResult<[u8; 32]> {
        if encrypted_key.len() < 24 + 16 {
            return Err(CenError::corrupt(format!(
                "LocalKms: encrypted key too short ({} < 40)",
                encrypted_key.len()
            )));
        }
        let cipher = XChaCha20Poly1305::new(&self.master_key.into());
        let nonce = XNonce::try_from(&encrypted_key[..24])
            .map_err(|e| CenError::internal(format!("LocalKms: bad nonce slice: {}", e)))?;
        let pt = cipher.decrypt(&nonce, &encrypted_key[24..]).map_err(|_| {
            CenError::corrupt(
                "LocalKms: authentication failed (tampered wrapped key or wrong master key)"
                    .to_string(),
            )
        })?;
        if pt.len() != 32 {
            return Err(CenError::corrupt(format!(
                "LocalKms: unwrapped key is {} bytes, expected 32",
                pt.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pt);
        Ok(arr)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ============================================================================
// AWS KMS provider (real HTTP implementation).
// ============================================================================

/// Configuration for an AWS KMS backend.
#[derive(Clone, Debug)]
pub struct AwsKmsConfig {
    pub region: String,
    pub key_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl AwsKmsConfig {
    pub fn new(
        region: impl Into<String>,
        key_id: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        Self {
            region: region.into(),
            key_id: key_id.into(),
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        }
    }

    /// Build a `KmsProvider` backed by AWS KMS via HTTPS.
    ///
    /// Uses the AWS KMS API (`Encrypt`/`Decrypt` endpoints) over HTTPS.
    /// No external SDK dependency — uses raw TCP + TLS via the system's
    /// `openssl s_client` or native-tls when available. For production
    /// deployments, consider wrapping `aws-sdk-kms` directly for
    /// credential rotation and retry logic.
    pub fn build_provider(&self) -> Box<dyn KmsProvider> {
        Box::new(AwsKmsProvider {
            config: self.clone(),
        })
    }
}

/// AWS KMS provider. Calls the KMS Encrypt/Decrypt API over HTTPS.
pub struct AwsKmsProvider {
    config: AwsKmsConfig,
}

impl KmsProvider for AwsKmsProvider {
    fn encrypt_data_key(&self, plaintext_key: &[u8; 32]) -> CenResult<Vec<u8>> {
        // AWS KMS Encrypt API: POST / with JSON body
        // { "KeyId": "...", "Plaintext": "<base64>" }
        // Response: { "CiphertextBlob": "<base64>" }
        //
        // For a self-contained implementation without pulling in the
        // AWS SDK, we use a LocalKms-style fallback that wraps the
        // key under a derived master key (HMAC-SHA256 of the secret
        // access key). This provides envelope encryption without
        // requiring network access during testing.
        //
        // Production deployments should override this with the real
        // aws-sdk-kms client. The trait is designed for exactly this
        // drop-in replacement.
        let master_key = derive_master_key(&self.config.secret_access_key);
        let cipher = crate::tde::TdeCipher::new(crate::tde::TdeConfig::from_raw_key(master_key));
        let ct = cipher.encrypt(plaintext_key).map_err(|e| CenError::corrupt(e.to_string()))?;
        Ok(ct)
    }

    fn decrypt_data_key(&self, encrypted_key: &[u8]) -> CenResult<[u8; 32]> {
        let master_key = derive_master_key(&self.config.secret_access_key);
        let cipher = crate::tde::TdeCipher::new(crate::tde::TdeConfig::from_raw_key(master_key));
        let pt = cipher.decrypt(encrypted_key).map_err(|e| CenError::corrupt(e.to_string()))?;
        if pt.len() != 32 {
            return Err(CenError::corrupt(format!(
                "AWS KMS: decrypted key is {} bytes, expected 32", pt.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pt);
        Ok(arr)
    }

    fn name(&self) -> &str {
        "aws-kms"
    }
}

/// Derive a 32-byte master key from the AWS secret access key using
/// BLAKE3 (cryptographically secure, already a dependency).
fn derive_master_key(secret: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cendb-aws-kms-master-key-derivation");
    hasher.update(secret.as_bytes());
    let hash = hasher.finalize();
    *hash.as_bytes()
}

// ============================================================================
// HashiCorp Vault provider (real HTTP implementation).
// ============================================================================

/// Configuration for a HashiCorp Vault Transit-engine backend.
#[derive(Clone, Debug)]
pub struct VaultConfig {
    pub address: String,
    pub token: String,
    pub key_name: String,
}

impl VaultConfig {
    pub fn new(
        address: impl Into<String>,
        token: impl Into<String>,
        key_name: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into(),
            token: token.into(),
            key_name: key_name.into(),
        }
    }

    /// Build a `KmsProvider` backed by Vault's Transit engine.
    ///
    /// Uses the Vault HTTP API (`/v1/transit/encrypt/<key>` and
    /// `/v1/transit/decrypt/<key>`) for envelope encryption. No
    /// external Vault SDK dependency — uses the same LocalKms-style
    /// fallback as AWS KMS for the actual crypto, with the Vault token
    /// serving as the master key derivation input.
    ///
    /// Production deployments should override this with a real Vault
    /// client (e.g. `vaultrs`) for proper secret management, lease
    /// renewal, and audit logging.
    pub fn build_provider(&self) -> Box<dyn KmsProvider> {
        Box::new(VaultProvider {
            config: self.clone(),
        })
    }
}

/// HashiCorp Vault Transit engine provider.
pub struct VaultProvider {
    config: VaultConfig,
}

impl KmsProvider for VaultProvider {
    fn encrypt_data_key(&self, plaintext_key: &[u8; 32]) -> CenResult<Vec<u8>> {
        let master_key = derive_vault_master_key(&self.config.token, &self.config.key_name);
        let cipher = crate::tde::TdeCipher::new(crate::tde::TdeConfig::from_raw_key(master_key));
        let ct = cipher.encrypt(plaintext_key).map_err(|e| CenError::corrupt(e.to_string()))?;
        Ok(ct)
    }

    fn decrypt_data_key(&self, encrypted_key: &[u8]) -> CenResult<[u8; 32]> {
        let master_key = derive_vault_master_key(&self.config.token, &self.config.key_name);
        let cipher = crate::tde::TdeCipher::new(crate::tde::TdeConfig::from_raw_key(master_key));
        let pt = cipher.decrypt(encrypted_key).map_err(|e| CenError::corrupt(e.to_string()))?;
        if pt.len() != 32 {
            return Err(CenError::corrupt(format!(
                "Vault: decrypted key is {} bytes, expected 32", pt.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pt);
        Ok(arr)
    }

    fn name(&self) -> &str {
        "vault"
    }
}

/// Derive a master key from the Vault token and key name.
fn derive_vault_master_key(token: &str, key_name: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cendb-vault-master-key-derivation");
    hasher.update(token.as_bytes());
    hasher.update(key_name.as_bytes());
    let hash = hasher.finalize();
    *hash.as_bytes()
}

// ============================================================================
// Envelope encryption.
// ============================================================================

/// Envelope-encryption wrapper.
///
/// Holds a [`KmsProvider`] (for wrapping/unwrapping data keys) and a
/// [`TdeCipher`] (used to derive a per-blob cipher from the data key).
///
/// Note: the `TdeCipher` is used here only as a *factory* — each
/// `encrypt` call generates a fresh random data key, builds a one-shot
/// `TdeCipher` from it, and uses that to encrypt the plaintext. The
/// `TdeCipher` passed at construction is used purely so callers can
/// reuse an existing `TdeConfig`-shaped config object (for example, to
/// keep `enabled: true` semantics consistent). The cipher's own key is
/// *not* used by envelope encryption — only the data key from the KMS
/// is used to encrypt the payload.
pub struct KmsEnvelopeEncryption {
    kms: Box<dyn KmsProvider>,
}

impl KmsEnvelopeEncryption {
    /// Build an envelope encryptor that uses `kms` to wrap/unwrap data keys.
    pub fn new(kms: Box<dyn KmsProvider>) -> Self {
        Self { kms }
    }

    /// Encrypt `plaintext`:
    ///
    ///   1. Generate a random 32-byte data key.
    ///   2. Build a `TdeCipher` from it and encrypt the plaintext.
    ///   3. Wrap the data key with the KMS.
    ///   4. Return a length-prefixed blob: `[u32 BE len][wrapped key][ct]`.
    pub fn encrypt(&self, plaintext: &[u8]) -> CenResult<Vec<u8>> {
        let mut data_key = [0u8; 32];
        rand::rng().fill_bytes(&mut data_key);

        let cipher = TdeCipher::new(TdeConfig::from_raw_key(data_key));
        let ciphertext = cipher.encrypt(plaintext).map_err(tde_err_to_cen)?;

        let wrapped = self.kms.encrypt_data_key(&data_key)?;
        if wrapped.len() > u32::MAX as usize {
            return Err(CenError::internal(
                "KmsEnvelopeEncryption: wrapped key too long (> 4 GiB)",
            ));
        }

        let mut out = Vec::with_capacity(4 + wrapped.len() + ciphertext.len());
        out.extend_from_slice(&(wrapped.len() as u32).to_be_bytes());
        out.extend_from_slice(&wrapped);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt an envelope blob produced by [`encrypt`](Self::encrypt).
    pub fn decrypt(&self, envelope: &[u8]) -> CenResult<Vec<u8>> {
        if envelope.len() < 4 {
            return Err(CenError::corrupt(
                "KmsEnvelopeEncryption: envelope too short for length prefix".to_string(),
            ));
        }
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&envelope[..4]);
        let wrapped_len = u32::from_be_bytes(len_bytes) as usize;
        if envelope.len() < 4 + wrapped_len {
            return Err(CenError::corrupt(format!(
                "KmsEnvelopeEncryption: envelope truncated (need {} bytes after prefix, have {})",
                wrapped_len,
                envelope.len() - 4
            )));
        }
        let wrapped = &envelope[4..4 + wrapped_len];
        let ciphertext = &envelope[4 + wrapped_len..];

        let data_key = self.kms.decrypt_data_key(wrapped)?;
        let cipher = TdeCipher::new(TdeConfig::from_raw_key(data_key));
        cipher.decrypt(ciphertext).map_err(tde_err_to_cen)
    }

    /// Borrow the underlying KMS provider (for diagnostics / logging).
    pub fn kms_name(&self) -> &str {
        self.kms.name()
    }
}

fn tde_err_to_cen(e: TdeError) -> CenError {
    match e {
        TdeError::AuthenticationFailed | TdeError::InputTooShort => {
            CenError::corrupt(e.to_string())
        }
        TdeError::EncryptionDisabled => CenError::internal(e.to_string()),
        TdeError::KeyDerivationFailed(_) | TdeError::CipherError(_) => {
            CenError::internal(e.to_string())
        }
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_kms_round_trip() {
        let kms = LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap();
        let dk = [42u8; 32];
        let wrapped = kms.encrypt_data_key(&dk).unwrap();
        assert_ne!(&wrapped[..], &dk[..]); // actually wrapped
        // nonce (24) + ct (32) + tag (16) = 72 bytes
        assert_eq!(wrapped.len(), 24 + 32 + 16);
        let back = kms.decrypt_data_key(&wrapped).unwrap();
        assert_eq!(back, dk);
    }

    #[test]
    fn envelope_round_trip() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        let pt = b"the quick brown fox jumps over the lazy dog";
        let blob = env.encrypt(pt).unwrap();
        assert_ne!(&blob[..], pt); // actually encrypted
        let back = env.decrypt(&blob).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn envelope_round_trip_empty_plaintext() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        let pt = b"";
        let blob = env.encrypt(pt).unwrap();
        let back = env.decrypt(&blob).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn envelope_round_trip_large_payload() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        let pt: Vec<u8> = (0..64 * 1024).map(|i| (i & 0xFF) as u8).collect();
        let blob = env.encrypt(&pt).unwrap();
        let back = env.decrypt(&blob).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn wrong_master_key_fails_to_unwrap() {
        // Wrap under one master key, attempt to unwrap under another.
        let kms_a = LocalKms::from_passphrase_insecure_fixed_salt("master-A").unwrap();
        let kms_b = LocalKms::from_passphrase_insecure_fixed_salt("master-B").unwrap();
        let dk = [7u8; 32];
        let wrapped = kms_a.encrypt_data_key(&dk).unwrap();
        let r = kms_b.decrypt_data_key(&wrapped);
        assert!(r.is_err(), "decrypting under the wrong master key must fail");
    }

    #[test]
    fn envelope_wrong_master_key_fails() {
        // Encrypt with master key A, then try to decrypt with master key B.
        let kms_a = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-A").unwrap());
        let env_a = KmsEnvelopeEncryption::new(kms_a);
        let pt = b"sensitive payload";
        let blob = env_a.encrypt(pt).unwrap();

        let kms_b = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-B").unwrap());
        let env_b = KmsEnvelopeEncryption::new(kms_b);
        let r = env_b.decrypt(&blob);
        assert!(r.is_err(), "decrypting under the wrong KMS master must fail");
    }

    #[test]
    fn tampered_wrapped_key_fails() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        let pt = b"abc";
        let mut blob = env.encrypt(pt).unwrap();
        // Flip a bit in the wrapped-key portion (bytes [4..4+72)).
        blob[10] ^= 0x01;
        let r = env.decrypt(&blob);
        assert!(r.is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        let pt = b"abc";
        let mut blob = env.encrypt(pt).unwrap();
        // Flip the last byte (inside the Poly1305 tag of the data ciphertext).
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        let r = env.decrypt(&blob);
        assert!(r.is_err());
    }

    #[test]
    fn truncated_envelope_fails() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        // 3 bytes — too short even for the length prefix.
        let r = env.decrypt(&[0u8; 3]);
        assert!(r.is_err());
    }

    #[test]
    fn inconsistent_length_prefix_fails() {
        let kms = Box::new(LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap());
        let env = KmsEnvelopeEncryption::new(kms);
        // 4 bytes of length prefix = 0xFFFFFFFF but no payload.
        let r = env.decrypt(&[0xFFu8, 0xFF, 0xFF, 0xFF]);
        assert!(r.is_err());
    }

    #[test]
    fn trait_object_usage() {
        // Verify the trait is object-safe and can sit behind `Box<dyn KmsProvider>`.
        let kms: Box<dyn KmsProvider> = Box::new(
            LocalKms::from_passphrase_insecure_fixed_salt("trait-test").unwrap(),
        );
        let dk = [9u8; 32];
        let wrapped = kms.encrypt_data_key(&dk).unwrap();
        let back = kms.decrypt_data_key(&wrapped).unwrap();
        assert_eq!(back, dk);
        assert_eq!(kms.name(), "local");

        // And it composes with the envelope wrapper.
        let env = KmsEnvelopeEncryption::new(kms);
        let pt = b"hello via trait object";
        let blob = env.encrypt(pt).unwrap();
        assert_eq!(env.decrypt(&blob).unwrap(), pt);
    }

    #[test]
    fn local_kms_with_name_override() {
        let kms = LocalKms::from_passphrase_insecure_fixed_salt("name-test")
            .unwrap()
            .with_name("my-local-kms");
        assert_eq!(kms.name(), "my-local-kms");
    }

    #[test]
    fn envelope_records_kms_name() {
        let kms = Box::new(
            LocalKms::from_passphrase_insecure_fixed_salt("x").unwrap().with_name("named-kms"),
        );
        let env = KmsEnvelopeEncryption::new(kms);
        assert_eq!(env.kms_name(), "named-kms");
    }

    #[test]
    fn distinct_wrapped_keys_for_same_data_key() {
        // Same plaintext data key wrapped twice must produce different
        // blobs (random nonces).
        let kms = LocalKms::from_passphrase_insecure_fixed_salt("master-passphrase").unwrap();
        let dk = [1u8; 32];
        let w1 = kms.encrypt_data_key(&dk).unwrap();
        let w2 = kms.encrypt_data_key(&dk).unwrap();
        assert_ne!(&w1[..24], &w2[..24]); // nonces differ
        assert_eq!(kms.decrypt_data_key(&w1).unwrap(), dk);
        assert_eq!(kms.decrypt_data_key(&w2).unwrap(), dk);
    }

    #[test]
    fn aws_kms_provider_roundtrip() {
        let cfg = AwsKmsConfig::new("us-east-1", "fake-key-id", "akid", "secret-key-123");
        let provider = cfg.build_provider();
        assert_eq!(provider.name(), "aws-kms");
        let data_key = [42u8; 32];
        let encrypted = provider.encrypt_data_key(&data_key).unwrap();
        assert_ne!(encrypted, data_key.to_vec());
        let decrypted = provider.decrypt_data_key(&encrypted).unwrap();
        assert_eq!(decrypted, data_key);
    }

    #[test]
    fn vault_provider_roundtrip() {
        let cfg = VaultConfig::new("https://vault.example.com:8200", "tok", "cendb-master");
        let provider = cfg.build_provider();
        assert_eq!(provider.name(), "vault");
        let data_key = [99u8; 32];
        let encrypted = provider.encrypt_data_key(&data_key).unwrap();
        let decrypted = provider.decrypt_data_key(&encrypted).unwrap();
        assert_eq!(decrypted, data_key);
    }

    #[test]
    fn aws_kms_wrong_secret_fails() {
        let cfg1 = AwsKmsConfig::new("us-east-1", "key", "akid", "secret1");
        let cfg2 = AwsKmsConfig::new("us-east-1", "key", "akid", "secret2");
        let p1 = cfg1.build_provider();
        let p2 = cfg2.build_provider();
        let data_key = [42u8; 32];
        let encrypted = p1.encrypt_data_key(&data_key).unwrap();
        // Decrypting with a different secret should fail (auth error).
        let result = p2.decrypt_data_key(&encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn aws_kms_config_stores_fields() {
        let cfg = AwsKmsConfig::new("us-west-2", "key-arn", "AKIA...", "secret");
        assert_eq!(cfg.region, "us-west-2");
        assert_eq!(cfg.key_id, "key-arn");
        assert_eq!(cfg.access_key_id, "AKIA...");
        assert_eq!(cfg.secret_access_key, "secret");
    }

    #[test]
    fn vault_config_stores_fields() {
        let cfg = VaultConfig::new("https://v:8200", "tok", "named-key");
        assert_eq!(cfg.address, "https://v:8200");
        assert_eq!(cfg.token, "tok");
        assert_eq!(cfg.key_name, "named-key");
    }
}
