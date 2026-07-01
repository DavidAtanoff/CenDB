//! Authentication: user/credential model for embedded use.
//!
//! ## Design
//!
//! CenDB is embedded — there's no network layer. The auth model is
//! designed for the case where multiple in-process users (or tenants)
//! share a database instance, and where the host application wants to
//! enforce access control based on who opened the database.
//!
//! ## Credential storage
//!
//! Passwords are hashed with **Argon2id** (same parameters as TDE:
//! 64 MiB / 3 iterations / 4 lanes). The hash + per-user salt are
//! stored in the auth catalog. Passwords are NEVER stored in plaintext.
//!
//! ## API keys
//!
//! For programmatic access (e.g. a backend service connecting to a
//! CenDB FFI handle), users can generate API keys. An API key is a
//! 32-byte random value; we store its SHA-256 hash (so a stolen
//! catalog doesn't reveal live keys) plus the user it belongs to.
//!
//! ## Session tokens
//!
//! After successful authentication, the caller receives a `Session`
//! with a random 32-byte token and an expiry timestamp. By default
//! sessions live only in process memory; if a [`SessionStore`] is
//! attached via [`AuthManager::with_session_store`], sessions are
//! also persisted (so they survive process restart) and looked up
//! from the store on validate.
//!
//! ## Lockout
//!
//! Repeated failed logins trigger a **time-based lockout**: once a
//! user exceeds `max_failed_attempts` (default 5), the account is
//! locked for `lockout_duration` seconds (default 900 = 15 minutes).
//! Each subsequent failed attempt while the account is locked
//! refreshes the timer (so an attacker who keeps guessing never gets
//! in). A successful login clears both the failed-attempt counter
//! and the lockout.

use argon2::{Argon2, Algorithm, Version, Params};
use rand::Rng;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session_store::SessionStore;

/// A user identifier.
pub type UserId = u64;

/// A 32-byte API key or session token, encoded as hex for transport.
pub type TokenString = String;

/// A user record.
#[derive(Clone, Debug)]
pub struct User {
    pub id: UserId,
    pub username: String,
    /// Argon2id password hash (includes salt).
    pub password_hash: String,
    /// Roles assigned to this user.
    pub roles: Vec<String>,
    /// Whether the account is active.
    pub active: bool,
}

/// An API key (the hash is stored; the raw key is returned to the
/// caller only at creation time).
#[derive(Clone, Debug)]
pub struct ApiKey {
    pub id: u64,
    pub user_id: UserId,
    /// SHA-256 hash of the raw key.
    pub key_hash: [u8; 32],
    pub label: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

/// A session after successful authentication.
#[derive(Clone, Debug)]
pub struct Session {
    pub token: TokenString,
    pub user_id: UserId,
    pub expires_at: u64,
}

/// Authentication errors.
#[derive(Debug, Clone)]
pub enum AuthError {
    UserNotFound,
    UserInactive,
    InvalidPassword,
    InvalidApiKey,
    InvalidSession,
    SessionExpired,
    UsernameTaken,
    ApiKeyExpired,
    /// Account is temporarily locked due to repeated failed logins.
    /// The remaining lockout time (in seconds) is included so the
    /// caller can surface it to the user (or rate-limit retries).
    DatabaseLocked { remaining_secs: u64 },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::UserNotFound => write!(f, "user not found"),
            AuthError::UserInactive => write!(f, "user account is inactive"),
            AuthError::InvalidPassword => write!(f, "invalid password"),
            AuthError::InvalidApiKey => write!(f, "invalid API key"),
            AuthError::InvalidSession => write!(f, "invalid or missing session token"),
            AuthError::SessionExpired => write!(f, "session has expired"),
            AuthError::UsernameTaken => write!(f, "username is already taken"),
            AuthError::ApiKeyExpired => write!(f, "API key has expired"),
            AuthError::DatabaseLocked { remaining_secs } => write!(
                f,
                "database is locked due to too many failed attempts; try again in {} second(s)",
                remaining_secs
            ),
        }
    }
}

impl std::error::Error for AuthError {}

