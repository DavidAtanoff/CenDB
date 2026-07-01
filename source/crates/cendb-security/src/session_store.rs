//! Persistent session storage.
//!
//! ## Overview
//!
//! The [`crate::auth::AuthManager`] holds active sessions in a
//! `HashMap<TokenString, Session>` in process memory. That is fast but
//! ephemeral: a process restart logs every user out. For deployments
//! where session continuity across restarts matters (long-running
//! services, graceful deploys, etc.), this module adds a pluggable
//! session-store abstraction.
//!
//! The module defines:
//!
//!   * [`SessionStore`] — a trait with four operations:
//!     `save`, `load`, `delete`, `delete_expired`.
//!   * [`InMemorySessionStore`] — a `RwLock<HashMap>` backed store;
//!     this is the behaviour `AuthManager` already had in-memory.
//!   * [`FileSessionStore`] — persists sessions to a JSONL (JSON Lines)
//!     file on disk, one session per line. `save` appends a line,
//!     `load` reads the file and finds the matching token,
//!     `delete` rewrites the file without the deleted session, and
//!     `delete_expired` rewrites the file without any session whose
//!     `expires_at` is older than the supplied cutoff.
//!
//! ## JSONL format
//!
//! Each line is a self-contained JSON object:
//!
//! ```json
//! {"token":"abcdef...","user_id":1,"expires_at":1700000000}
//! ```
//!
//! Because the file is appended to, the same token may appear more
//! than once (e.g. if a session is refreshed and re-saved). On `load`
//! we take the last entry that matches; on `delete` and
//! `delete_expired` we compact the file down to one line per surviving
//! session.
//!
//! The on-disk JSON is implemented without `serde_json` (the crate has
//! no serde dependency today, by design — see the spatial crate's
//! self-contained GeoJSON parser for the same pattern). The serialiser
//! is hard-coded for the three-field `Session` shape and properly
//! escapes the token string.
//!
//! ## Threat model
//!
//! **Persistent sessions are plaintext on disk.** Anyone with read
//! access to the session file can impersonate any user whose session
//! is stored there. Production deployments should either:
//!
//!   * Place the session file on an encrypted filesystem (LUKS,
//!     eCryptfs, FileVault), or
//!   * Wrap [`FileSessionStore`] in an encrypting decorator that
//!     AES-GCM / XChaCha20-Poly1305 encrypts each line, or
//!   * Use a real database-backed session store behind a custom
//!     `SessionStore` impl.
//!
//! The trait abstraction makes any of those a drop-in change.

use cendb_core::{CenError, CenResult};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::auth::Session;

// ============================================================================
// Trait.
// ============================================================================

/// Pluggable session storage.
///
/// All operations are fallible: even the in-memory implementation can
/// return an error if the internal lock is poisoned.
///
/// Implementations must be `Send + Sync` so they can sit behind an
/// `Arc<dyn SessionStore>` shared across threads.
pub trait SessionStore: Send + Sync {
    /// Persist `session`. If a session with the same token already
    /// exists, it is overwritten.
    fn save(&self, session: &Session) -> CenResult<()>;

    /// Load the session with this token, if any (and if it has not
    /// been deleted). Returns `Ok(None)` if the token is unknown.
    fn load(&self, token: &str) -> CenResult<Option<Session>>;

    /// Delete the session with this token. Idempotent — deleting an
    /// unknown token returns `Ok(())`.
    fn delete(&self, token: &str) -> CenResult<()>;

    /// Delete every session whose `expires_at` is strictly less than
    /// `before_ts` (Unix seconds). Returns the number of sessions
    /// removed.
    fn delete_expired(&self, before_ts: u64) -> CenResult<usize>;
}

// ============================================================================
// InMemorySessionStore.
// ============================================================================

