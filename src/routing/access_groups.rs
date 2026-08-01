//! Reusable message-channel access groups (`accessGroup:<name>` allowlist
//! entries).
//!
//! Ported from OpenClaw `src/channels/allow-from.ts`,
//! `src/channels/allowlist-match.ts`, and
//! `src/channels/message-access/runtime-access-groups.ts` (v2026.5.x–7.1):
//!
//! - `accessGroup:<name>` entries in any channel `allowFrom` /
//!   `groupAllowFrom` list reference a reusable group configured at
//!   `accessGroups.<name>` (config root).
//! - Static `message.senders` groups expand to sender ids during allowlist
//!   normalization; unknown or dynamic groups stay symbolic and yield
//!   `not-matched` (never an implicit allow).
//! - Access-group entries are evaluated **before** numeric/direct id checks
//!   (v2026.7.1 Routing/Auth row "accessGroup:* before numeric checks").
//! - Group-name matching is exact: group `team` never matches `team-ext`
//!   (parent-group false-positive fix).
//! - Malformed paired lists (nested arrays / numbers / blank strings) are
//!   normalized to flat trimmed string entries.
//! - Channel allowlists match **bare runtime channel ids**: `C0123` matches
//!   `channel:C0123` / `#C0123` decorations and vice versa.
//!
//! The Discord channel cluster codes against
//! `resolve(name: &str) -> Option<Vec<String>>`; that contract is preserved.

use crate::config::{AccessGroupConfig, Config};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::HashMap;

/// Prefix that marks an allowFrom entry as an access-group reference.
pub const ACCESS_GROUP_ALLOW_FROM_PREFIX: &str = "accessGroup:";

/// Static access-group type whose members are plain sender ids.
pub const ACCESS_GROUP_TYPE_MESSAGE_SENDERS: &str = "message.senders";

// ============================================================================
// Installed group registry
// ============================================================================

/// Process-wide installed access groups (name → static member sender ids).
///
/// Populated from config at gateway startup via [`install_access_groups`].
/// Dynamic (non-`message.senders`) groups are not stored here — they resolve
/// through runtime membership hooks upstream and stay symbolic in this port.
static INSTALLED_GROUPS: Lazy<RwLock<HashMap<String, Vec<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Install static access groups from the loaded config.
///
/// Only `message.senders` (or untyped, which defaults to `message.senders`)
/// groups are installed; dynamic group types are skipped so lookups yield
/// `None` (⇒ treated as non-matching by callers).
pub fn install_access_groups(config: &Config) {
    let map = config
        .access_groups
        .as_ref()
        .map(build_static_group_map)
        .unwrap_or_default();
    *INSTALLED_GROUPS.write() = map;
}

/// Build the static group map from raw config entries (pure; testable).
pub fn build_static_group_map(
    groups: &HashMap<String, AccessGroupConfig>,
) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    for (name, group) in groups {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let group_type = group
            .group_type
            .as_deref()
            .unwrap_or(ACCESS_GROUP_TYPE_MESSAGE_SENDERS);
        if group_type != ACCESS_GROUP_TYPE_MESSAGE_SENDERS {
            // Dynamic group types resolve via runtime membership hooks only.
            continue;
        }
        let raw: Vec<serde_json::Value> = group
            .senders
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        map.insert(name.to_string(), normalize_string_entries(&raw));
    }
    map
}

/// Clear installed groups (test isolation).
pub fn clear_access_groups_for_tests() {
    INSTALLED_GROUPS.write().clear();
}

/// Install a raw name → members map directly (startup wiring and tests).
pub fn install_access_group_map(map: HashMap<String, Vec<String>>) {
    *INSTALLED_GROUPS.write() = map;
}

/// Resolve an access group by name to its member sender ids.
///
/// `Some(member_ids)` expands the group into direct sender ids; `None` means
/// unknown/dynamic/unresolvable — callers must treat the symbolic entry as
/// non-matching (mirrors upstream `not-matched`/`failed` facts, never an
/// implicit allow).
pub fn resolve(name: &str) -> Option<Vec<String>> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    INSTALLED_GROUPS.read().get(name).cloned()
}