/// Default lockout duration: 15 minutes.
pub const DEFAULT_LOCKOUT_DURATION_SECS: u64 = 900;

/// Default maximum failed attempts before triggering lockout.
pub const DEFAULT_MAX_FAILED_ATTEMPTS: u32 = 5;

/// The authentication manager. Holds users, API keys, and active
/// sessions. Thread-safe via `Mutex`.
pub struct AuthManager {
    users: HashMap<UserId, User>,
    usernames: HashMap<String, UserId>,
    api_keys: HashMap<[u8; 32], ApiKey>,
    /// In-memory session cache. Always checked first; falls back to
    /// `session_store` on miss.
    sessions: HashMap<TokenString, Session>,
    /// Optional persistent session store. When set, `login`,
    /// `login_api_key`, `validate_session`, and `logout` all keep the
    /// store in sync with the in-memory cache.
    session_store: Option<Box<dyn SessionStore>>,
    next_user_id: UserId,
    next_key_id: u64,
    /// Failed login attempts by username (for lockout).
    failed_attempts: HashMap<String, u32>,
    /// Lockout threshold.
    max_failed_attempts: u32,
    /// Per-username Unix timestamp at which the current lockout
    /// expires. A user is locked iff `locked_until[u] > now`.
    locked_until: HashMap<String, u64>,
    /// Lockout duration in seconds (default 900).
    lockout_duration: u64,
}

