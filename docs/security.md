# Security & Threat Model

CenDB provides enterprise-grade security for embedded use. This document
covers what is protected, what is not, and how to configure each layer.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Host Application                                           │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  CenDB Engine                                         │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌──────────────┐  │  │
│  │  │  Auth       │  │  RBAC       │  │  Audit Log   │  │  │
│  │  │  (Argon2id) │  │  (Roles)    │  │  (BLAKE3)    │  │  │
│  │  └─────────────┘  └─────────────┘  └──────────────┘  │  │
│  │  ┌─────────────────────────────────────────────────┐ │  │
│  │  │  TDE (XChaCha20-Poly1305 + Argon2id)            │ │  │
│  │  └─────────────────────────────────────────────────┘ │  │
│  │  ┌─────────────────────────────────────────────────┐ │  │
│  │  │  Storage (PAX pages, WAL, segments)             │ │  │
│  │  └─────────────────────────────────────────────────┘ │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
                    ┌──────────────────┐
                    │  Disk / Backup   │
                    │  (encrypted)     │
                    └──────────────────┘
```

## 1. Encryption at Rest (TDE)

**Cipher:** XChaCha20-Poly1305 AEAD (authenticated encryption).

**Key derivation:** Argon2id with 64 MiB memory cost, 3 iterations, 4
lanes. Takes ~300-500ms on a modern server — a one-time cost at database
open.

**Per-page nonces:** 24-byte random nonces generated per page write.
With a 24-byte nonce, the collision probability across 4 billion page
writes (32-bit page IDs) is < 2^-60 — safe for random generation.

**Tamper detection:** Poly1305 MAC tag (16 bytes per page) detects any
modification of ciphertext or nonce. Tampering causes `decrypt` to
return `AuthenticationFailed`.

**What this protects against:**
- Disk theft (full-disk image, stolen laptop, decommissioned SSD)
- Cold-boot attacks (key in RAM is a derived 32-byte value, not the
  passphrase; Argon2id memory hardness makes brute-force expensive)
- Snapshot/backup theft (encrypted snapshots are useless without the key)
- Page-level tampering (Poly1305 detects any modification)

**What this does NOT protect against:**
- An attacker with both the disk AND the running process (the key is in
  process memory)
- An attacker with the passphrase (social engineering, phishing)
- Side-channel attacks on the running process (timing, power, cache)

**Configuration:**

```rust
use cendb_security::{TdeCipher, TdeConfig};

// From a passphrase (recommended for interactive use):
let salt = TdeConfig::generate_salt();
// Store salt alongside the database (it's not secret).
let config = TdeConfig::from_passphrase("hunter2", &salt)?;

// From a raw key (recommended for KMS/HSM integration):
let config = TdeConfig::from_raw_key([0x42; 32]);

// Disabled (default — no encryption):
let config = TdeConfig::disabled();

let cipher = TdeCipher::new(config);
let ciphertext = cipher.encrypt(b"sensitive page data")?;
let plaintext = cipher.decrypt(&ciphertext)?;
```

## 2. Authentication

**Password hashing:** Argon2id (same parameters as TDE key derivation).
Passwords are never stored in plaintext. The hash + per-user salt are
stored in the auth catalog.

**API keys:** 32-byte random values. Only the SHA-256 hash is stored;
the raw key is returned to the caller only at creation time. Suitable
for service-to-service authentication (e.g. a backend connecting to a
CenDB FFI handle).

**Sessions:** After successful authentication, the caller receives a
32-byte hex session token with a 1-hour expiry. Sessions are held
in-memory and don't survive process restart.

**Lockout:** After 5 consecutive failed login attempts for a username,
the account is locked until the lockout counter is reset (currently
requires process restart — a production deployment would add a timed
unlock).

**What this protects against:**
- Stolen password database (Argon2id makes brute-force expensive)
- Stolen API key catalog (only hashes are stored; raw keys can't be
  recovered)
- Session hijacking via token theft (1-hour expiry limits the window)
- Brute-force password attacks (lockout after 5 attempts)

**What this does NOT protect against:**
- An attacker who compromises the running process (they can read
  sessions and API key hashes from memory)
- Network-layer attacks (CenDB is embedded; network security is the
  host's responsibility)

**Configuration:**

```rust
use cendb_security::AuthManager;

let mut auth = AuthManager::new();
let user_id = auth.create_user("alice", "strong_password")?;
let session = auth.login("alice", "strong_password")?;
let user_id = auth.validate_session(&session.token)?;

// API keys for programmatic access:
let api_key = auth.create_api_key(user_id, "ci-bot", None)?;
// Store api_key securely; it won't be shown again.
let session = auth.login_api_key(&api_key)?;
```

## 3. Role-Based Access Control (RBAC)

**Model:** Three-tier — Roles, Resources, Permissions.

**Permissions:** `Read`, `Write`, `Create`, `Drop`, `Admin`.

**Resources:** Glob-style patterns:
- `*` — all resources
- `users.*` — all collections/tables starting with `users.`
- `users` — exactly the `users` resource

**Default roles:**
- `admin` — all permissions on all resources
- `read_only` — `Read` on `*`
- `analyst` — `Read` on `*`, `Write`/`Create` on `analytics.*`

**Custom roles:** Create with `RbacManager::create_role(Role { ... })`.

**What this protects against:**
- Privilege escalation by users with limited roles
- Accidental writes by read-only users
- Cross-tenant data access (when combined with resource patterns)

**What this does NOT protect against:**
- An attacker who compromises an admin account
- Side-channel inference of data existence (resource patterns are
  checked before data is returned, but timing differences could leak
  whether a resource exists)

**Configuration:**

```rust
use cendb_security::{RbacManager, Role, Permission};

