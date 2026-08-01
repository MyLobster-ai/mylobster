//! Discord routing behavior (v2026.7.1).
//!
//! Ports of OpenClaw thread-binding persistence (SQLite), the cross-provider
//! guild-admin action block (`trusted-requester-actions.ts`), PluralKit sender
//! identity for DM pairing (`monitor/sender-identity.ts`), implicit-reply
//! fanout limiting (`reply-reference.ts`), the alpha-bucket model picker
//! (`monitor/model-picker.state.ts`), the agent-components registry TTL, and
//! the `suppressEmbeds`-by-default outbound flags
//! (`send.message-request.ts` / `send.outbound.ts`).
//!
//! Bundled-native port; upstream ships these inside the Discord npm plugin.

use crate::config::ReplyToMode;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

// ============================================================================
// Thread bindings persisted in SQLite, routed to plugin owners
// ============================================================================

/// A persisted Discord thread binding: a thread pinned to a session and an
/// owning handler (e.g. a plugin id) that inbound thread messages route to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordThreadBinding {
    pub account_id: String,
    pub thread_id: String,
    pub session_key: String,
    /// Owning handler (plugin id or "native"). Inbound messages for a bound
    /// thread route to this owner instead of the default agent.
    pub owner: String,
    pub created_at_ms: u64,
    pub last_activity_at_ms: u64,
}

/// SQLite-backed store for Discord thread bindings (survives restarts).
pub struct DiscordThreadBindingStore {
    conn: Connection,
}