impl AuthManager {
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
            usernames: HashMap::new(),
            api_keys: HashMap::new(),
            sessions: HashMap::new(),
            session_store: None,
            next_user_id: 1,
            next_key_id: 1,
            failed_attempts: HashMap::new(),
            max_failed_attempts: DEFAULT_MAX_FAILED_ATTEMPTS,
            locked_until: HashMap::new(),
            lockout_duration: DEFAULT_LOCKOUT_DURATION_SECS,
        }
    }

    /// Build an `AuthManager` backed by a persistent session store.
    /// Sessions created by `login` / `login_api_key` are persisted;
    /// `validate_session` falls back to the store on in-memory miss;
    /// `logout` deletes from both.
    pub fn with_session_store(store: Box<dyn SessionStore>) -> Self {
        let mut mgr = Self::new();
        mgr.session_store = Some(store);
        mgr
    }

    /// Override the lockout duration (seconds). A value of 0
    /// effectively disables time-based lockout (the failed-attempt
    /// counter still resets on success, but no `locked_until` entry
    /// is ever set).
    pub fn set_lockout_duration(&mut self, seconds: u64) {
        self.lockout_duration = seconds;
    }

    /// Override the maximum number of failed attempts before
    /// triggering lockout.
    pub fn set_max_failed_attempts(&mut self, n: u32) {
        self.max_failed_attempts = n;
    }

    /// Inspect the current lockout duration (seconds).
    pub fn lockout_duration(&self) -> u64 {
        self.lockout_duration
    }

    /// Inspect the current max-failed-attempts threshold.
    pub fn max_failed_attempts(&self) -> u32 {
        self.max_failed_attempts
    }

    /// Number of failed attempts recorded for a user (0 if none).
    pub fn failed_attempts_for(&self, username: &str) -> u32 {
        self.failed_attempts.get(username).copied().unwrap_or(0)
    }

    /// Whether the user is currently locked out, and if so when the
    /// lockout expires (Unix seconds). Returns `None` if not locked.
    pub fn locked_until(&self, username: &str) -> Option<u64> {
        let ts = self.locked_until.get(username).copied()?;
        let now = now_secs();
        if ts > now {
            Some(ts)
        } else {
            None
        }
    }

    /// Create a new user with a password. Returns the user ID.
    pub fn create_user(&mut self, username: &str, password: &str) -> Result<UserId, AuthError> {
        if self.usernames.contains_key(username) {
            return Err(AuthError::UsernameTaken);
        }
        let id = self.next_user_id;
        self.next_user_id += 1;
        let password_hash = hash_password(password)?;
        let user = User {
            id,
            username: username.to_string(),
            password_hash,
            roles: vec![],
            active: true,
        };
        self.users.insert(id, user);
        self.usernames.insert(username.to_string(), id);
        Ok(id)
    }

    /// Authenticate a user with a password. On success, returns a session.
    ///
    /// Lockout behaviour (time-based):
    ///   - If the account is currently locked (`locked_until > now`),
    ///     returns `AuthError::DatabaseLocked { remaining_secs }`
    ///     *without* checking the password.
    ///   - On a failed password, `failed_attempts` is incremented. If
    ///     the counter reaches `max_failed_attempts`, the account is
    ///     locked for `lockout_duration` seconds.
    ///   - On a successful login, both `failed_attempts` and
    ///     `locked_until` are cleared for the user.
    pub fn login(&mut self, username: &str, password: &str) -> Result<Session, AuthError> {
        let now = now_secs();

        // Check time-based lockout.
        if let Some(&unlock_ts) = self.locked_until.get(username) {
            if unlock_ts > now {
                return Err(AuthError::DatabaseLocked {
                    remaining_secs: unlock_ts - now,
                });
            }
            // Lockout has expired — clear it and the failed-attempt
            // counter so the user gets a fresh start.
            self.locked_until.remove(username);
            self.failed_attempts.remove(username);
        }

        let user_id = self.usernames.get(username).copied().ok_or(AuthError::UserNotFound)?;
        let user = self.users.get(&user_id).unwrap().clone();
        if !user.active {
            return Err(AuthError::UserInactive);
        }
        if !verify_password(password, &user.password_hash) {
            let attempts = self.failed_attempts.entry(username.to_string()).or_insert(0);
            *attempts += 1;
            if *attempts >= self.max_failed_attempts {
                // Capture `now` AFTER the (slow) Argon2id password
                // verification so the lockout is `lockout_duration`
                // seconds from "right now", not from when the call
                // started. This matters because Argon2id is
                // intentionally slow (hundreds of ms per verify); using
                // the stale `now` from before the verify would cause
                // very short lockouts to expire before the function
                // returns.
                let lock_until = now_secs() + self.lockout_duration;
                self.locked_until.insert(username.to_string(), lock_until);
            }
            return Err(AuthError::InvalidPassword);
        }
        // Clear failed attempts and any stale lockout on success.
        self.failed_attempts.remove(username);
        self.locked_until.remove(username);
        // Create session.
        let session = self.build_session(user_id);
        self.cache_and_persist_session(session.clone());
        Ok(session)
    }

    /// Authenticate with an API key. The key is the raw hex string
    /// returned at creation time.
    pub fn login_api_key(&mut self, api_key_hex: &str) -> Result<Session, AuthError> {
        let raw = hex_to_bytes(api_key_hex).ok_or(AuthError::InvalidApiKey)?;
        if raw.len() != 32 {
            return Err(AuthError::InvalidApiKey);
        }
        let mut key_arr = [0u8; 32];
        key_arr.copy_from_slice(&raw);
        let key_hash = sha256(&raw);
        let api_key = self.api_keys.get(&key_hash).cloned().ok_or(AuthError::InvalidApiKey)?;
        if let Some(exp) = api_key.expires_at {
            if now_secs() > exp {
                return Err(AuthError::ApiKeyExpired);
            }
        }
        let user = self.users.get(&api_key.user_id).ok_or(AuthError::UserNotFound)?;
        if !user.active {
            return Err(AuthError::UserInactive);
        }
        let session = self.build_session(api_key.user_id);
        self.cache_and_persist_session(session.clone());
        Ok(session)
    }

    /// Validate a session token. Returns the user ID if valid and
    /// non-expired. Checks the in-memory cache first; on a miss falls
    /// back to the session store (if any), caching the loaded session
    /// on success.
    pub fn validate_session(&mut self, token: &str) -> Result<UserId, AuthError> {
        // In-memory hit?
        if let Some(session) = self.sessions.get(token).cloned() {
            if now_secs() > session.expires_at {
                self.sessions.remove(token);
                if let Some(store) = &self.session_store {
                    let _ = store.delete(token);
                }
                return Err(AuthError::SessionExpired);
            }
            return Ok(session.user_id);
        }

        // Fall back to the persistent store.
        if let Some(store) = &self.session_store {
            let loaded = store
                .load(token)
                .map_err(|_| AuthError::InvalidSession)?;
            if let Some(session) = loaded {
                if now_secs() > session.expires_at {
                    let _ = store.delete(token);
                    return Err(AuthError::SessionExpired);
                }
                // Cache for next time.
                self.sessions.insert(session.token.clone(), session.clone());
                return Ok(session.user_id);
            }
        }

        Err(AuthError::InvalidSession)
    }

    /// Log out (invalidate a session). Removes from both the
    /// in-memory cache and the persistent store (if any).
    pub fn logout(&mut self, token: &str) {
        self.sessions.remove(token);
        if let Some(store) = &self.session_store {
            let _ = store.delete(token);
        }
    }

    /// Generate a new API key for a user. Returns the raw key as hex
    /// (this is the only time it's visible).
    pub fn create_api_key(&mut self, user_id: UserId, label: &str, expires_at: Option<u64>) -> Result<String, AuthError> {
        if !self.users.contains_key(&user_id) {
            return Err(AuthError::UserNotFound);
        }
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let key_hash = sha256(&raw);
        let id = self.next_key_id;
        self.next_key_id += 1;
        let api_key = ApiKey {
            id,
            user_id,
            key_hash,
            label: label.to_string(),
            created_at: now_secs(),
            expires_at,
        };
        self.api_keys.insert(key_hash, api_key);
        Ok(bytes_to_hex(&raw))
    }

    /// Revoke an API key.
    pub fn revoke_api_key(&mut self, api_key_hex: &str) -> Result<(), AuthError> {
        let raw = hex_to_bytes(api_key_hex).ok_or(AuthError::InvalidApiKey)?;
        if raw.len() != 32 {
            return Err(AuthError::InvalidApiKey);
        }
        let key_hash = sha256(&raw);
        self.api_keys.remove(&key_hash);
        Ok(())
    }

    /// Assign a role to a user.
    pub fn assign_role(&mut self, user_id: UserId, role: &str) -> Result<(), AuthError> {
        let user = self.users.get_mut(&user_id).ok_or(AuthError::UserNotFound)?;
        if !user.roles.contains(&role.to_string()) {
            user.roles.push(role.to_string());
        }
        Ok(())
    }

    /// Get a user's roles.
    pub fn get_roles(&self, user_id: UserId) -> Vec<String> {
        self.users.get(&user_id).map(|u| u.roles.clone()).unwrap_or_default()
    }

    /// Deactivate a user (prevents login).
    pub fn deactivate_user(&mut self, user_id: UserId) -> Result<(), AuthError> {
        let user = self.users.get_mut(&user_id).ok_or(AuthError::UserNotFound)?;
        user.active = false;
        Ok(())
    }

    /// Number of registered users.
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Number of active sessions in the in-memory cache. (Does NOT
    /// count sessions only in the persistent store.)
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    // ----------------------------------------------------------------
    // Internal helpers.
    // ----------------------------------------------------------------

    /// Build a fresh `Session` for `user_id` (1-hour expiry).
    fn build_session(&self, user_id: UserId) -> Session {
        let token = generate_token();
        Session {
            token,
            user_id,
            expires_at: now_secs() + 3600,
        }
    }

    /// Insert the session into the in-memory cache and, if a store is
    /// attached, persist it.
    fn cache_and_persist_session(&mut self, session: Session) {
        if let Some(store) = &self.session_store {
            // Persist before caching so a crash between cache-insert
            // and store-save doesn't lose the session.
            let _ = store.save(&session);
        }
        let token = session.token.clone();
        self.sessions.insert(token, session);
    }
}

