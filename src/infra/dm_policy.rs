//! DM policy enforcement with account-scoped allowlists (v2026.2.26).
//!
//! Ported from OpenClaw `src/channels/dm-policy.ts`. Enforces that DM
//! `allowFrom` lists are account-scoped and do NOT inherit from parent
//! configuration to group channel contexts.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ============================================================================
// Types
// ============================================================================

/// DM access policy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Allow DMs from anyone.
    Open,
    /// Only allow DMs from addresses on the allowlist.
    Allowlist,
    /// Block all DMs.
    Block,
}

impl Default for DmPolicy {
    fn default() -> Self {
        Self::Open
    }
}

/// Reason why a DM was allowed or denied.
///
/// Used for audit trails and debugging DM routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmAccessReason {
    /// DM policy is open — all senders allowed.
    PolicyOpen,
    /// Sender is on the explicit allowlist.
    AllowlistMatch,
    /// Sender is on the allowlist via wildcard pattern.
    WildcardMatch,
    /// DM policy blocks all senders.
    PolicyBlock,
    /// Sender is not on the allowlist.
    NotOnAllowlist,
    /// The channel/group context does not inherit parent allowlists.
    NoGroupInheritance,
    /// Account-scoped allowlist matched.
    AccountScopedMatch,
}

/// Result of a DM access check.
#[derive(Debug, Clone)]
pub struct DmAccessResult {
    /// Whether the DM is allowed.
    pub allowed: bool,
    /// Reason for the decision.
    pub reason: DmAccessReason,
    /// The matching allowlist entry, if any.
    pub matched_entry: Option<String>,
    /// Account ID that owns the matching allowlist.
    pub account_id: Option<String>,
}

/// An allowlist source with its owning account context.
#[derive(Debug, Clone)]
pub struct AllowFromSource {
    /// The account ID this allowlist belongs to.
    pub account_id: String,
    /// Allowed sender addresses/patterns.
    pub entries: Vec<String>,
    /// Whether this source is from a group context (no inheritance).
    pub is_group: bool,
}

// ============================================================================
// Core functions
// ============================================================================

/// Merge allowlist entries from multiple sources, respecting account scoping.
///
/// **Key invariant**: Group-context sources do NOT inherit entries from
/// their parent (account-level) configuration. This prevents a user from
/// granting DM access to a group they don't own.
pub fn merge_dm_allow_from_sources(
    sources: &[AllowFromSource],
) -> HashMap<String, Vec<String>> {
    let mut merged: HashMap<String, Vec<String>> = HashMap::new();

    for source in sources {
        if source.is_group {
            // Groups use ONLY their own entries — no inheritance.
            merged.insert(source.account_id.clone(), source.entries.clone());
        } else {
            // Account-level entries merge additively.
            merged
                .entry(source.account_id.clone())
                .or_default()
                .extend(source.entries.iter().cloned());
        }
    }

    // Deduplicate within each account.
    for entries in merged.values_mut() {
        let seen: HashSet<String> = entries.drain(..).collect();
        *entries = seen.into_iter().collect();
        entries.sort();
    }

    merged
}

/// Resolve the effective allowlist for a group context.
///
/// Groups get ONLY explicitly configured entries — they do NOT inherit
/// from the parent account allowlist. This is the key v2026.2.26 security
/// fix that prevents privilege escalation via group DM configuration.
pub fn resolve_group_allow_from_sources(
    group_entries: &[String],
    _parent_entries: &[String],
) -> Vec<String> {
    // Intentionally ignore parent_entries — groups don't inherit.
    group_entries.to_vec()
}