/// In-memory session store: a `RwLock<HashMap>` of token → session.
///
/// This is the simplest possible implementation and matches the prior
/// behaviour of `AuthManager` (which held sessions directly in a
/// `HashMap`). Use this for tests or for deployments where session
/// persistence across restarts is not required.
pub struct InMemorySessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn save(&self, session: &Session) -> CenResult<()> {
        let mut g = self
            .sessions
            .write()
            .map_err(|e| CenError::internal(format!("InMemorySessionStore: lock poisoned: {}", e)))?;
        g.insert(session.token.clone(), session.clone());
        Ok(())
    }

    fn load(&self, token: &str) -> CenResult<Option<Session>> {
        let g = self
            .sessions
            .read()
            .map_err(|e| CenError::internal(format!("InMemorySessionStore: lock poisoned: {}", e)))?;
        Ok(g.get(token).cloned())
    }

    fn delete(&self, token: &str) -> CenResult<()> {
        let mut g = self
            .sessions
            .write()
            .map_err(|e| CenError::internal(format!("InMemorySessionStore: lock poisoned: {}", e)))?;
        g.remove(token);
        Ok(())
    }

    fn delete_expired(&self, before_ts: u64) -> CenResult<usize> {
        let mut g = self
            .sessions
            .write()
            .map_err(|e| CenError::internal(format!("InMemorySessionStore: lock poisoned: {}", e)))?;
        let initial = g.len();
        g.retain(|_, s| s.expires_at >= before_ts);
        Ok(initial - g.len())
    }
}

// ============================================================================
// FileSessionStore (JSONL).
// ============================================================================

/// File-backed session store using JSON Lines.
///
/// Each line of the file at `path` is a JSON object representing one
/// `Session`. See the [module docs](self) for the threat-model caveat:
/// sessions are written to disk in plaintext.
pub struct FileSessionStore {
    path: PathBuf,
    /// A process-local lock that serialises file operations. This
    /// makes a single `FileSessionStore` instance safe to share
    /// across threads. (Inter-process safety is the OS's
    /// responsibility — there is no `flock` here.)
    lock: RwLock<()>,
}

impl FileSessionStore {
    /// Open (or create) a session file at `path`. The file is created
    /// on first `save`; `load` against a non-existent file returns
    /// `Ok(None)`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            lock: RwLock::new(()),
        }
    }

    /// Read every session currently in the file. The last entry for a
    /// given token wins (file is append-only; later writes supersede
    /// earlier ones with the same token).
    fn read_all(&self) -> CenResult<HashMap<String, Session>> {
        let _g = self
            .lock
            .read()
            .map_err(|e| CenError::internal(format!("FileSessionStore: lock poisoned: {}", e)))?;
        let mut map: HashMap<String, Session> = HashMap::new();
        if !self.path.exists() {
            return Ok(map);
        }
        let file = File::open(&self.path).map_err(|e| {
            CenError::io(format!(
                "FileSessionStore: cannot open {}: {}",
                self.path.display(),
                e
            ))
        })?;
        let reader = BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                CenError::io(format!(
                    "FileSessionStore: read error at line {}: {}",
                    lineno,
                    e
                ))
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_session_json(trimmed) {
                Ok(s) => {
                    map.insert(s.token.clone(), s);
                }
                Err(e) => {
                    return Err(CenError::corrupt(format!(
                        "FileSessionStore: cannot parse session at line {}: {} (line: {})",
                        lineno, e, trimmed
                    )));
                }
            }
        }
        Ok(map)
    }

    /// Rewrite the file from scratch with the given sessions.
    fn rewrite(&self, sessions: &HashMap<String, Session>) -> CenResult<()> {
        let _g = self
            .lock
            .write()
            .map_err(|e| CenError::internal(format!("FileSessionStore: lock poisoned: {}", e)))?;

        // Write to a sibling temp file then atomically rename, so a
        // crash mid-rewrite leaves either the old file or the new file
        // intact — never a truncated one.
        let tmp_path = self.path.with_extension("session.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| {
                    CenError::io(format!(
                        "FileSessionStore: cannot create temp {}: {}",
                        tmp_path.display(),
                        e
                    ))
                })?;
            for (_, s) in sessions {
                let line = serialize_session_json(s);
                f.write_all(line.as_bytes()).map_err(|e| {
                    CenError::io(format!("FileSessionStore: write error: {}", e))
                })?;
                f.write_all(b"\n").map_err(|e| {
                    CenError::io(format!("FileSessionStore: write error: {}", e))
                })?;
            }
            f.sync_all().map_err(|e| {
                CenError::io(format!("FileSessionStore: fsync error: {}", e))
            })?;
        }
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            CenError::io(format!(
                "FileSessionStore: rename {} -> {} failed: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;
        Ok(())
    }
}