// ============================================================================
// Entry parsing / normalization
// ============================================================================

/// Parse an `accessGroup:<name>` allowFrom entry, returning the group name.
pub fn parse_access_group_entry(entry: &str) -> Option<&str> {
    let trimmed = entry.trim();
    let rest = trimmed.strip_prefix(ACCESS_GROUP_ALLOW_FROM_PREFIX)?;
    let name = rest.trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Every access-group name referenced across grouped allowFrom entry arrays.
pub fn all_referenced_access_group_names(entry_groups: &[&[String]]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for group in entry_groups {
        for entry in group.iter() {
            if let Some(name) = parse_access_group_entry(entry) {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Normalize raw allowFrom entries: flatten nested arrays (malformed paired
/// lists), stringify numbers, trim, drop blanks, dedupe preserving order.
pub fn normalize_string_entries(entries: &[serde_json::Value]) -> Vec<String> {
    fn push_value(out: &mut Vec<String>, value: &serde_json::Value) {
        match value {
            serde_json::Value::String(s) => {
                let t = s.trim();
                if !t.is_empty() && !out.iter().any(|e| e == t) {
                    out.push(t.to_string());
                }
            }
            serde_json::Value::Number(n) => {
                let t = n.to_string();
                if !out.iter().any(|e| e == &t) {
                    out.push(t);
                }
            }
            // Malformed paired lists: flatten entries that are themselves lists.
            serde_json::Value::Array(items) => {
                for item in items {
                    push_value(out, item);
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        push_value(&mut out, entry);
    }
    out
}

// ============================================================================
// Allowlist matching
// ============================================================================

/// How an allowlist entry matched (diagnostics parity with upstream
/// `AllowlistMatchSource`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowMatch {
    /// Wildcard `*` entry.
    Wildcard,
    /// Access group matched the sender.
    AccessGroup { group: String },
    /// Direct sender-id entry.
    Id { key: String },
    /// Sender display-name entry (only when name matching enabled).
    Name { key: String },
    /// No entry matched.
    NotMatched,
}

impl AllowMatch {
    pub fn allowed(&self) -> bool {
        !matches!(self, AllowMatch::NotMatched)
    }
}

/// Check a sender against an allowlist that may contain `accessGroup:` refs.
///
/// Evaluation order (upstream v2026.7.1): wildcard, then **access-group
/// entries before numeric/direct checks**, then direct id (case-insensitive),
/// then optional display-name matching.
///
/// Empty allowlists return `NotMatched` — empty-list policy (allow-when-empty
/// for DMs etc.) is the caller's decision, mirroring `isSenderIdAllowed`'s
/// `allowWhenEmpty` parameter.
pub fn match_sender(
    allowlist: &[String],
    sender_id: &str,
    sender_name: Option<&str>,
    allow_name_matching: bool,
) -> AllowMatch {
    if allowlist.is_empty() {
        return AllowMatch::NotMatched;
    }
    if allowlist.iter().any(|e| e.trim() == "*") {
        return AllowMatch::Wildcard;
    }

    let sender_id_lower = sender_id.trim().to_lowercase();

    // Access-group entries first.
    for entry in allowlist {
        if let Some(group_name) = parse_access_group_entry(entry) {
            if let Some(members) = resolve(group_name) {
                if members
                    .iter()
                    .any(|m| m.trim().to_lowercase() == sender_id_lower)
                {
                    return AllowMatch::AccessGroup {
                        group: group_name.to_string(),
                    };
                }
            }
            // Unknown/dynamic groups: non-matching, keep scanning.
        }
    }

    // Direct id checks.
    if !sender_id_lower.is_empty() {
        for entry in allowlist {
            if parse_access_group_entry(entry).is_some() {
                continue;
            }
            if entry.trim().to_lowercase() == sender_id_lower {
                return AllowMatch::Id {
                    key: entry.trim().to_string(),
                };
            }
        }
    }

    // Optional display-name matching.
    if allow_name_matching {
        if let Some(name) = sender_name {
            let name_lower = name.trim().to_lowercase();
            if !name_lower.is_empty() {
                for entry in allowlist {
                    if parse_access_group_entry(entry).is_some() {
                        continue;
                    }
                    if entry.trim().to_lowercase() == name_lower {
                        return AllowMatch::Name {
                            key: entry.trim().to_string(),
                        };
                    }
                }
            }
        }
    }

    AllowMatch::NotMatched
}

/// Wildcard/empty-list policy wrapper (upstream `isSenderIdAllowed`).
pub fn is_sender_allowed(allowlist: &[String], sender_id: &str, allow_when_empty: bool) -> bool {
    if allowlist.is_empty() {
        return allow_when_empty;
    }
    match_sender(allowlist, sender_id, None, false).allowed()
}

// ============================================================================
// Bare runtime channel-id matching
// ============================================================================

/// Strip channel-target decorations from a runtime channel id.
///
/// `channel:C0123`, `#C0123`, and `C0123` all normalize to `c0123`.
fn normalize_channel_id_key(value: &str) -> String {
    let t = value.trim();
    let lower = t.to_lowercase();
    let t = if lower.starts_with("channel:") {
        &t["channel:".len()..]
    } else {
        t
    };
    let t = t.strip_prefix('#').unwrap_or(t);
    t.trim().to_lowercase()
}

/// Match a channel allowlist against a **bare runtime channel id**.
///
/// Allowlist entries and runtime ids match regardless of `channel:`/`#`
/// decoration on either side (v2026.5.2 Routing row "Channel allowlist
/// matching against bare runtime channel IDs"). Wildcard `*` allows all.
pub fn allowlist_matches_channel_id(allowlist: &[String], runtime_channel_id: &str) -> bool {
    if allowlist.iter().any(|e| e.trim() == "*") {
        return true;
    }
    let key = normalize_channel_id_key(runtime_channel_id);
    if key.is_empty() {
        return false;
    }
    allowlist
        .iter()
        .any(|entry| normalize_channel_id_key(entry) == key)
}

// ============================================================================
// Group-session target hygiene
// ============================================================================

/// True when an allowlist entry is a direct-only target (a DM/user ref) that
/// must be kept out of group-session allowlists (v2026.7.1 Routing row
/// "direct-only targets kept out of group sessions").
pub fn is_direct_only_entry(entry: &str) -> bool {
    let t = entry.trim().to_lowercase();
    t.starts_with("user:") || t.starts_with("dm:") || t.starts_with('@')
}

/// Filter an allowlist for use in group-session contexts: direct-only targets
/// are dropped (they can never legitimately match a group conversation).
pub fn filter_group_session_entries(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| !is_direct_only_entry(e))
        .cloned()
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Serializes tests that touch the process-global installed-group map.
    static TEST_LOCK: Lazy<parking_lot::Mutex<()>> = Lazy::new(|| parking_lot::Mutex::new(()));

    fn install(groups: &[(&str, &[&str])]) -> parking_lot::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock();
        let mut map = HashMap::new();
        for (name, members) in groups {
            map.insert(
                name.to_string(),
                members.iter().map(|m| m.to_string()).collect(),
            );
        }
        install_access_group_map(map);
        guard
    }

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_entry_basics() {
        assert_eq!(parse_access_group_entry("accessGroup:family"), Some("family"));
        assert_eq!(parse_access_group_entry("  accessGroup: family "), Some("family"));
        assert_eq!(parse_access_group_entry("accessGroup:"), None);
        assert_eq!(parse_access_group_entry("12345"), None);
        assert_eq!(parse_access_group_entry("group:family"), None);
    }

    #[test]
    fn build_static_map_skips_dynamic_groups() {
        let mut groups = HashMap::new();
        groups.insert(
            "static".to_string(),
            AccessGroupConfig {
                group_type: None,
                senders: Some(vec!["1".into(), " 2 ".into(), "".into()]),
            },
        );
        groups.insert(
            "dyn".to_string(),
            AccessGroupConfig {
                group_type: Some("guild.role".into()),
                senders: Some(vec!["x".into()]),
            },
        );
        let map = build_static_group_map(&groups);
        assert_eq!(
            map.get("static"),
            Some(&vec!["1".to_string(), "2".to_string()])
        );
        assert!(!map.contains_key("dyn"));
        // Exact-name lookups only: parent-group false positive fix.
        assert!(!map.contains_key("stat"));
    }

    #[test]
    fn match_sender_access_group_before_direct() {
        let _g = install(&[("vip", &["999"])]);
        let allow = list(&["accessGroup:vip", "999"]);
        // Group entry wins (evaluated before direct/numeric checks).
        assert_eq!(
            match_sender(&allow, "999", None, false),
            AllowMatch::AccessGroup { group: "vip".into() }
        );
        // Exact group-name resolution: no prefix matches.
        assert_eq!(resolve("vi"), None);
        assert_eq!(resolve("vip-ext"), None);
    }

    #[test]
    fn match_sender_unresolved_group_not_allowed() {
        let _g = install(&[]);
        let allow = list(&["accessGroup:ghost"]);
        assert!(!match_sender(&allow, "1", None, false).allowed());
        // Unresolved group never falls back to matching the literal entry.
        assert!(!match_sender(&allow, "accessgroup:ghost", None, false).allowed());
    }

    #[test]
    fn match_sender_direct_and_wildcard() {
        let _g = install(&[]);
        assert!(match_sender(&list(&["*"]), "anyone", None, false).allowed());
        assert_eq!(
            match_sender(&list(&["ABC"]), "abc", None, false),
            AllowMatch::Id { key: "ABC".into() }
        );
        assert!(!match_sender(&list(&["abc"]), "abcd", None, false).allowed());
        // Empty list: NotMatched (empty policy is the caller's).
        assert!(!match_sender(&[], "abc", None, false).allowed());
        assert!(is_sender_allowed(&[], "abc", true));
        assert!(!is_sender_allowed(&[], "abc", false));
    }

    #[test]
    fn match_sender_name_matching_opt_in() {
        let _g = install(&[]);
        let allow = list(&["Alice"]);
        assert!(!match_sender(&allow, "123", Some("alice"), false).allowed());
        assert_eq!(
            match_sender(&allow, "123", Some("alice"), true),
            AllowMatch::Name { key: "Alice".into() }
        );
    }

    #[test]
    fn normalize_entries_flattens_malformed_pairs() {
        let entries = vec![
            json!("  111 "),
            json!(222),
            json!(["333", ["444"], 555]),
            json!(""),
            json!(null),
            json!("111"),
        ];
        assert_eq!(
            normalize_string_entries(&entries),
            vec!["111", "222", "333", "444", "555"]
        );
    }

    #[test]
    fn referenced_group_names_unique() {
        let a = list(&["accessGroup:one", "123"]);
        let b = list(&["accessGroup:two", "accessGroup:one"]);
        let names = all_referenced_access_group_names(&[&a, &b]);
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn bare_channel_id_matching() {
        let allow = list(&["C0123", "#general", "channel:C9"]);
        assert!(allowlist_matches_channel_id(&allow, "C0123"));
        assert!(allowlist_matches_channel_id(&allow, "channel:C0123"));
        assert!(allowlist_matches_channel_id(&allow, "#c0123"));
        assert!(allowlist_matches_channel_id(&allow, "general"));
        assert!(allowlist_matches_channel_id(&allow, "C9"));
        assert!(!allowlist_matches_channel_id(&allow, "C01234"));
        assert!(allowlist_matches_channel_id(&list(&["*"]), "anything"));
        assert!(!allowlist_matches_channel_id(&allow, ""));
    }

    #[test]
    fn direct_only_entries_filtered_from_group_sessions() {
        let entries = list(&["user:U1", "@alice", "dm:5", "C123", "accessGroup:g"]);
        assert_eq!(
            filter_group_session_entries(&entries),
            vec!["C123".to_string(), "accessGroup:g".to_string()]
        );
    }
}