/// Check if a sender address is allowed to send DMs under the given policy.
pub fn check_dm_access(
    sender: &str,
    policy: DmPolicy,
    allow_from: &HashMap<String, Vec<String>>,
    account_id: Option<&str>,
    is_group_context: bool,
) -> DmAccessResult {
    match policy {
        DmPolicy::Open => {
            return DmAccessResult {
                allowed: true,
                reason: DmAccessReason::PolicyOpen,
                matched_entry: None,
                account_id: None,
            };
        }
        DmPolicy::Block => {
            return DmAccessResult {
                allowed: false,
                reason: DmAccessReason::PolicyBlock,
                matched_entry: None,
                account_id: None,
            };
        }
        DmPolicy::Allowlist => {
            // Proceed to allowlist check below.
        }
    }

    // In group context, explicitly block inheritance.
    if is_group_context {
        // Only check entries explicitly configured for this group.
        if let Some(acct) = account_id {
            if let Some(entries) = allow_from.get(acct) {
                if let Some(matched) = matches_allowlist(sender, entries) {
                    return DmAccessResult {
                        allowed: true,
                        reason: DmAccessReason::AccountScopedMatch,
                        matched_entry: Some(matched),
                        account_id: Some(acct.to_string()),
                    };
                }
            }
        }
        return DmAccessResult {
            allowed: false,
            reason: DmAccessReason::NoGroupInheritance,
            matched_entry: None,
            account_id: account_id.map(String::from),
        };
    }

    // Account-scoped allowlist check.
    if let Some(acct) = account_id {
        if let Some(entries) = allow_from.get(acct) {
            if let Some(matched) = matches_allowlist(sender, entries) {
                return DmAccessResult {
                    allowed: true,
                    reason: DmAccessReason::AccountScopedMatch,
                    matched_entry: Some(matched),
                    account_id: Some(acct.to_string()),
                };
            }
        }
    }

    // Check all accounts (for non-scoped lookups).
    for (acct_id, entries) in allow_from {
        if let Some(matched) = matches_allowlist(sender, entries) {
            return DmAccessResult {
                allowed: true,
                reason: DmAccessReason::AllowlistMatch,
                matched_entry: Some(matched),
                account_id: Some(acct_id.clone()),
            };
        }
    }

    DmAccessResult {
        allowed: false,
        reason: DmAccessReason::NotOnAllowlist,
        matched_entry: None,
        account_id: account_id.map(String::from),
    }
}

/// Simple check: is a given source address allowed by a flat allowlist?
///
/// This is a convenience function for channel implementations that
/// just need a quick "is this user ID on the list?" check without
/// the full policy/account scoping machinery.
pub fn is_source_allowed(allow_from: &[String], source: &str) -> bool {
    if allow_from.is_empty() {
        return true;
    }
    matches_allowlist(source, allow_from).is_some()
}