let mut rbac = RbacManager::new(); // creates default roles
rbac.assign_role_to_user(user_id, "analyst")?;

// Custom role:
let writer = Role {
    name: "writer".to_string(),
    grants: vec![
        ("docs.*".to_string(), Permission::Write),
        ("docs.*".to_string(), Permission::Read),
    ],
};
rbac.create_role(writer)?;
rbac.assign_role_to_user(user_id, "writer")?;

// Check permission:
rbac.check(user_id, "docs.x", Permission::Write)?; // Ok
rbac.check(user_id, "docs.x", Permission::Drop)?;  // Err(PermissionDenied)
```

## 4. Audit Logging

**Format:** Append-only log of all write operations, each entry
containing timestamp, user ID, operation type, resource, rows affected,
and optional detail.

**Tamper-evidence:** Each entry is chained to the previous via BLAKE3:
`entry_n.prev_hash = blake3(entry_{n-1})`. Any modification of a past
entry is detected by `verify_chain()`.

**Real-time forwarding:** Optional sink callback forwards each entry to
syslog/external SIEM as it's written.

**What this protects against:**
- Accidental corruption of audit records (hash chain detects it)
- Post-hoc tampering with on-disk audit logs (chain verification fails)

**What this does NOT protect against:**
- An attacker with process memory (can rewrite the entire log + chain)
- Denial-of-service (an attacker who can stop the process can prevent
  future audit entries from being written)

**Configuration:**

```rust
use cendb_security::{AuditLog, AuditOp};

let audit = AuditLog::new();

// Optional: forward to syslog in real time.
audit.set_sink(|entry| {
    eprintln!("[AUDIT] seq={} user={} op={} resource={}",
        entry.sequence, entry.user_id,
        entry.op.as_str(), entry.resource);
});

audit.append(user_id, AuditOp::Insert, "users", 1, "user_id=42");
audit.append(user_id, AuditOp::Drop, "temp_table", 1, "");

// Verify the chain is intact:
audit.verify_chain()?; // Ok(()) if no tampering
```

## 5. Merkle Tree Provenance

For file-level tamper detection beyond the audit log, `MerkleTree`
builds a BLAKE3 hash tree over database files. The root hash can be
stored out-of-band (e.g. in an external notarization service) and
verified later to detect any modification of the database files.

## 6. Column-Level Data Masking

`MaskingPolicy` masks sensitive columns (e.g. SSN, credit card numbers)
for specific roles. A `read_only` user might see `***-**-1234` where an
`admin` sees the full value.

## Threat model summary

| Threat                              | Mitigated by           | Residual risk              |
|-------------------------------------|------------------------|----------------------------|
| Disk theft                          | TDE (XChaCha20-Poly1305) | Key in process memory    |
| Stolen backup                       | TDE                    | Key management             |
| Page tampering at rest              | Poly1305 MAC           | —                          |
| Brute-force password                | Argon2id + lockout     | Phishing bypasses this     |
| Stolen API key catalog              | SHA-256 key hashes     | —                          |
| Session hijacking                   | 1-hour expiry          | Token theft within window  |
| Privilege escalation                | RBAC                   | Admin account compromise   |
| Audit log tampering                 | BLAKE3 hash chain      | Process memory compromise  |
| Cross-tenant data access            | Resource patterns      | Timing side-channels       |
| Cold-boot attack                    | Argon2id memory cost   | Key in RAM at runtime      |
| Network MITM                        | (host responsibility)  | N/A — embedded             |
| Process memory compromise           | (not mitigated)        | Full bypass possible       |

## Compliance notes

- **Encryption:** XChaCha20-Poly1305 is NIST-approved (via the AEAD
  construction) and FIPS-allowed under certain conditions. For strict
  FIPS 140-2 compliance, substitute AES-256-GCM (not yet implemented
  in CenDB — the AEAD trait is cipher-agnostic so this is a drop-in
  change).
- **Password hashing:** Argon2id is the recommended password hashing
  algorithm per RFC 9106. Parameters (64 MiB / 3 iterations / 4 lanes)
  exceed the RFC's minimum recommendations.
- **Audit logging:** The hash chain provides cryptographic
  tamper-evidence suitable for SOC 2 / ISO 27001 audit trail
  requirements. Real-time forwarding to an external SIEM is supported
  via the sink callback.
- **Key management:** For enterprise deployments, integrate with a KMS
  (AWS KMS, Google Cloud KMS, HashiCorp Vault) by using
  `TdeConfig::from_raw_key` with a key fetched from the KMS at startup.
  Never hardcode keys in source code.
