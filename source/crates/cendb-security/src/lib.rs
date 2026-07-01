//! cendb-security: enterprise security features.
//!
//!   * **Merkle Tree Provenance** — detect unauthorized tampering of
//!     database files at rest via a cryptographic hash chain.
//!   * **Transparent Data Encryption (TDE)** — encrypt data pages
//!     transparently using XOR-Keystream (prototype) or AES-GCM.
//!   * **Column-Level Data Masking** — mask sensitive columns for
//!     specific users/roles.

pub mod merkle;
pub mod tde;
pub mod masking;

pub use merkle::{MerkleTree, MerkleProof};
pub use tde::{TdeCipher, TdeConfig, TdeError};
pub use masking::{ColumnMask, MaskingPolicy, MaskingRule};
