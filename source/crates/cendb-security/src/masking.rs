//! Column-level data masking.
//!
//! Masks sensitive columns (e.g. SSN, email, credit card) for specific
//! users or roles. Masking rules can be:
//!   * `Full` — replace the entire value with `****`.
//!   * `Partial` — show first/last N characters, mask the rest.
//!   * `Hash` — replace with a hash of the value (deterministic).
//!   * `Null` — replace with NULL.

/// Type of masking to apply.
#[derive(Clone, Debug)]
pub enum ColumnMask {
    /// Replace the entire value with asterisks.
    Full,
    /// Show the first `prefix` and last `suffix` characters, mask the rest.
    Partial { prefix: usize, suffix: usize },
    /// Replace with a BLAKE3 hash of the value (deterministic, not reversible).
    Hash,
    /// Replace with NULL.
    Null,
}

impl ColumnMask {
    /// Apply the mask to a byte value.
    pub fn mask(&self, value: &[u8]) -> Vec<u8> {
        match self {
            ColumnMask::Full => b"*".repeat(value.len().max(1)),
            ColumnMask::Partial { prefix, suffix } => {
                if value.len() <= prefix + suffix {
                    return b"*".repeat(value.len());
                }
                let mut result = Vec::with_capacity(value.len());
                result.extend_from_slice(&value[..*prefix]);
                result.extend(b"*".repeat(value.len() - prefix - suffix));
                result.extend_from_slice(&value[value.len() - *suffix..]);
                result
            }
            ColumnMask::Hash => {
                let h = blake3::hash(value);
                h.to_hex().as_str().as_bytes().to_vec()
            }
            ColumnMask::Null => Vec::new(),
        }
    }

    /// Apply the mask to a string value.
    pub fn mask_string(&self, value: &str) -> String {
        String::from_utf8_lossy(&self.mask(value.as_bytes())).into_owned()
    }
}

/// A masking rule: maps a column name to a mask.
#[derive(Clone, Debug)]
pub struct MaskingRule {
    pub column_name: String,
    pub mask: ColumnMask,
    /// Only apply to these roles (empty = all roles).
    pub roles: Vec<String>,
}

impl MaskingRule {
    pub fn new(column: impl Into<String>, mask: ColumnMask) -> Self {
        Self {
            column_name: column.into(),
            mask,
            roles: Vec::new(),
        }
    }

    pub fn for_roles(mut self, roles: &[&str]) -> Self {
        self.roles = roles.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Check if this rule applies to the given role.
    pub fn applies_to(&self, role: &str) -> bool {
        self.roles.is_empty() || self.roles.iter().any(|r| r == role)
    }
}

/// A masking policy: a collection of rules.
#[derive(Clone, Debug, Default)]
pub struct MaskingPolicy {
    pub rules: Vec<MaskingRule>,
}

impl MaskingPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a masking rule.
    pub fn add_rule(&mut self, rule: MaskingRule) {
        self.rules.push(rule);
    }

    /// Mask a column value for a given role. Returns the masked value,
    /// or the original value if no rule applies.
    pub fn mask_column(&self, column: &str, value: &[u8], role: &str) -> Vec<u8> {
        for rule in &self.rules {
            if rule.column_name == column && rule.applies_to(role) {
                return rule.mask.mask(value);
            }
        }
        value.to_vec()
    }

    /// Convenience: mask a string value.
    pub fn mask_string(&self, column: &str, value: &str, role: &str) -> String {
        String::from_utf8_lossy(&self.mask_column(column, value.as_bytes(), role)).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_mask() {
        let mask = ColumnMask::Full;
        assert_eq!(mask.mask_string("alice"), "*****");
        assert_eq!(mask.mask_string("bob"), "***");
    }

    #[test]
    fn partial_mask() {
        let mask = ColumnMask::Partial { prefix: 2, suffix: 2 };
        // "alice@example.com" is 17 chars → 2 + 13 stars + 2 = "al*************om"
        assert_eq!(mask.mask_string("alice@example.com"), "al*************om");
        assert_eq!(mask.mask_string("ab"), "**"); // Too short for prefix+suffix.
    }

    #[test]
    fn hash_mask() {
        let mask = ColumnMask::Hash;
        let h1 = mask.mask_string("alice");
        let h2 = mask.mask_string("alice");
        let h3 = mask.mask_string("bob");
        assert_eq!(h1, h2); // Deterministic.
        assert_ne!(h1, h3); // Different inputs.
    }

    #[test]
    fn null_mask() {
        let mask = ColumnMask::Null;
        assert_eq!(mask.mask_string("sensitive"), "");
    }

    #[test]
    fn policy_applies_rules_by_role() {
        let mut policy = MaskingPolicy::new();
        policy.add_rule(
            MaskingRule::new("ssn", ColumnMask::Full).for_roles(&["analyst"]),
        );
        policy.add_rule(
            MaskingRule::new("ssn", ColumnMask::Partial { prefix: 3, suffix: 4 }),
        );

        // Analyst sees fully masked SSN.
        let analyst_view = policy.mask_string("ssn", "123-45-6789", "analyst");
        assert_eq!(analyst_view, "***********");

        // Admin sees partially masked SSN (fallback rule).
        // "123-45-6789" is 11 chars, prefix=3, suffix=4 → 4 stars masked.
        let admin_view = policy.mask_string("ssn", "123-45-6789", "admin");
        assert_eq!(admin_view, "123****6789");

        // Non-sensitive column is not masked.
        let name_view = policy.mask_string("name", "Alice", "analyst");
        assert_eq!(name_view, "Alice");
    }
}
