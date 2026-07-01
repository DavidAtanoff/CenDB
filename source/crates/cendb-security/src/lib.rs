//! cendb-security: enterprise security features.
//!
//!   * **Transparent Data Encryption (TDE)** — page-level encryption
//!     using XChaCha20-Poly1305 AEAD with Argon2id key derivation.
//!   * **Field-level Encryption** — per-column encryption under
//!     independent keys, complementing TDE.
//!   * **Key Management Service (KMS)** — abstraction over AWS KMS,
//!     Google Cloud KMS, and HashiCorp Vault, with a local in-process
//!     KMS for tests; supports envelope encryption.
//!   * **Authentication** — user/credential model with Argon2id
//!     password hashing, API keys, session tokens, time-based lockout,
//!     and pluggable persistent session storage.
//!   * **Role-Based Access Control (RBAC)** — three-tier permission
//!     model (roles, resources, permissions) with glob-style resource
//!     patterns.
//!   * **Audit Logging** — tamper-evident append-only log of all write
//!     operations, chained via BLAKE3 hashes.
//!   * **Merkle Tree Provenance** — detect unauthorized tampering of
//!     database files at rest via a cryptographic hash chain.
//!   * **Column-Level Data Masking** — mask sensitive columns for
//!     specific users/roles.

pub mod audit;
pub mod auth;
pub mod field_encryption;
pub mod kms;
pub mod masking;
pub mod merkle;
pub mod rbac;
pub mod session_store;
pub mod tde;

pub use audit::{AuditEntry, AuditLog, AuditOp};
pub use auth::{ApiKey, AuthError, AuthManager, Session, User};
pub use field_encryption::{FieldEncryptionConfig, FieldEncryptor};
pub use kms::{
    AwsKmsConfig, KmsEnvelopeEncryption, KmsProvider, LocalKms, VaultConfig,
};
pub use masking::{ColumnMask, MaskingPolicy, MaskingRule};
pub use merkle::{MerkleTree, MerkleProof};
pub use rbac::{Permission, RbacError, RbacManager, Role};
pub use session_store::{FileSessionStore, InMemorySessionStore, SessionStore};
pub use tde::{TdeCipher, TdeConfig, TdeError};
