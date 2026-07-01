//! Role-Based Access Control (RBAC).
//!
//! ## Model
//!
//! Three-tier permission model:
//!   - **Roles** (e.g. "admin", "read_only", "analyst")
//!   - **Resources** (tables, collections, or namespaces like "users.*")
//!   - **Permissions** (Read, Write, Create, Drop, Admin)
//!
//! A user holds zero or more roles. Each role grants a set of
//! permissions on a set of resources. A user can perform an operation
//! iff *any* of their roles grants the required permission on the
//! target resource.
//!
//! ## Resource matching
//!
//! Resources use glob-style patterns:
//!   - `*` — all resources
//!   - `users.*` — all collections/tables starting with `users.`
//!   - `users` — exactly the `users` resource
//!
//! ## Default roles
//!
//! On `RbacManager::new()`, three default roles are created:
//!   - `admin` — all permissions on all resources
//!   - `read_only` — Read on `*`
//!   - `analyst` — Read on `*`, Write on `analytics.*`

use std::collections::HashMap;

/// A permission bit.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Permission {
    Read,
    Write,
    Create,
    Drop,
    Admin,
}

impl Permission {
    pub fn as_str(&self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write",
            Permission::Create => "create",
            Permission::Drop => "drop",
            Permission::Admin => "admin",
        }
    }
}

/// A role: a named set of (resource_pattern, permission) grants.
#[derive(Clone, Debug)]
pub struct Role {
    pub name: String,
    pub grants: Vec<(String, Permission)>,
}

/// RBAC errors.
#[derive(Debug, Clone)]
pub enum RbacError {
    RoleNotFound,
    RoleAlreadyExists,
    UserNotFound,
    PermissionDenied,
}

impl std::fmt::Display for RbacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RbacError::RoleNotFound => write!(f, "role not found"),
            RbacError::RoleAlreadyExists => write!(f, "role already exists"),
            RbacError::UserNotFound => write!(f, "user not found"),
            RbacError::PermissionDenied => write!(f, "permission denied"),
        }
    }
}

impl std::error::Error for RbacError {}

/// The RBAC manager. Holds role definitions and per-user role
/// assignments. Uses the `AuthManager`'s user IDs.
pub struct RbacManager {
    roles: HashMap<String, Role>,
    /// User ID → list of role names.
    user_roles: HashMap<u64, Vec<String>>,
}

impl RbacManager {
    pub fn new() -> Self {
        let mut mgr = Self {
            roles: HashMap::new(),
            user_roles: HashMap::new(),
        };
        mgr.create_default_roles();
        mgr
    }

    fn create_default_roles(&mut self) {
        // admin: all permissions on everything.
        let admin = Role {
            name: "admin".to_string(),
            grants: vec![
                ("*".to_string(), Permission::Read),
                ("*".to_string(), Permission::Write),
                ("*".to_string(), Permission::Create),
                ("*".to_string(), Permission::Drop),
                ("*".to_string(), Permission::Admin),
            ],
        };
        self.roles.insert("admin".to_string(), admin);

        // read_only: Read on everything.
        let read_only = Role {
            name: "read_only".to_string(),
            grants: vec![("*".to_string(), Permission::Read)],
        };
        self.roles.insert("read_only".to_string(), read_only);

        // analyst: Read on everything, Write only on analytics.
        let analyst = Role {
            name: "analyst".to_string(),
            grants: vec![
                ("*".to_string(), Permission::Read),
                ("analytics.*".to_string(), Permission::Write),
                ("analytics.*".to_string(), Permission::Create),
            ],
        };
        self.roles.insert("analyst".to_string(), analyst);
    }

    /// Create a custom role.
    pub fn create_role(&mut self, role: Role) -> Result<(), RbacError> {
        if self.roles.contains_key(&role.name) {
            return Err(RbacError::RoleAlreadyExists);
        }
        self.roles.insert(role.name.clone(), role);
        Ok(())
    }

    /// Assign a role to a user.
    pub fn assign_role_to_user(&mut self, user_id: u64, role_name: &str) -> Result<(), RbacError> {
        if !self.roles.contains_key(role_name) {
            return Err(RbacError::RoleNotFound);
        }
        let roles = self.user_roles.entry(user_id).or_default();
        if !roles.contains(&role_name.to_string()) {
            roles.push(role_name.to_string());
        }
        Ok(())
    }

    /// Revoke a role from a user.
    pub fn revoke_role_from_user(&mut self, user_id: u64, role_name: &str) -> Result<(), RbacError> {
        let roles = self.user_roles.get_mut(&user_id).ok_or(RbacError::UserNotFound)?;
        roles.retain(|r| r != role_name);
        Ok(())
    }