impl DiscordThreadBindingStore {
    /// Open (or create) the store at `path`. Use `":memory:"` for tests.
    pub fn open(path: &str) -> Result<Self> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS discord_thread_bindings (
                account_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                owner TEXT NOT NULL DEFAULT 'native',
                created_at_ms INTEGER NOT NULL,
                last_activity_at_ms INTEGER NOT NULL,
                PRIMARY KEY (account_id, thread_id)
            );",
        )?;
        Ok(Self { conn })
    }

    /// Bind (or rebind) a thread to a session + owner.
    pub fn bind(
        &self,
        account_id: &str,
        thread_id: &str,
        session_key: &str,
        owner: &str,
        now_ms: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO discord_thread_bindings
                (account_id, thread_id, session_key, owner, created_at_ms, last_activity_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(account_id, thread_id) DO UPDATE SET
                session_key = excluded.session_key,
                owner = excluded.owner,
                last_activity_at_ms = excluded.last_activity_at_ms",
            rusqlite::params![account_id, thread_id, session_key, owner, now_ms as i64],
        )?;
        Ok(())
    }

    /// Resolve the binding for a thread, refreshing its activity timestamp.
    pub fn resolve(
        &self,
        account_id: &str,
        thread_id: &str,
        now_ms: u64,
    ) -> Result<Option<DiscordThreadBinding>> {
        let binding = self
            .conn
            .query_row(
                "SELECT account_id, thread_id, session_key, owner, created_at_ms,
                        last_activity_at_ms
                 FROM discord_thread_bindings
                 WHERE account_id = ?1 AND thread_id = ?2",
                rusqlite::params![account_id, thread_id],
                |row| {
                    Ok(DiscordThreadBinding {
                        account_id: row.get(0)?,
                        thread_id: row.get(1)?,
                        session_key: row.get(2)?,
                        owner: row.get(3)?,
                        created_at_ms: row.get::<_, i64>(4)? as u64,
                        last_activity_at_ms: row.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        if binding.is_some() {
            self.conn.execute(
                "UPDATE discord_thread_bindings SET last_activity_at_ms = ?3
                 WHERE account_id = ?1 AND thread_id = ?2",
                rusqlite::params![account_id, thread_id, now_ms as i64],
            )?;
        }
        Ok(binding)
    }

    /// Remove a binding.
    pub fn unbind(&self, account_id: &str, thread_id: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM discord_thread_bindings WHERE account_id = ?1 AND thread_id = ?2",
            rusqlite::params![account_id, thread_id],
        )?;
        Ok(removed > 0)
    }

    /// Prune bindings idle past `idle_hours` or older than `max_age_hours`
    /// (0 disables either check). Returns the number pruned.
    pub fn prune(&self, idle_hours: u64, max_age_hours: u64, now_ms: u64) -> Result<usize> {
        let mut pruned = 0usize;
        if idle_hours > 0 {
            let cutoff = now_ms.saturating_sub(idle_hours * 3_600_000);
            pruned += self.conn.execute(
                "DELETE FROM discord_thread_bindings WHERE last_activity_at_ms < ?1",
                rusqlite::params![cutoff as i64],
            )?;
        }
        if max_age_hours > 0 {
            let cutoff = now_ms.saturating_sub(max_age_hours * 3_600_000);
            pruned += self.conn.execute(
                "DELETE FROM discord_thread_bindings WHERE created_at_ms < ?1",
                rusqlite::params![cutoff as i64],
            )?;
        }
        Ok(pruned)
    }
}

// ============================================================================
// Cross-provider guild admin action block
// ============================================================================

/// Guild-admin actions that need a Discord sender identity for permission
/// checks — requests originating from another provider (cross-provider tool
/// calls) are blocked for these.
pub const TRUSTED_REQUESTER_GUILD_ADMIN_ACTIONS: &[&str] = &[
    "emoji-upload",
    "sticker-upload",
    "role-add",
    "role-remove",
    "channel-create",
    "channel-edit",
    "channel-delete",
    "channel-move",
    "category-create",
    "category-edit",
    "category-delete",
    "event-create",
    "timeout",
    "kick",
    "ban",
    // camelCase aliases used by the bundled discord tool surface.
    "roleAdd",
    "roleRemove",
    "channelCreate",
    "eventCreate",
    "emojiUpload",
    "stickerUpload",
];

/// Whether an action requires a trusted Discord requester.
pub fn is_trusted_requester_guild_admin_action(action: &str) -> bool {
    TRUSTED_REQUESTER_GUILD_ADMIN_ACTIONS.contains(&action)
}

/// Whether a session key belongs to a Discord conversation (guild-admin
/// actions demand a Discord-originated requester).
pub fn is_discord_session_key(session_key: &str) -> bool {
    let lower = session_key.to_lowercase();
    lower.starts_with("discord")
        || lower.contains(":discord:")
        || lower.contains(":discord-")
        || lower.contains("discord:")
}

// ============================================================================
// PluralKit sender identity (DM pairing)
// ============================================================================

/// Resolved sender identity, PluralKit-aware: proxied messages pair and route
/// under the PluralKit member identity instead of the webhook bot author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordSenderIdentity {
    pub id: String,
    pub name: String,
    pub label: String,
    pub is_pluralkit: bool,
}

/// Port of `resolveDiscordSenderIdentity`: PluralKit member id+name win when
/// present; otherwise nickname > global name > username, labeled with the tag.
pub fn resolve_discord_sender_identity(
    author_id: &str,
    author_username: &str,
    author_global_name: Option<&str>,
    member_nickname: Option<&str>,
    pk_member_id: Option<&str>,
    pk_member_display_name: Option<&str>,
    pk_member_name: Option<&str>,
    pk_system_name: Option<&str>,
) -> DiscordSenderIdentity {
    let member_id = pk_member_id.map(str::trim).filter(|s| !s.is_empty());
    let member_name = pk_member_display_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| pk_member_name.map(str::trim).filter(|s| !s.is_empty()));
    if let (Some(id), Some(name)) = (member_id, member_name) {
        let label = match pk_system_name.map(str::trim).filter(|s| !s.is_empty()) {
            Some(system) => format!("{} (PK:{})", name, system),
            None => format!("{} (PK)", name),
        };
        return DiscordSenderIdentity {
            id: id.to_string(),
            name: name.to_string(),
            label,
            is_pluralkit: true,
        };
    }
    let display = member_nickname
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(author_global_name.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or(author_username);
    let label = if display != author_username {
        format!("{} ({})", display, author_username)
    } else {
        display.to_string()
    };
    DiscordSenderIdentity {
        id: author_id.to_string(),
        name: display.to_string(),
        label,
        is_pluralkit: false,
    }
}

// ============================================================================
// Implicit-reply fanout limit
// ============================================================================

/// A reply reference with its physical-send scope, keeping text/media/
/// component sends from desynchronizing parallel reply options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordReplyReference {
    pub message_id: String,
    /// "all" replies on every chunk; "first" only on the first physical send
    /// (implicit-reply fanout limit for single-use reply modes).
    pub scope: ReplyScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyScope {
    All,
    First,
}

/// Single-use reply modes attach the implicit reply reference only to the
/// first outbound message of a fanout.
pub fn is_single_use_reply_to_mode(mode: ReplyToMode) -> bool {
    matches!(mode, ReplyToMode::First | ReplyToMode::Batched)
}

/// Resolve the reply reference: explicit reply ids always fan out to every
/// chunk; implicit ids under single-use modes are limited to the first send.
pub fn resolve_discord_reply_reference(
    reply_to_id: Option<&str>,
    reply_to_id_is_explicit: bool,
    reply_to_mode: Option<ReplyToMode>,
) -> Option<DiscordReplyReference> {
    let message_id = reply_to_id.map(str::trim).filter(|id| !id.is_empty())?;
    let single_use = !reply_to_id_is_explicit
        && reply_to_mode.map(is_single_use_reply_to_mode).unwrap_or(false);
    Some(DiscordReplyReference {
        message_id: message_id.to_string(),
        scope: if single_use {
            ReplyScope::First
        } else {
            ReplyScope::All
        },
    })
}

/// The reply message id to attach to a physical send (None = no reply ref).
pub fn resolve_discord_reply_message_id(
    reply: Option<&DiscordReplyReference>,
    is_first: bool,
) -> Option<String> {
    let reply = reply?;
    if is_first || reply.scope == ReplyScope::All {
        Some(reply.message_id.clone())
    } else {
        None
    }
}

// ============================================================================
// Alpha-bucket model picker (>25 options)
// ============================================================================

/// Discord caps selects at 25 options.
pub const DISCORD_COMPONENT_MAX_SELECT_OPTIONS: usize = 25;
/// Alpha buckets engage only above the single-page select cap.
pub const DISCORD_MODEL_PICKER_BUCKET_THRESHOLD: usize = DISCORD_COMPONENT_MAX_SELECT_OPTIONS;
/// Target items per alpha bucket.
pub const DISCORD_MODEL_PICKER_BUCKET_TARGET_SIZE: usize = 20;

/// A letter-range bucket over a sorted item list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlphaBucket {
    /// Stable lowercase id, e.g. "a-g".
    pub id: String,
    /// Human label with count, e.g. "A–G (12)".
    pub label: String,
    /// Inclusive start index into the sorted item list.
    pub start: usize,
    /// Exclusive end index.
    pub end: usize,
}

