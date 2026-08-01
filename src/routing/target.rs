//! Outbound target prefix parsing and channel selection helpers.
//!
//! Ported from OpenClaw `src/infra/outbound/channel-target-prefix.ts` and the
//! `last`-fallback selection behavior of
//! `src/infra/outbound/channel-selection.ts` (v2026.5.2–7.1):
//!
//! - Generic target-kind prefixes (`channel:`, `user:`, `room:`, …) are never
//!   provider prefixes.
//! - A target whose channel-owned prefix belongs to a *different* selected
//!   channel is rejected ("Target prefix \"telegram:\" belongs to telegram,
//!   not discord."). Selection `last` never rejects.
//! - On `last` fallback, a provider-prefixed target (`telegram:123`) selects
//!   its owning channel.
//! - Hooks derive `ctx.channelId` from the conversation target rather than
//!   the provider name (v2026.5.2 Channels row). The hooks cluster owns
//!   `src/hooks/mod.rs` — it should call
//!   [`derive_hook_channel_id`] when mapping conversation ids into hook
//!   context (noted handoff).
//!
//! Upstream resolves prefix ownership through the live plugin registry
//! (`plugin.messaging.targetPrefixes`); mylobster bundles its channels
//! natively, so ownership comes from the static [`builtin_target_prefixes`]
//! table plus any prefixes registered at runtime via
//! [`register_target_prefix`] (used by plugin-backed channels).

use once_cell::sync::Lazy;
use parking_lot::RwLock;

/// Generic target-kind prefixes that never denote a provider/channel.
pub const TARGET_KIND_PREFIXES: &[&str] = &[
    "channel",
    "conversation",
    "dm",
    "group",
    "room",
    "thread",
    "user",
];

/// Built-in channel-owned target prefixes (channel id → accepted prefixes).
///
/// Conservative mirror of the bundled channels' `messaging.targetPrefixes`.
pub fn builtin_target_prefixes() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("telegram", &["telegram", "tg"]),
        ("discord", &["discord"]),
        ("slack", &["slack"]),
        ("whatsapp", &["whatsapp", "wa"]),
        ("signal", &["signal"]),
        ("imessage", &["imessage", "imsg"]),
        ("matrix", &["matrix"]),
        ("mattermost", &["mattermost"]),
        ("msteams", &["msteams", "teams"]),
        ("googlechat", &["googlechat", "gchat"]),
        ("irc", &["irc"]),
        ("twitch", &["twitch"]),
        ("nostr", &["nostr"]),
        ("line", &["line"]),
        ("zalo", &["zalo"]),
        ("zalouser", &["zalouser"]),
        ("feishu", &["feishu", "lark"]),
        ("qqbot", &["qqbot", "qq"]),
        ("yuanbao", &["yuanbao"]),
        ("voicecall", &["voicecall"]),
        ("googlemeet", &["googlemeet"]),
        ("sms", &["sms"]),
        ("bluebubbles", &["bluebubbles"]),
        ("nextcloud", &["nextcloud"]),
        ("synology_chat", &["synology", "synology_chat"]),
    ]
}

/// Runtime-registered extra prefixes (plugin channels).
static EXTRA_PREFIXES: Lazy<RwLock<Vec<(String, String)>>> = Lazy::new(|| RwLock::new(Vec::new()));

/// Register an additional channel-owned target prefix at runtime.
pub fn register_target_prefix(channel: &str, prefix: &str) {
    let channel = channel.trim().to_lowercase();
    let prefix = prefix.trim().to_lowercase();
    if channel.is_empty() || prefix.is_empty() {
        return;
    }
    let mut extra = EXTRA_PREFIXES.write();
    if !extra.iter().any(|(c, p)| c == &channel && p == &prefix) {
        extra.push((channel, prefix));
    }
}

/// Clear runtime-registered prefixes (test isolation).
pub fn clear_registered_prefixes_for_tests() {
    EXTRA_PREFIXES.write().clear();
}