    /// Check whether a user has a permission on a resource.
    pub fn check(&self, user_id: u64, resource: &str, permission: Permission) -> Result<(), RbacError> {
        let roles = self.user_roles.get(&user_id);
        if let Some(role_names) = roles {
            for role_name in role_names {
                if let Some(role) = self.roles.get(role_name) {
                    for (pattern, perm) in &role.grants {
                        if *perm == permission && glob_match(pattern, resource) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Err(RbacError::PermissionDenied)
    }

    /// List a user's role names.
    pub fn user_roles(&self, user_id: u64) -> Vec<String> {
        self.user_roles.get(&user_id).cloned().unwrap_or_default()
    }

    /// List all role names.
    pub fn role_names(&self) -> Vec<String> {
        self.roles.keys().cloned().collect()
    }
}

impl Default for RbacManager {
    fn default() -> Self { Self::new() }
}

/// Glob-style pattern matching: `*` matches any sequence of characters
/// (including `.`), `?` matches a single character. No other regex
/// metacharacters.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_helper(&p, 0, &t, 0)
}

fn glob_match_helper(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            // Try matching zero or more characters.
            for k in ti..=t.len() {
                if glob_match_helper(p, pi + 1, t, k) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < t.len() {
                glob_match_helper(p, pi + 1, t, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < t.len() && t[ti] == c {
                glob_match_helper(p, pi + 1, t, ti + 1)
            } else {
                false
            }
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
    fn default_roles_exist() {
        let mgr = RbacManager::new();
        let names = mgr.role_names();
        assert!(names.contains(&"admin".to_string()));
        assert!(names.contains(&"read_only".to_string()));
        assert!(names.contains(&"analyst".to_string()));
    }

    #[test]
    fn admin_has_all_permissions() {
        let mut mgr = RbacManager::new();
        mgr.assign_role_to_user(1, "admin").unwrap();
        mgr.check(1, "any_table", Permission::Read).unwrap();
        mgr.check(1, "any_table", Permission::Write).unwrap();
        mgr.check(1, "any_table", Permission::Drop).unwrap();
        mgr.check(1, "any_table", Permission::Admin).unwrap();
    }

    #[test]
    fn read_only_cannot_write() {
        let mut mgr = RbacManager::new();
        mgr.assign_role_to_user(1, "read_only").unwrap();
        mgr.check(1, "any_table", Permission::Read).unwrap();
        let result = mgr.check(1, "any_table", Permission::Write);
        assert!(matches!(result, Err(RbacError::PermissionDenied)));
    }

    #[test]
    fn analyst_can_write_analytics_only() {
        let mut mgr = RbacManager::new();
        mgr.assign_role_to_user(1, "analyst").unwrap();
        // Can read anything.
        mgr.check(1, "users", Permission::Read).unwrap();
        // Can write to analytics.*.
        mgr.check(1, "analytics.events", Permission::Write).unwrap();
        mgr.check(1, "analytics.events", Permission::Create).unwrap();
        // Cannot write to users.
        let result = mgr.check(1, "users", Permission::Write);
        assert!(matches!(result, Err(RbacError::PermissionDenied)));
    }

    #[test]
    fn no_roles_denies_all() {
        let mgr = RbacManager::new();
        let result = mgr.check(1, "any_table", Permission::Read);
        assert!(matches!(result, Err(RbacError::PermissionDenied)));
    }

    #[test]
    fn revoke_role_removes_permissions() {
        let mut mgr = RbacManager::new();
        mgr.assign_role_to_user(1, "admin").unwrap();
        mgr.check(1, "table", Permission::Read).unwrap();
        mgr.revoke_role_from_user(1, "admin").unwrap();
        let result = mgr.check(1, "table", Permission::Read);
        assert!(matches!(result, Err(RbacError::PermissionDenied)));
    }

    #[test]
    fn custom_role() {
        let mut mgr = RbacManager::new();
        let role = Role {
            name: "writer".to_string(),
            grants: vec![
                ("docs.*".to_string(), Permission::Write),
                ("docs.*".to_string(), Permission::Read),
            ],
        };
        mgr.create_role(role).unwrap();
        mgr.assign_role_to_user(1, "writer").unwrap();
        mgr.check(1, "docs.x", Permission::Write).unwrap();
        mgr.check(1, "docs.x", Permission::Read).unwrap();
        let result = mgr.check(1, "docs.x", Permission::Drop);
        assert!(matches!(result, Err(RbacError::PermissionDenied)));
    }

    #[test]
    fn duplicate_role_rejected() {
        let mut mgr = RbacManager::new();
        let role = Role { name: "admin".to_string(), grants: vec![] };
        let result = mgr.create_role(role);
        assert!(matches!(result, Err(RbacError::RoleAlreadyExists)));
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("users.*", "users.table1"));
        assert!(glob_match("users.*", "users."));
        assert!(!glob_match("users.*", "other.table1"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "other"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }
}