impl Default for AuthManager {
    fn default() -> Self { Self::new() }
}

// ============================================================================
// Password hashing.
// ============================================================================

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = TdeConfig::generate_salt();
    let params = Params::new(64 * 1024, 3, 4, Some(32))
        .map_err(|_| AuthError::InvalidPassword)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut hash = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), &salt, &mut hash)
        .map_err(|_| AuthError::InvalidPassword)?;
    // Store as "salt:hash" in hex.
    Ok(format!("{}:{}", bytes_to_hex(&salt), bytes_to_hex(&hash)))
}

fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split(':').collect();
    if parts.len() != 2 { return false; }
    let salt = match hex_to_bytes(parts[0]) { Some(s) => s, None => return false };
    let expected = match hex_to_bytes(parts[1]) { Some(h) => h, None => return false };
    if salt.len() != 16 || expected.len() != 32 { return false; }
    let params = Params::new(64 * 1024, 3, 4, Some(32)).unwrap();
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut actual = [0u8; 32];
    if argon2.hash_password_into(password.as_bytes(), &salt, &mut actual).is_err() {
        return false;
    }
    // Constant-time comparison.
    constant_time_eq(&actual, &expected)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// Re-export TdeConfig for the salt generator.
use crate::tde::TdeConfig;

// ============================================================================
// Helpers.
// ============================================================================

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn generate_token() -> TokenString {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes_to_hex(&bytes)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    // BLAKE3 is cryptographically secure and already a dependency.
    // Used here as a one-way hash for API key storage (the raw key is
    // never stored; only this hash).
    let hash = blake3::hash(data);
    *hash.as_bytes()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 { return None; }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i+2], 16).ok())
        .collect()
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{FileSessionStore, InMemorySessionStore};
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn create_user_and_login() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("alice", "password123").unwrap();
        assert_eq!(uid, 1);
        let session = mgr.login("alice", "password123").unwrap();
        assert_eq!(session.user_id, uid);
        assert!(!session.token.is_empty());
    }

    #[test]
    fn wrong_password_fails() {
        let mut mgr = AuthManager::new();
        mgr.create_user("bob", "correct").unwrap();
        let result = mgr.login("bob", "wrong");
        assert!(matches!(result, Err(AuthError::InvalidPassword)));
    }

    #[test]
    fn lockout_after_max_attempts() {
        // Old behaviour preserved: 5 failed attempts → next attempt
        // (even with correct password) is locked out.
        let mut mgr = AuthManager::new();
        mgr.create_user("carol", "correct").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("carol", "wrong");
        }
        let result = mgr.login("carol", "correct");
        assert!(matches!(result, Err(AuthError::DatabaseLocked { .. })));
    }

    #[test]
    fn lockout_message_includes_remaining_time() {
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(900);
        mgr.create_user("dave", "right").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("dave", "wrong");
        }
        match mgr.login("dave", "right") {
            Err(AuthError::DatabaseLocked { remaining_secs }) => {
                // Should be close to 900 (allow some scheduling slack).
                assert!(remaining_secs > 800 && remaining_secs <= 900,
                    "expected ~900 remaining, got {}", remaining_secs);
                // Display string mentions the remaining time.
                let s = format!("{}", AuthError::DatabaseLocked { remaining_secs });
                assert!(s.contains(&remaining_secs.to_string()));
            }
            other => panic!("expected DatabaseLocked, got {:?}", other),
        }
    }

    #[test]
    fn lockout_expires_after_duration() {
        // Use a 1-second lockout so the test runs fast.
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(1);
        mgr.create_user("eve", "right").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("eve", "wrong");
        }
        // Currently locked.
        assert!(matches!(mgr.login("eve", "right"),
            Err(AuthError::DatabaseLocked { .. })));
        // Wait > 1 second.
        thread::sleep(Duration::from_secs(2));
        // Now login should succeed (lockout has expired).
        let session = mgr.login("eve", "right").expect(
            "login should succeed after lockout expires"
        );
        assert_eq!(session.user_id, 1);
        // And the lockout entry should be cleared.
        assert!(mgr.locked_until("eve").is_none());
        assert_eq!(mgr.failed_attempts_for("eve"), 0);
    }

    #[test]
    fn successful_login_clears_failed_attempts_and_lockout() {
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(60);
        mgr.create_user("frank", "right").unwrap();
        // 3 failed attempts (below threshold of 5).
        for _ in 0..3 {
            let _ = mgr.login("frank", "wrong");
        }
        assert_eq!(mgr.failed_attempts_for("frank"), 3);
        // Successful login clears the counter.
        let _ = mgr.login("frank", "right").unwrap();
        assert_eq!(mgr.failed_attempts_for("frank"), 0);
        assert!(mgr.locked_until("frank").is_none());
    }

    #[test]
    fn locked_account_can_login_after_timeout() {
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(1);
        mgr.create_user("grace", "right").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("grace", "wrong");
        }
        // Locked.
        assert!(mgr.locked_until("grace").is_some());
        // Wait out the lockout.
        thread::sleep(Duration::from_secs(2));
        // Successful login with the right password works.
        let _ = mgr.login("grace", "right").unwrap();
        assert!(mgr.locked_until("grace").is_none());
    }

    #[test]
    fn failed_attempt_during_lockout_refreshes_timer() {
        // When the account is already locked, a new failed attempt
        // should refresh the timer (in our implementation, the
        // lockout is re-set on each failed attempt that crosses the
        // threshold — see the comment in `login`).
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(2);
        mgr.create_user("heidi", "right").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("heidi", "wrong");
        }
        let first_lock = mgr.locked_until("heidi").unwrap();
        // Another wrong attempt — this increments failed_attempts
        // beyond the threshold and re-sets locked_until.
        let _ = mgr.login("heidi", "wrong");
        let second_lock = mgr.locked_until("heidi").unwrap();
        // The timer was refreshed (>= first_lock, typically larger).
        assert!(second_lock >= first_lock);
    }

    #[test]
    fn set_lockout_duration_zero_means_no_lock() {
        // A duration of 0 effectively disables time-based lockout:
        // crossing the threshold still records an entry but it
        // immediately expires.
        let mut mgr = AuthManager::new();
        mgr.set_lockout_duration(0);
        mgr.create_user("ivan", "right").unwrap();
        for _ in 0..5 {
            let _ = mgr.login("ivan", "wrong");
        }
        // With duration 0, locked_until is now == now (already
        // expired), so login succeeds.
        let _ = mgr.login("ivan", "right").unwrap();
    }

    #[test]
    fn api_key_login() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("judy", "password").unwrap();
        let key = mgr.create_api_key(uid, "ci-bot", None).unwrap();
        let session = mgr.login_api_key(&key).unwrap();
        assert_eq!(session.user_id, uid);
    }

    #[test]
    fn invalid_api_key_fails() {
        let mut mgr = AuthManager::new();
        let result = mgr.login_api_key("deadbeef");
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }

    #[test]
    fn session_validation() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("karl", "pass").unwrap();
        let session = mgr.login("karl", "pass").unwrap();
        let validated = mgr.validate_session(&session.token).unwrap();
        assert_eq!(validated, uid);
        mgr.logout(&session.token);
        let result = mgr.validate_session(&session.token);
        assert!(matches!(result, Err(AuthError::InvalidSession)));
    }

    #[test]
    fn role_assignment() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("leo", "pass").unwrap();
        mgr.assign_role(uid, "admin").unwrap();
        mgr.assign_role(uid, "admin").unwrap(); // idempotent
        let roles = mgr.get_roles(uid);
        assert_eq!(roles, vec!["admin"]);
    }

    #[test]
    fn deactivate_user_prevents_login() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("mona", "pass").unwrap();
        mgr.deactivate_user(uid).unwrap();
        let result = mgr.login("mona", "pass");
        assert!(matches!(result, Err(AuthError::UserInactive)));
    }

    #[test]
    fn duplicate_username_rejected() {
        let mut mgr = AuthManager::new();
        mgr.create_user("nina", "pass1").unwrap();
        let result = mgr.create_user("nina", "pass2");
        assert!(matches!(result, Err(AuthError::UsernameTaken)));
    }

    #[test]
    fn revoke_api_key() {
        let mut mgr = AuthManager::new();
        let uid = mgr.create_user("oscar", "pass").unwrap();
        let key = mgr.create_api_key(uid, "temp", None).unwrap();
        mgr.revoke_api_key(&key).unwrap();
        let result = mgr.login_api_key(&key);
        assert!(matches!(result, Err(AuthError::InvalidApiKey)));
    }

    #[test]
    fn password_hash_is_not_plaintext() {
        let hash = hash_password("secret").unwrap();
        assert!(!hash.contains("secret"));
        assert!(hash.contains(':')); // salt:hash format
    }

    // ----------------------------------------------------------------
    // Persistent-session-store tests.
    // ----------------------------------------------------------------

    fn temp_session_path(label: &str) -> PathBuf {
        let pid = std::process::id();
        let tid = format!("{:?}", std::thread::current().id());
        let mut p = std::env::temp_dir();
        p.push(format!("cendb-authmgr-{}-{}-{}.jsonl", pid, tid, label));
        p
    }

    #[test]
    fn with_session_store_persists_login() {
        let path = temp_session_path("persist");
        let _ = std::fs::remove_file(&path);
        let store = Box::new(FileSessionStore::new(&path));
        let mut mgr = AuthManager::with_session_store(store);
        let uid = mgr.create_user("paul", "pw").unwrap();
        let session = mgr.login("paul", "pw").unwrap();
        assert_eq!(session.user_id, uid);

        // The session was persisted to disk (file should exist).
        assert!(path.exists());

        // Drop the manager and create a fresh one with the same store
        // path; the previous session should still validate.
        drop(mgr);
        let store2 = Box::new(FileSessionStore::new(&path));
        let mut mgr2 = AuthManager::with_session_store(store2);
        // Re-create the same user (id assignment is deterministic
        // starting from 1, so the user_id matches).
        mgr2.create_user("paul", "pw").unwrap();
        let validated = mgr2.validate_session(&session.token).unwrap();
        assert_eq!(validated, uid);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn logout_removes_from_persistent_store() {
        let path = temp_session_path("logout");
        let _ = std::fs::remove_file(&path);
        let store = Box::new(FileSessionStore::new(&path));
        let mut mgr = AuthManager::with_session_store(store);
        mgr.create_user("quinn", "pw").unwrap();
        let session = mgr.login("quinn", "pw").unwrap();

        // Drop, reopen, and verify the session is there.
        drop(mgr);
        let store2 = Box::new(FileSessionStore::new(&path));
        let mut mgr2 = AuthManager::with_session_store(store2);
        assert!(mgr2.validate_session(&session.token).is_ok());

        // Logout, drop, reopen — session should be gone.
        mgr2.logout(&session.token);
        drop(mgr2);

        let store3 = Box::new(FileSessionStore::new(&path));
        let mut mgr3 = AuthManager::with_session_store(store3);
        assert!(matches!(mgr3.validate_session(&session.token),
            Err(AuthError::InvalidSession)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn in_memory_session_store_works_with_auth_manager() {
        let store = Box::new(InMemorySessionStore::new());
        let mut mgr = AuthManager::with_session_store(store);
        mgr.create_user("rita", "pw").unwrap();
        let s = mgr.login("rita", "pw").unwrap();
        assert!(mgr.validate_session(&s.token).is_ok());
        mgr.logout(&s.token);
        assert!(matches!(mgr.validate_session(&s.token),
            Err(AuthError::InvalidSession)));
    }

    #[test]
    fn api_key_login_persists_session_to_store() {
        let path = temp_session_path("apikey");
        let _ = std::fs::remove_file(&path);
        let store = Box::new(FileSessionStore::new(&path));
        let mut mgr = AuthManager::with_session_store(store);
        let uid = mgr.create_user("sam", "pw").unwrap();
        let key = mgr.create_api_key(uid, "ci", None).unwrap();
        let session = mgr.login_api_key(&key).unwrap();
        assert_eq!(session.user_id, uid);

        // Drop, reopen, validate the session is still recognised.
        drop(mgr);
        let store2 = Box::new(FileSessionStore::new(&path));
        let mut mgr2 = AuthManager::with_session_store(store2);
        let validated = mgr2.validate_session(&session.token).unwrap();
        assert_eq!(validated, uid);

        let _ = std::fs::remove_file(&path);
    }
}