fn first_letter(value: &str) -> String {
    value.chars().next().map(|c| c.to_lowercase().to_string()).unwrap_or_default()
}

fn bucket_target_size(total_items: usize) -> usize {
    let cap_by_bucket_count = total_items.div_ceil(DISCORD_COMPONENT_MAX_SELECT_OPTIONS);
    DISCORD_MODEL_PICKER_BUCKET_TARGET_SIZE.max(cap_by_bucket_count)
}

fn chunk_buckets_by_count(sorted_items: &[String]) -> Vec<AlphaBucket> {
    let target = bucket_target_size(sorted_items.len());
    let mut buckets = Vec::new();
    let mut start = 0usize;
    let mut index = 1usize;
    while start < sorted_items.len() {
        let end = (start + target).min(sorted_items.len());
        buckets.push(AlphaBucket {
            id: format!("part-{}", index),
            label: format!("Part {} ({})", index, end - start),
            start,
            end,
        });
        start = end;
        index += 1;
    }
    buckets
}

/// Split a sorted item list into letter-range buckets when it exceeds the
/// select cap; items sharing a first letter never straddle two buckets. If
/// every item shares a first letter, fall back to count-based chunks. Below
/// the threshold a single "All" bucket is returned.
pub fn compute_alpha_buckets(sorted_items: &[String]) -> Vec<AlphaBucket> {
    if sorted_items.is_empty() {
        return Vec::new();
    }
    if sorted_items.len() <= DISCORD_MODEL_PICKER_BUCKET_THRESHOLD {
        return vec![AlphaBucket {
            id: "all".to_string(),
            label: format!("All ({})", sorted_items.len()),
            start: 0,
            end: sorted_items.len(),
        }];
    }
    let first = first_letter(&sorted_items[0]);
    if sorted_items.iter().all(|item| first_letter(item) == first) {
        return chunk_buckets_by_count(sorted_items);
    }
    let target = bucket_target_size(sorted_items.len());
    let mut buckets = Vec::new();
    let mut start = 0usize;
    while start < sorted_items.len() {
        let mut end = (start + target).min(sorted_items.len());
        if end < sorted_items.len() {
            let last = first_letter(&sorted_items[end - 1]);
            while end < sorted_items.len() && first_letter(&sorted_items[end]) == last {
                end += 1;
            }
        }
        let start_letter = first_letter(&sorted_items[start]);
        let end_letter = first_letter(&sorted_items[end - 1]);
        let (id, label) = if start_letter == end_letter {
            (
                start_letter.clone(),
                format!("{} ({})", start_letter.to_uppercase(), end - start),
            )
        } else {
            (
                format!("{}-{}", start_letter, end_letter),
                format!(
                    "{}–{} ({})",
                    start_letter.to_uppercase(),
                    end_letter.to_uppercase(),
                    end - start
                ),
            )
        };
        buckets.push(AlphaBucket { id, label, start, end });
        start = end;
    }
    buckets
}

