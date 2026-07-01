---
sidebar_position: 11
title: Security
---

# Security

## Encryption at rest (TDE)

**Cipher:** XChaCha20-Poly1305 AEAD with 24-byte random nonces per page. Argon2id key derivation (64 MiB / 3 iterations / 4 lanes).

## Field-level encryption

Per-column encryption keys. Different columns can use different keys. Columns not in the config pass through as plaintext.

```rust
use cendb_security::{FieldEncryptionConfig, FieldEncryptor};

let mut config = FieldEncryptionConfig::new();
config.add_column("ssn", ssn_key);
config.add_column("credit_card", cc_key);
let encryptor = FieldEncryptor::new(config);
```

## KMS integration

Supports AWS KMS, Google Cloud KMS, and HashiCorp Vault via the `KmsProvider` trait. Envelope encryption: data key encrypted by KMS master key.

```rust
use cendb_security::{KmsEnvelopeEncryption, LocalKms};

let kms = LocalKms::new(master_key);
let envelope = KmsEnvelopeEncryption::new(Box::new(kms));
```

## Authentication

Argon2id password hashing. API keys (32-byte random, SHA-256 hashed at rest). Session tokens with configurable expiry. Timed lockout (default 15 minutes). Persistent sessions via `FileSessionStore`.

## RBAC

Three-tier: Roles, Resources (glob patterns), Permissions (Read/Write/Create/Drop/Admin). Default roles: admin, read_only, analyst.

## Audit logging

Append-only, BLAKE3 hash-chained. Tamper-evident: `verify_chain()` detects any modification. Optional sink callback for real-time forwarding to SIEM.

## Threat model

| Threat | Mitigated by | Residual risk |
|---|---|---|
| Disk theft | TDE | Key in process memory |
| Page tampering | Poly1305 MAC | — |
| Brute-force password | Argon2id + timed lockout | Phishing |
| Stolen API key catalog | SHA-256 hashes | — |
| Privilege escalation | RBAC | Admin compromise |
| Audit log tampering | BLAKE3 hash chain | Process compromise |