impl SessionStore for FileSessionStore {
    fn save(&self, session: &Session) -> CenResult<()> {
        // We append a new line for save (the read path takes the last
        // matching line). This is O(1) on save and avoids rewriting
        // the whole file on every save. Compaction happens on
        // delete / delete_expired.
        let _g = self
            .lock
            .write()
            .map_err(|e| CenError::internal(format!("FileSessionStore: lock poisoned: {}", e)))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                CenError::io(format!(
                    "FileSessionStore: cannot open for append {}: {}",
                    self.path.display(),
                    e
                ))
            })?;
        let line = serialize_session_json(session);
        f.write_all(line.as_bytes()).map_err(|e| {
            CenError::io(format!("FileSessionStore: write error: {}", e))
        })?;
        f.write_all(b"\n").map_err(|e| {
            CenError::io(format!("FileSessionStore: write error: {}", e))
        })?;
        f.sync_all().map_err(|e| {
            CenError::io(format!("FileSessionStore: fsync error: {}", e))
        })?;
        Ok(())
    }

    fn load(&self, token: &str) -> CenResult<Option<Session>> {
        let all = self.read_all()?;
        Ok(all.get(token).cloned())
    }

    fn delete(&self, token: &str) -> CenResult<()> {
        let mut all = self.read_all()?;
        if all.remove(token).is_some() {
            self.rewrite(&all)
        } else {
            Ok(())
        }
    }

    fn delete_expired(&self, before_ts: u64) -> CenResult<usize> {
        let mut all = self.read_all()?;
        let initial = all.len();
        all.retain(|_, s| s.expires_at >= before_ts);
        let removed = initial - all.len();
        if removed > 0 {
            self.rewrite(&all)?;
        }
        Ok(removed)
    }
}

// ============================================================================
// Minimal JSON serializer / parser for Session.
// ============================================================================

/// Serialise a [`Session`] as a single-line JSON object. The output
/// contains no newlines so it can sit on its own line in a JSONL file.
fn serialize_session_json(s: &Session) -> String {
    let mut out = String::with_capacity(64 + s.token.len());
    out.push('{');
    out.push_str("\"token\":\"");
    out.push_str(&escape_json_string(&s.token));
    out.push_str("\",");
    out.push_str("\"user_id\":");
    out.push_str(&s.user_id.to_string());
    out.push(',');
    out.push_str("\"expires_at\":");
    out.push_str(&s.expires_at.to_string());
    out.push('}');
    out
}

/// Parse a single-line JSON object into a [`Session`].
///
/// Accepts the exact shape produced by [`serialize_session_json`]:
/// `{"token":"...","user_id":N,"expires_at":N}`. Tolerates whitespace
/// between tokens. Rejects unknown fields, missing fields, or
/// malformed JSON.
fn parse_session_json(input: &str) -> CenResult<Session> {
    let mut p = JsonParser::new(input);
    p.skip_ws();
    p.expect_char('{')?;
    p.skip_ws();

    let mut token: Option<String> = None;
    let mut user_id: Option<u64> = None;
    let mut expires_at: Option<u64> = None;

    // Loop until `}`. Each iteration: "key":value, optional comma.
    loop {
        p.skip_ws();
        if p.peek() == Some('}') {
            break;
        }
        let key = p.parse_string()?;
        p.skip_ws();
        p.expect_char(':')?;
        p.skip_ws();
        match key.as_str() {
            "token" => {
                let v = p.parse_string()?;
                token = Some(v);
            }
            "user_id" => {
                let v = p.parse_number()?;
                user_id = Some(v);
            }
            "expires_at" => {
                let v = p.parse_number()?;
                expires_at = Some(v);
            }
            other => {
                return Err(CenError::corrupt(format!(
                    "parse_session_json: unknown field {:?}",
                    other
                )));
            }
        }
        p.skip_ws();
        if p.peek() == Some(',') {
            p.advance();
            p.skip_ws();
        } else if p.peek() == Some('}') {
            break;
        } else {
            return Err(CenError::corrupt(format!(
                "parse_session_json: expected ',' or '}}', got {:?}",
                p.peek()
            )));
        }
    }
    p.expect_char('}')?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(CenError::corrupt(format!(
            "parse_session_json: trailing input after object: {:?}",
            p.rest()
        )));
    }
    let token = token.ok_or_else(|| {
        CenError::corrupt("parse_session_json: missing field \"token\"".to_string())
    })?;
    let user_id = user_id.ok_or_else(|| {
        CenError::corrupt("parse_session_json: missing field \"user_id\"".to_string())
    })?;
    let expires_at = expires_at.ok_or_else(|| {
        CenError::corrupt("parse_session_json: missing field \"expires_at\"".to_string())
    })?;
    Ok(Session {
        token,
        user_id,
        expires_at,
    })
}