// ============================================================================
// Agent components registry TTL (`agentComponents.ttlMs`)
// ============================================================================

/// Default TTL for sent Discord component callbacks (30 min).
pub const DEFAULT_AGENT_COMPONENTS_TTL_MS: u64 = 1_800_000;

/// Resolve `agentComponents.ttlMs` from the raw config value.
pub fn resolve_agent_components_ttl_ms(agent_components: Option<&serde_json::Value>) -> u64 {
    agent_components
        .and_then(|v| v.get("ttlMs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_AGENT_COMPONENTS_TTL_MS)
}

/// Whether agent-controlled interactive components are enabled (default: true).
pub fn agent_components_enabled(agent_components: Option<&serde_json::Value>) -> bool {
    agent_components
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Registry of sent component callbacks; entries expire after the TTL so
/// stale button clicks are rejected instead of firing old handlers.
#[derive(Debug, Default)]
pub struct ComponentRegistry {
    ttl_ms: u64,
    entries: HashMap<String, (serde_json::Value, u64)>,
}

impl ComponentRegistry {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            entries: HashMap::new(),
        }
    }

    pub fn register(&mut self, custom_id: &str, payload: serde_json::Value, now_ms: u64) {
        self.prune(now_ms);
        self.entries.insert(custom_id.to_string(), (payload, now_ms));
    }

    /// Look up a callback payload; expired entries return `None`.
    pub fn resolve(&mut self, custom_id: &str, now_ms: u64) -> Option<serde_json::Value> {
        self.prune(now_ms);
        self.entries.get(custom_id).map(|(payload, _)| payload.clone())
    }

    pub fn prune(&mut self, now_ms: u64) {
        let ttl = self.ttl_ms;
        self.entries
            .retain(|_, (_, at)| now_ms.saturating_sub(*at) < ttl);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// suppressEmbeds default + outbound message flags
// ============================================================================

/// `MessageFlags.SuppressEmbeds`.
pub const SUPPRESS_EMBEDS_FLAG: u64 = 1 << 2;
/// `MessageFlags.SuppressNotifications` (silent messages).
pub const SUPPRESS_NOTIFICATIONS_FLAG: u64 = 1 << 12;

/// Resolve effective suppressEmbeds: per-send override > account config >
/// default **true** (Discord-generated link embeds are suppressed by default).
pub fn resolve_discord_suppress_embeds(configured: Option<bool>, override_: Option<bool>) -> bool {
    override_.or(configured).unwrap_or(true)
}

/// Compose outbound message flags; `None` when no flags apply.
pub fn resolve_discord_message_flags(silent: bool, suppress_embeds: bool) -> Option<u64> {
    let mut flags = 0u64;
    if suppress_embeds {
        flags |= SUPPRESS_EMBEDS_FLAG;
    }
    if silent {
        flags |= SUPPRESS_NOTIFICATIONS_FLAG;
    }
    if flags == 0 {
        None
    } else {
        Some(flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- thread bindings ----------------------------------------------------

    #[test]
    fn thread_bindings_persist_and_route_to_owner() {
        let store = DiscordThreadBindingStore::open(":memory:").unwrap();
        store
            .bind("acct", "thread-1", "agent:main:discord:channel:1", "plugin:focus", 1_000)
            .unwrap();
        let binding = store.resolve("acct", "thread-1", 2_000).unwrap().unwrap();
        assert_eq!(binding.owner, "plugin:focus");
        assert_eq!(binding.session_key, "agent:main:discord:channel:1");
        // Rebinding replaces owner + session.
        store
            .bind("acct", "thread-1", "agent:other", "native", 3_000)
            .unwrap();
        let binding = store.resolve("acct", "thread-1", 3_500).unwrap().unwrap();
        assert_eq!(binding.owner, "native");
        assert_eq!(binding.session_key, "agent:other");
        // Unknown thread resolves to None; unbind removes.
        assert!(store.resolve("acct", "nope", 0).unwrap().is_none());
        assert!(store.unbind("acct", "thread-1").unwrap());
        assert!(store.resolve("acct", "thread-1", 0).unwrap().is_none());
    }

    #[test]
    fn thread_bindings_prune_idle_and_aged() {
        let store = DiscordThreadBindingStore::open(":memory:").unwrap();
        let hour = 3_600_000u64;
        store.bind("a", "t1", "s1", "native", 0).unwrap();
        store.bind("a", "t2", "s2", "native", 30 * hour).unwrap();
        // Idle prune (24h): t1 idle since 0 → pruned at now=30h.
        let pruned = store.prune(24, 0, 30 * hour).unwrap();
        assert_eq!(pruned, 1);
        assert!(store.resolve("a", "t2", 30 * hour).unwrap().is_some());
        // Max-age prune: t2 created at 30h → pruned at now=80h with max 48h.
        let pruned = store.prune(0, 48, 80 * hour).unwrap();
        assert_eq!(pruned, 1);
    }

    // ---- guild admin cross-provider block -----------------------------------

    #[test]
    fn guild_admin_actions_classified() {
        assert!(is_trusted_requester_guild_admin_action("ban"));
        assert!(is_trusted_requester_guild_admin_action("role-add"));
        assert!(is_trusted_requester_guild_admin_action("roleAdd"));
        assert!(!is_trusted_requester_guild_admin_action("send"));
        assert!(!is_trusted_requester_guild_admin_action("react"));
    }

    #[test]
    fn discord_session_key_detection() {
        assert!(is_discord_session_key("discord:channel:123"));
        assert!(is_discord_session_key("agent:main:discord:channel:1"));
        assert!(!is_discord_session_key("telegram:chat:5"));
        assert!(!is_discord_session_key("agent:main:slack:C1"));
    }

    // ---- PluralKit sender identity ------------------------------------------

    #[test]
    fn pluralkit_member_identity_wins() {
        let identity = resolve_discord_sender_identity(
            "999",
            "webhookbot",
            None,
            None,
            Some("pk-m1"),
            Some("Ivy"),
            Some("ivy"),
            Some("The System"),
        );
        assert!(identity.is_pluralkit);
        assert_eq!(identity.id, "pk-m1");
        assert_eq!(identity.label, "Ivy (PK:The System)");
        // Without system name.
        let identity = resolve_discord_sender_identity(
            "999", "webhookbot", None, None, Some("pk-m1"), None, Some("ivy"), None,
        );
        assert_eq!(identity.label, "ivy (PK)");
    }

    #[test]
    fn non_pluralkit_identity_prefers_nickname() {
        let identity = resolve_discord_sender_identity(
            "42",
            "dendi",
            Some("Dendi S"),
            Some("Boss"),
            None,
            None,
            None,
            None,
        );
        assert!(!identity.is_pluralkit);
        assert_eq!(identity.id, "42");
        assert_eq!(identity.name, "Boss");
        assert_eq!(identity.label, "Boss (dendi)");
        let plain = resolve_discord_sender_identity("42", "dendi", None, None, None, None, None, None);
        assert_eq!(plain.label, "dendi");
    }

    // ---- implicit-reply fanout limit ----------------------------------------

    #[test]
    fn implicit_reply_fanout_limited_to_first() {
        let implicit = resolve_discord_reply_reference(Some("m1"), false, Some(ReplyToMode::First))
            .unwrap();
        assert_eq!(implicit.scope, ReplyScope::First);
        assert_eq!(
            resolve_discord_reply_message_id(Some(&implicit), true).as_deref(),
            Some("m1")
        );
        assert_eq!(resolve_discord_reply_message_id(Some(&implicit), false), None);
        // Explicit reply ids fan out to every chunk.
        let explicit = resolve_discord_reply_reference(Some("m1"), true, Some(ReplyToMode::First))
            .unwrap();
        assert_eq!(explicit.scope, ReplyScope::All);
        assert_eq!(
            resolve_discord_reply_message_id(Some(&explicit), false).as_deref(),
            Some("m1")
        );
        // "all" mode is multi-use even when implicit.
        let all = resolve_discord_reply_reference(Some("m1"), false, Some(ReplyToMode::All)).unwrap();
        assert_eq!(all.scope, ReplyScope::All);
        assert!(resolve_discord_reply_reference(None, false, None).is_none());
    }

    // ---- alpha buckets -------------------------------------------------------

    fn items(prefixes: &[(&str, usize)]) -> Vec<String> {
        let mut out = Vec::new();
        for (prefix, count) in prefixes {
            for i in 0..*count {
                out.push(format!("{}{:02}", prefix, i));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn small_lists_single_bucket() {
        let list = items(&[("a", 10), ("b", 10)]);
        let buckets = compute_alpha_buckets(&list);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].id, "all");
        assert_eq!(buckets[0].end, 20);
        assert!(compute_alpha_buckets(&[]).is_empty());
    }

    #[test]
    fn large_lists_bucket_by_letter_without_splitting_groups() {
        let list = items(&[("a", 15), ("g", 10), ("m", 15), ("z", 10)]);
        assert_eq!(list.len(), 50);
        let buckets = compute_alpha_buckets(&list);
        assert!(buckets.len() >= 2);
        // No letter group straddles two buckets.
        for pair in buckets.windows(2) {
            let last_letter = first_letter(&list[pair[0].end - 1]);
            let next_letter = first_letter(&list[pair[1].start]);
            assert_ne!(last_letter, next_letter);
        }
        // Buckets cover the whole list contiguously.
        assert_eq!(buckets[0].start, 0);
        assert_eq!(buckets.last().unwrap().end, list.len());
        // Bucket count stays under the select cap.
        assert!(buckets.len() <= DISCORD_COMPONENT_MAX_SELECT_OPTIONS);
    }

    #[test]
    fn same_prefix_falls_back_to_count_chunks() {
        let list = items(&[("qwen3-", 60)]);
        let buckets = compute_alpha_buckets(&list);
        assert!(buckets.len() >= 2);
        assert!(buckets[0].id.starts_with("part-"));
        assert_eq!(buckets.last().unwrap().end, 60);
    }

    // ---- component registry TTL ---------------------------------------------

    #[test]
    fn component_registry_ttl() {
        let mut registry = ComponentRegistry::new(1_000);
        registry.register("btn1", serde_json::json!({"cb": 1}), 0);
        assert!(registry.resolve("btn1", 500).is_some());
        assert!(registry.resolve("btn1", 1_000).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn agent_components_config_resolution() {
        assert_eq!(resolve_agent_components_ttl_ms(None), 1_800_000);
        let cfg = serde_json::json!({ "ttlMs": 5000, "enabled": false });
        assert_eq!(resolve_agent_components_ttl_ms(Some(&cfg)), 5000);
        assert!(!agent_components_enabled(Some(&cfg)));
        assert!(agent_components_enabled(None));
    }

    // ---- suppressEmbeds + flags ---------------------------------------------

    #[test]
    fn suppress_embeds_defaults_true() {
        assert!(resolve_discord_suppress_embeds(None, None));
        assert!(!resolve_discord_suppress_embeds(Some(false), None));
        assert!(resolve_discord_suppress_embeds(Some(false), Some(true)));
        assert!(!resolve_discord_suppress_embeds(Some(true), Some(false)));
    }

    #[test]
    fn message_flags_composition() {
        assert_eq!(resolve_discord_message_flags(false, false), None);
        assert_eq!(resolve_discord_message_flags(false, true), Some(4));
        assert_eq!(resolve_discord_message_flags(true, false), Some(4096));
        assert_eq!(resolve_discord_message_flags(true, true), Some(4100));
    }
}