fn normalize_lower(value: &str) -> Option<String> {
    let t = value.trim().to_lowercase();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Remove a selected channel/provider prefix from an outbound target string.
///
/// Mirror of upstream `stripTargetProviderPrefix`.
pub fn strip_target_provider_prefix(raw: &str, providers: &[&str]) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_lowercase();
    for provider in providers {
        if let Some(p) = normalize_lower(provider) {
            if lower.starts_with(&format!("{p}:")) {
                return trimmed[p.len() + 1..].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Remove generic target-kind prefixes such as `room:`, `thread:`, `user:`.
///
/// Mirror of upstream `stripTargetKindPrefix` (case-insensitive, one level).
pub fn strip_target_kind_prefix(raw: &str, kinds: &[&str]) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_lowercase();
    for kind in kinds {
        if let Some(k) = normalize_lower(kind) {
            if lower.starts_with(&format!("{k}:")) {
                return trimmed[k.len() + 1..].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Strip plugin topic suffixes (`…:topic:<x>`) while preserving ordinary
/// colon-containing targets. With `allow_numeric_shorthand`, `-100123:45`
/// yields the chat id `-100123` (mirror of `stripTargetTopicSuffix`).
pub fn strip_target_topic_suffix(raw: &str, allow_numeric_shorthand: bool) -> String {
    let trimmed = raw.trim();
    if allow_numeric_shorthand {
        if let Some((chat, topic)) = trimmed.split_once(':') {
            let chat_ok = {
                let c = chat.strip_prefix('-').unwrap_or(chat);
                !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit())
            };
            if chat_ok && !topic.is_empty() && topic.chars().all(|ch| ch.is_ascii_digit()) {
                return chat.to_string();
            }
        }
    }
    let lower = trimmed.to_lowercase();
    if let Some(idx) = lower.find(":topic:") {
        return trimmed[..idx].trim().to_string();
    }
    trimmed.to_string()
}

/// Parse the leading `prefix:` token of a target, if syntactically present.
fn leading_prefix(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let (head, _) = trimmed.split_once(':')?;
    let head = head.trim().to_lowercase();
    let mut chars = head.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(head)
}

/// Resolve the channel implied by a channel-owned target prefix, if any.
///
/// Generic kind prefixes (`user:`, `channel:`, …) return `None`.
pub fn resolve_target_prefixed_channel(raw: &str) -> Option<String> {
    let prefix = leading_prefix(raw)?;
    if TARGET_KIND_PREFIXES.contains(&prefix.as_str()) {
        return None;
    }
    for (channel, prefixes) in builtin_target_prefixes() {
        if prefixes.contains(&prefix.as_str()) {
            return Some((*channel).to_string());
        }
    }
    let extra = EXTRA_PREFIXES.read();
    extra
        .iter()
        .find(|(_, p)| p == &prefix)
        .map(|(c, _)| c.clone())
}

/// Reject targets whose channel-owned prefix belongs to a different selected
/// channel. Mirror of upstream `validateTargetProviderPrefix`:
/// `channel == "last"` (or empty) never rejects; kind prefixes and unknown
/// prefixes pass through.
pub fn validate_target_provider_prefix(channel: &str, to: Option<&str>) -> Result<(), String> {
    let selected = match normalize_lower(channel) {
        Some(c) if c != "last" => c,
        _ => return Ok(()),
    };
    let Some(target) = to else { return Ok(()) };
    let Some(prefixed_channel) = resolve_target_prefixed_channel(target) else {
        return Ok(());
    };
    if prefixed_channel == selected {
        return Ok(());
    }
    let prefix = leading_prefix(target).unwrap_or_default();
    Err(format!(
        "Target prefix \"{prefix}:\" belongs to {prefixed_channel}, not {selected}."
    ))
}

/// On `last` fallback, let a provider-prefixed target select its channel
/// (`telegram:123` ⇒ telegram) — v2026.5.2 Channels row.
pub fn select_channel_for_last_fallback(to: &str) -> Option<String> {
    resolve_target_prefixed_channel(to)
}

/// Derive the hook `ctx.channelId` from a conversation target, falling back
/// to the provider name only when the target carries no channel prefix.
///
/// v2026.5.2 Channels row "Hooks: derive `ctx.channelId` from conversation
/// target, not provider name". HANDOFF: `src/hooks/mod.rs` (agents-core
/// cluster) should call this when building hook context.
pub fn derive_hook_channel_id(conversation_target: Option<&str>, provider_name: &str) -> String {
    if let Some(target) = conversation_target {
        if let Some(channel) = resolve_target_prefixed_channel(target) {
            return channel;
        }
    }
    normalize_lower(provider_name).unwrap_or_default()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_provider_prefix() {
        assert_eq!(
            strip_target_provider_prefix("telegram:12345", &["telegram", "tg"]),
            "12345"
        );
        assert_eq!(
            strip_target_provider_prefix(" TG:12345 ", &["telegram", "tg"]),
            "12345"
        );
        assert_eq!(
            strip_target_provider_prefix("12345", &["telegram"]),
            "12345"
        );
        // Foreign prefixes stay put.
        assert_eq!(
            strip_target_provider_prefix("discord:9", &["telegram"]),
            "discord:9"
        );
    }

    #[test]
    fn strip_kind_prefix() {
        assert_eq!(strip_target_kind_prefix("user:U1", TARGET_KIND_PREFIXES), "U1");
        assert_eq!(
            strip_target_kind_prefix("Channel:C1", TARGET_KIND_PREFIXES),
            "C1"
        );
        assert_eq!(strip_target_kind_prefix("C1", TARGET_KIND_PREFIXES), "C1");
    }

    #[test]
    fn strip_topic_suffix() {
        assert_eq!(strip_target_topic_suffix("-100123:topic:77", false), "-100123");
        assert_eq!(strip_target_topic_suffix("-100123:45", true), "-100123");
        // Non-numeric colon targets preserved without shorthand.
        assert_eq!(strip_target_topic_suffix("room:abc:def", false), "room:abc:def");
        assert_eq!(strip_target_topic_suffix("-100123:45", false), "-100123:45");
    }

    #[test]
    fn prefixed_channel_resolution() {
        assert_eq!(
            resolve_target_prefixed_channel("telegram:123").as_deref(),
            Some("telegram")
        );
        assert_eq!(
            resolve_target_prefixed_channel("tg:123").as_deref(),
            Some("telegram")
        );
        assert_eq!(
            resolve_target_prefixed_channel("wa:491701234567").as_deref(),
            Some("whatsapp")
        );
        // Kind prefixes are not providers.
        assert_eq!(resolve_target_prefixed_channel("user:U123"), None);
        assert_eq!(resolve_target_prefixed_channel("channel:C123"), None);
        // Unknown prefixes are not providers.
        assert_eq!(resolve_target_prefixed_channel("mailto:x@y"), None);
        // No prefix at all.
        assert_eq!(resolve_target_prefixed_channel("12345"), None);
        // Numeric head is not a prefix.
        assert_eq!(resolve_target_prefixed_channel("-100:45"), None);
    }

    #[test]
    fn wrong_channel_prefix_rejected() {
        let err = validate_target_provider_prefix("discord", Some("telegram:123")).unwrap_err();
        assert!(err.contains("telegram"), "{err}");
        assert!(err.contains("discord"), "{err}");

        // Matching prefix passes.
        assert!(validate_target_provider_prefix("telegram", Some("telegram:123")).is_ok());
        // Alias prefix maps to the same channel.
        assert!(validate_target_provider_prefix("telegram", Some("tg:123")).is_ok());
        // `last` never rejects.
        assert!(validate_target_provider_prefix("last", Some("telegram:123")).is_ok());
        // Kind prefixes never reject.
        assert!(validate_target_provider_prefix("discord", Some("user:U1")).is_ok());
        // No target: ok.
        assert!(validate_target_provider_prefix("discord", None).is_ok());
    }

    #[test]
    fn last_fallback_selects_prefixed_channel() {
        assert_eq!(
            select_channel_for_last_fallback("telegram:123").as_deref(),
            Some("telegram")
        );
        assert_eq!(select_channel_for_last_fallback("123"), None);
    }

    #[test]
    fn runtime_registered_prefixes() {
        clear_registered_prefixes_for_tests();
        register_target_prefix("mychan", "mc");
        assert_eq!(
            resolve_target_prefixed_channel("mc:42").as_deref(),
            Some("mychan")
        );
        clear_registered_prefixes_for_tests();
    }

    #[test]
    fn hook_channel_id_from_target() {
        assert_eq!(
            derive_hook_channel_id(Some("telegram:123"), "anthropic"),
            "telegram"
        );
        assert_eq!(derive_hook_channel_id(Some("123"), "Slack"), "slack");
        assert_eq!(derive_hook_channel_id(None, "discord"), "discord");
    }
}