/// Check if a sender matches any entry in an allowlist.
///
/// Supports exact matches and wildcard patterns:
/// - `*` — matches all senders
/// - `*@domain.com` — matches any sender at domain.com
/// - `user@*` — matches user at any domain
fn matches_allowlist(sender: &str, entries: &[String]) -> Option<String> {
    let sender_lower = sender.to_ascii_lowercase();

    for entry in entries {
        let pattern_lower = entry.to_ascii_lowercase();

        if pattern_lower == "*" {
            return Some(entry.clone());
        }

        if pattern_lower == sender_lower {
            return Some(entry.clone());
        }

        // Wildcard prefix: *@domain.com
        if let Some(suffix) = pattern_lower.strip_prefix('*') {
            if sender_lower.ends_with(suffix) {
                return Some(entry.clone());
            }
        }

        // Wildcard suffix: user@*
        if let Some(prefix) = pattern_lower.strip_suffix('*') {
            if sender_lower.starts_with(prefix) {
                return Some(entry.clone());
            }
        }
    }

    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // DmPolicy defaults
    // ====================================================================

    #[test]
    fn default_policy_is_open() {
        assert_eq!(DmPolicy::default(), DmPolicy::Open);
    }

    // ====================================================================
    // check_dm_access — open policy
    // ====================================================================

    #[test]
    fn open_policy_allows_all() {
        let result = check_dm_access(
            "anyone@example.com",
            DmPolicy::Open,
            &HashMap::new(),
            None,
            false,
        );
        assert!(result.allowed);
        assert_eq!(result.reason, DmAccessReason::PolicyOpen);
    }

    // ====================================================================
    // check_dm_access — block policy
    // ====================================================================

    #[test]
    fn block_policy_denies_all() {
        let result = check_dm_access(
            "anyone@example.com",
            DmPolicy::Block,
            &HashMap::new(),
            None,
            false,
        );
        assert!(!result.allowed);
        assert_eq!(result.reason, DmAccessReason::PolicyBlock);
    }

    // ====================================================================
    // check_dm_access — allowlist policy
    // ====================================================================

    #[test]
    fn allowlist_exact_match() {
        let mut allow = HashMap::new();
        allow.insert(
            "acct1".to_string(),
            vec!["user@example.com".to_string()],
        );

        let result = check_dm_access(
            "user@example.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(result.allowed);
        assert_eq!(result.reason, DmAccessReason::AccountScopedMatch);
    }

    #[test]
    fn allowlist_wildcard_all() {
        let mut allow = HashMap::new();
        allow.insert("acct1".to_string(), vec!["*".to_string()]);

        let result = check_dm_access(
            "anyone@anywhere.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(result.allowed);
    }

    #[test]
    fn allowlist_domain_wildcard() {
        let mut allow = HashMap::new();
        allow.insert(
            "acct1".to_string(),
            vec!["*@mycompany.com".to_string()],
        );

        let result = check_dm_access(
            "employee@mycompany.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(result.allowed);

        let result2 = check_dm_access(
            "outsider@other.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(!result2.allowed);
    }

    #[test]
    fn allowlist_not_found() {
        let mut allow = HashMap::new();
        allow.insert(
            "acct1".to_string(),
            vec!["allowed@example.com".to_string()],
        );

        let result = check_dm_access(
            "denied@example.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(!result.allowed);
        assert_eq!(result.reason, DmAccessReason::NotOnAllowlist);
    }

    #[test]
    fn allowlist_case_insensitive() {
        let mut allow = HashMap::new();
        allow.insert(
            "acct1".to_string(),
            vec!["User@Example.COM".to_string()],
        );

        let result = check_dm_access(
            "user@example.com",
            DmPolicy::Allowlist,
            &allow,
            Some("acct1"),
            false,
        );
        assert!(result.allowed);
    }

    // ====================================================================
    // Group context — NO inheritance
    // ====================================================================

    #[test]
    fn group_context_does_not_inherit() {
        let mut allow = HashMap::new();
        // Parent account has broad allowlist
        allow.insert(
            "parent-acct".to_string(),
            vec!["*".to_string()],
        );
        // Group has no explicit entries
        allow.insert("group-acct".to_string(), vec![]);

        let result = check_dm_access(
            "anyone@example.com",
            DmPolicy::Allowlist,
            &allow,
            Some("group-acct"),
            true, // is_group_context
        );
        assert!(!result.allowed);
        assert_eq!(result.reason, DmAccessReason::NoGroupInheritance);
    }

    #[test]
    fn group_context_uses_own_entries() {
        let mut allow = HashMap::new();
        allow.insert(
            "group-acct".to_string(),
            vec!["trusted@example.com".to_string()],
        );

        let result = check_dm_access(
            "trusted@example.com",
            DmPolicy::Allowlist,
            &allow,
            Some("group-acct"),
            true,
        );
        assert!(result.allowed);
        assert_eq!(result.reason, DmAccessReason::AccountScopedMatch);
    }

    // ====================================================================
    // merge_dm_allow_from_sources
    // ====================================================================

    #[test]
    fn merge_deduplicates_entries() {
        let sources = vec![
            AllowFromSource {
                account_id: "acct1".into(),
                entries: vec!["a@b.com".into(), "c@d.com".into()],
                is_group: false,
            },
            AllowFromSource {
                account_id: "acct1".into(),
                entries: vec!["a@b.com".into(), "e@f.com".into()],
                is_group: false,
            },
        ];
        let merged = merge_dm_allow_from_sources(&sources);
        let entries = merged.get("acct1").unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn merge_group_replaces_parent() {
        let sources = vec![
            AllowFromSource {
                account_id: "grp1".into(),
                entries: vec!["x@y.com".into()],
                is_group: false,
            },
            AllowFromSource {
                account_id: "grp1".into(),
                entries: vec!["z@w.com".into()],
                is_group: true, // Group overwrites
            },
        ];
        let merged = merge_dm_allow_from_sources(&sources);
        let entries = merged.get("grp1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries.contains(&"z@w.com".to_string()));
    }

    // ====================================================================
    // resolve_group_allow_from_sources
    // ====================================================================

    #[test]
    fn group_sources_ignore_parent() {
        let parent = vec!["parent@example.com".to_string()];
        let group = vec!["group@example.com".to_string()];
        let resolved = resolve_group_allow_from_sources(&group, &parent);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], "group@example.com");
    }

    // ====================================================================
    // Serialization
    // ====================================================================

    #[test]
    fn dm_policy_serialization() {
        assert_eq!(
            serde_json::to_string(&DmPolicy::Open).unwrap(),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&DmPolicy::Block).unwrap(),
            "\"block\""
        );
        assert_eq!(
            serde_json::to_string(&DmPolicy::Allowlist).unwrap(),
            "\"allowlist\""
        );
    }

    #[test]
    fn dm_access_reason_serialization() {
        assert_eq!(
            serde_json::to_string(&DmAccessReason::NoGroupInheritance).unwrap(),
            "\"no_group_inheritance\""
        );
    }
}