/// Escape a string for inclusion in a JSON string literal.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Tiny cursor-based JSON parser, scoped to just what `Session`
/// serialisation needs.
struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.bytes.get(self.pos).map(|b| *b as char)
    }

    fn advance(&mut self) {
        if self.pos < self.bytes.len() {
            self.pos += 1;
        }
    }

    fn rest(&self) -> &str {
        std::str::from_utf8(&self.bytes[self.pos..]).unwrap_or("<bad utf-8>")
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn expect_char(&mut self, c: char) -> CenResult<()> {
        match self.peek() {
            Some(p) if p == c => {
                self.advance();
                Ok(())
            }
            Some(other) => Err(CenError::corrupt(format!(
                "parse_session_json: expected {:?}, got {:?}",
                c, other
            ))),
            None => Err(CenError::corrupt(format!(
                "parse_session_json: expected {:?}, got EOF",
                c
            ))),
        }
    }

    fn parse_string(&mut self) -> CenResult<String> {
        self.expect_char('"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(CenError::corrupt(
                        "parse_session_json: unterminated string".to_string(),
                    ));
                }
                Some('"') => {
                    self.advance();
                    return Ok(out);
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('b') => out.push('\x08'),
                        Some('f') => out.push('\x0c'),
                        Some('u') => {
                            self.advance();
                            // 4 hex digits.
                            let mut code: u32 = 0;
                            for _ in 0..4 {
                                let h = self.peek().ok_or_else(|| {
                                    CenError::corrupt(
                                        "parse_session_json: \\u escape truncated".to_string(),
                                    )
                                })?;
                                let d = h.to_digit(16).ok_or_else(|| {
                                    CenError::corrupt(format!(
                                        "parse_session_json: bad hex digit in \\u escape: {:?}",
                                        h
                                    ))
                                })?;
                                code = code * 16 + d;
                                self.advance();
                            }
                            // For simplicity we only handle the BMP.
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            } else {
                                return Err(CenError::corrupt(format!(
                                    "parse_session_json: bad unicode codepoint U+{:04X}",
                                    code
                                )));
                            }
                            continue; // already advanced past last hex digit
                        }
                        Some(other) => {
                            return Err(CenError::corrupt(format!(
                                "parse_session_json: bad escape \\{:?}",
                                other
                            )))
                        }
                        None => {
                            return Err(CenError::corrupt(
                                "parse_session_json: unterminated escape".to_string(),
                            ))
                        }
                    }
                    self.advance();
                }
                Some(c) => {
                    out.push(c);
                    self.advance();
                }
            }
        }
    }

    fn parse_number(&mut self) -> CenResult<u64> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(CenError::corrupt(format!(
                "parse_session_json: expected number, got {:?}",
                self.peek()
            )));
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        s.parse::<u64>().map_err(|e| {
            CenError::corrupt(format!("parse_session_json: bad number {}: {}", s, e))
        })
    }
}

// Read the file fully into a string; used for nothing currently but
// kept as a small helper for future schema-migration paths.
#[allow(dead_code)]
fn read_file_to_string(path: &Path) -> CenResult<String> {
    let mut f = File::open(path)
        .map_err(|e| CenError::io(format!("cannot open {}: {}", path.display(), e)))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| CenError::io(format!("read {}: {}", path.display(), e)))?;
    Ok(s)
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn make_session(token: &str, user_id: u64, ttl: u64) -> Session {
        Session {
            token: token.to_string(),
            user_id,
            expires_at: now_secs() + ttl,
        }
    }

    // --- InMemorySessionStore ---

    #[test]
    fn in_memory_basic_save_load_delete() {
        let store = InMemorySessionStore::new();
        let s = make_session("tok-A", 1, 3600);
        store.save(&s).unwrap();
        let loaded = store.load("tok-A").unwrap().unwrap();
        assert_eq!(loaded.token, "tok-A");
        assert_eq!(loaded.user_id, 1);

        store.delete("tok-A").unwrap();
        assert!(store.load("tok-A").unwrap().is_none());
    }

    #[test]
    fn in_memory_delete_unknown_is_ok() {
        let store = InMemorySessionStore::new();
        store.delete("never-saved").unwrap();
    }

    #[test]
    fn in_memory_delete_expired_removes_old_sessions() {
        let store = InMemorySessionStore::new();
        let now = now_secs();
        let young = Session { token: "young".into(), user_id: 1, expires_at: now + 1000 };
        let old = Session { token: "old".into(), user_id: 2, expires_at: now - 1000 };
        store.save(&young).unwrap();
        store.save(&old).unwrap();
        let removed = store.delete_expired(now).unwrap();
        assert_eq!(removed, 1);
        assert!(store.load("young").unwrap().is_some());
        assert!(store.load("old").unwrap().is_none());
    }

    #[test]
    fn in_memory_save_overwrites_same_token() {
        let store = InMemorySessionStore::new();
        let s1 = Session { token: "tok".into(), user_id: 1, expires_at: 100 };
        let s2 = Session { token: "tok".into(), user_id: 2, expires_at: 200 };
        store.save(&s1).unwrap();
        store.save(&s2).unwrap();
        let loaded = store.load("tok").unwrap().unwrap();
        assert_eq!(loaded.user_id, 2);
        assert_eq!(loaded.expires_at, 200);
    }

    // --- FileSessionStore ---

    fn temp_path(name: &str) -> PathBuf {
        // Use the per-test thread-id to avoid collisions between
        // parallel test runs.
        let pid = std::process::id();
        let tid = format!("{:?}", std::thread::current().id());
        let mut p = std::env::temp_dir();
        p.push(format!("cendb-session-{}-{}-{}.jsonl", pid, tid, name));
        p
    }

    #[test]
    fn file_store_basic_save_load_delete() {
        let path = temp_path("basic");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);

        let s = make_session("tok-FS-1", 42, 3600);
        store.save(&s).unwrap();
        let loaded = store.load("tok-FS-1").unwrap().unwrap();
        assert_eq!(loaded.token, "tok-FS-1");
        assert_eq!(loaded.user_id, 42);

        store.delete("tok-FS-1").unwrap();
        assert!(store.load("tok-FS-1").unwrap().is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_load_unknown_returns_none() {
        let path = temp_path("unknown");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);
        // File doesn't exist yet.
        assert!(store.load("nope").unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_survives_restart() {
        let path = temp_path("restart");
        let _ = std::fs::remove_file(&path);

        // Phase 1: write a session and drop the store.
        let s = make_session("tok-restart", 99, 3600);
        {
            let store = FileSessionStore::new(&path);
            store.save(&s).unwrap();
            assert_eq!(store.load("tok-restart").unwrap().unwrap().user_id, 99);
        } // store dropped here

        // Phase 2: open a fresh store on the same path and verify the
        // session is still readable.
        {
            let store = FileSessionStore::new(&path);
            let loaded = store.load("tok-restart").unwrap().unwrap();
            assert_eq!(loaded.token, "tok-restart");
            assert_eq!(loaded.user_id, 99);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_save_overwrites_same_token() {
        let path = temp_path("overwrite");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);

        let s1 = Session { token: "tok-ov".into(), user_id: 1, expires_at: 100 };
        let s2 = Session { token: "tok-ov".into(), user_id: 2, expires_at: 200 };
        store.save(&s1).unwrap();
        store.save(&s2).unwrap();

        // The in-memory map view returns the latest entry.
        let loaded = store.load("tok-ov").unwrap().unwrap();
        assert_eq!(loaded.user_id, 2);
        assert_eq!(loaded.expires_at, 200);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_delete_expired_removes_old_sessions() {
        let path = temp_path("expired");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);

        let now = now_secs();
        let young = Session { token: "young".into(), user_id: 1, expires_at: now + 1000 };
        let old = Session { token: "old".into(), user_id: 2, expires_at: now - 1000 };
        store.save(&young).unwrap();
        store.save(&old).unwrap();

        let removed = store.delete_expired(now).unwrap();
        assert_eq!(removed, 1);
        assert!(store.load("young").unwrap().is_some());
        assert!(store.load("old").unwrap().is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_delete_expired_with_no_old_sessions_is_noop() {
        let path = temp_path("noop");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);

        let now = now_secs();
        let young = Session { token: "young".into(), user_id: 1, expires_at: now + 1000 };
        store.save(&young).unwrap();

        let removed = store.delete_expired(now).unwrap();
        assert_eq!(removed, 0);
        assert!(store.load("young").unwrap().is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_multiple_sessions_persist() {
        let path = temp_path("multi");
        let _ = std::fs::remove_file(&path);

        {
            let store = FileSessionStore::new(&path);
            for i in 0..10u64 {
                let s = Session {
                    token: format!("tok-{}", i),
                    user_id: i,
                    expires_at: now_secs() + 3600,
                };
                store.save(&s).unwrap();
            }
        }

        // Reopen and verify all 10 are present.
        {
            let store = FileSessionStore::new(&path);
            for i in 0..10u64 {
                let loaded = store.load(&format!("tok-{}", i)).unwrap().unwrap();
                assert_eq!(loaded.user_id, i);
            }
            // Unknown token still returns None.
            assert!(store.load("tok-X").unwrap().is_none());
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_store_delete_compacts_file() {
        let path = temp_path("compact");
        let _ = std::fs::remove_file(&path);
        let store = FileSessionStore::new(&path);

        // Save the same token 5 times — file will have 5 lines.
        for _ in 0..5 {
            store
                .save(&Session { token: "tok-c".into(), user_id: 1, expires_at: now_secs() + 100 })
                .unwrap();
        }
        // Plus a second token twice.
        for _ in 0..2 {
            store
                .save(&Session { token: "tok-d".into(), user_id: 2, expires_at: now_secs() + 100 })
                .unwrap();
        }

        store.delete("tok-c").unwrap();

        // After deletion + compaction, tok-c should be gone, tok-d
        // should still be present.
        assert!(store.load("tok-c").unwrap().is_none());
        assert!(store.load("tok-d").unwrap().is_some());

        let _ = std::fs::remove_file(&path);
    }

    // --- JSON serialisation round-trip ---

    #[test]
    fn json_round_trip_preserves_all_fields() {
        let s = Session {
            token: "deadbeefcafef00d".to_string(),
            user_id: 12345,
            expires_at: 1700000000,
        };
        let json = serialize_session_json(&s);
        assert!(!json.contains('\n'));
        let back = parse_session_json(&json).unwrap();
        assert_eq!(back.token, s.token);
        assert_eq!(back.user_id, s.user_id);
        assert_eq!(back.expires_at, s.expires_at);
    }

    #[test]
    fn json_round_trip_handles_special_chars_in_token() {
        // Real session tokens are 64-char hex strings, so they won't
        // contain quotes/backslashes, but the parser must still handle
        // them safely (e.g. if someone stores a non-hex token).
        let s = Session {
            token: "with\"quote\\and\nnewline".to_string(),
            user_id: 1,
            expires_at: 1,
        };
        let json = serialize_session_json(&s);
        let back = parse_session_json(&json).unwrap();
        assert_eq!(back.token, s.token);
    }

    #[test]
    fn json_parser_rejects_missing_field() {
        // No user_id.
        let bad = r#"{"token":"x","expires_at":1}"#;
        assert!(parse_session_json(bad).is_err());
    }

    #[test]
    fn json_parser_rejects_unknown_field() {
        let bad = r#"{"token":"x","user_id":1,"expires_at":1,"extra":2}"#;
        assert!(parse_session_json(bad).is_err());
    }

    #[test]
    fn json_parser_rejects_trailing_input() {
        let bad = r#"{"token":"x","user_id":1,"expires_at":1}garbage"#;
        assert!(parse_session_json(bad).is_err());
    }

    #[test]
    fn json_parser_accepts_whitespace_between_tokens() {
        let s = r#"  {  "token"  :  "x"  ,  "user_id"  :  1  ,  "expires_at"  :  2  }  "#;
        let parsed = parse_session_json(s).unwrap();
        assert_eq!(parsed.token, "x");
        assert_eq!(parsed.user_id, 1);
        assert_eq!(parsed.expires_at, 2);
    }

    // --- Trait-object usage ---

    #[test]
    fn session_store_can_be_used_as_trait_object() {
        let path = temp_path("trait");
        let _ = std::fs::remove_file(&path);

        let stores: Vec<Box<dyn SessionStore>> = vec![
            Box::new(InMemorySessionStore::new()),
            Box::new(FileSessionStore::new(&path)),
        ];
        for (i, store) in stores.iter().enumerate() {
            let tok = format!("tok-trait-{}", i);
            let s = Session { token: tok.clone(), user_id: i as u64, expires_at: now_secs() + 100 };
            store.save(&s).unwrap();
            let loaded = store.load(&tok).unwrap().unwrap();
            assert_eq!(loaded.user_id, i as u64);
            store.delete(&tok).unwrap();
            assert!(store.load(&tok).unwrap().is_none());
        }

        let _ = std::fs::remove_file(&path);
    }
}
