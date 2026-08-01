//! Telegram delivery-target parsing (port of OpenClaw
//! `extensions/telegram/src/targets.ts` at v2026.7.1).
//!
//! Supported formats:
//! - `chatId` (plain chat id, t.me link, @username, internal `telegram:`/`tg:`
//!   prefixes, legacy `telegram:group:<id>`)
//! - `chatId:topicId` (numeric topic/thread id)
//! - `chatId:topic:topicId` (explicit topic marker; preferred)

use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramChatType {
    Direct,
    Group,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramTarget {
    pub chat_id: String,
    pub message_thread_id: Option<u64>,
    pub chat_type: TelegramChatType,
}

static NUMERIC_CHAT_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^-?\d+$").unwrap());
static USERNAME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?i)[A-Za-z0-9_]{5,}$").unwrap());
static TME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?i)(?:https?://)?t\.me/([A-Za-z0-9_]+)$").unwrap());
static TOPIC_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(.+?):topic:(\d+)$").unwrap());
static COLON_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(.+):(\d+)$").unwrap());
static LEGACY_GROUP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?i)group:(-?\d+(?::topic:\d+|:\d+)?)$").unwrap());

pub fn is_numeric_telegram_chat_id(raw: &str) -> bool {
    NUMERIC_CHAT_ID_RE.is_match(raw.trim())
}

/// Strips `telegram:` / `tg:` provider prefixes and the legacy internal
/// `group:` marker that only appears after a provider prefix.
pub fn strip_telegram_internal_prefixes(to: &str) -> String {
    let mut trimmed = to.trim().to_string();
    let mut stripped_provider_prefix = false;
    loop {
        let lower = trimmed.to_lowercase();
        let next = if lower.starts_with("telegram:") {
            stripped_provider_prefix = true;
            trimmed["telegram:".len()..].trim().to_string()
        } else if lower.starts_with("tg:") {
            stripped_provider_prefix = true;
            trimmed["tg:".len()..].trim().to_string()
        } else if stripped_provider_prefix && lower.starts_with("group:") {
            trimmed["group:".len()..].trim().to_string()
        } else {
            trimmed.clone()
        };
        if next == trimmed {
            return trimmed;
        }
        trimmed = next;
    }
}

/// Legacy outbound form `group:<id>[...]` → bare id form.
pub fn normalize_telegram_outbound_target(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(caps) = LEGACY_GROUP_RE.captures(trimmed) {
        return caps[1].to_string();
    }
    raw.to_string()
}

/// Normalizes a lookup target to a numeric chat id or `@username`, resolving
/// t.me links; `None` when the target cannot address a Telegram chat.
pub fn normalize_telegram_lookup_target(raw: &str) -> Option<String> {
    let stripped = strip_telegram_internal_prefixes(raw);
    if stripped.is_empty() {
        return None;
    }
    if is_numeric_telegram_chat_id(&stripped) {
        return Some(stripped);
    }
    if let Some(caps) = TME_RE.captures(&stripped) {
        return Some(format!("@{}", &caps[1]));
    }
    if let Some(handle) = stripped.strip_prefix('@') {
        if handle.is_empty() || !USERNAME_RE.is_match(handle) {
            return None;
        }
        return Some(format!("@{handle}"));
    }
    if USERNAME_RE.is_match(&stripped) {
        return Some(format!("@{stripped}"));
    }
    None
}

fn resolve_chat_type(chat_id: &str) -> TelegramChatType {
    let trimmed = chat_id.trim();
    if trimmed.is_empty() {
        return TelegramChatType::Unknown;
    }
    if is_numeric_telegram_chat_id(trimmed) {
        if trimmed.starts_with('-') {
            TelegramChatType::Group
        } else {
            TelegramChatType::Direct
        }
    } else {
        TelegramChatType::Unknown
    }
}

fn parse_thread_id(raw: &str) -> Option<u64> {
    // Strict non-negative integer: digits only, no leading '+'/'-'.
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Parses a Telegram delivery target into chat id + optional topic/thread id.
pub fn parse_telegram_target(to: &str) -> TelegramTarget {
    let normalized = strip_telegram_internal_prefixes(to);

    if let Some(caps) = TOPIC_RE.captures(&normalized) {
        if let Some(thread_id) = parse_thread_id(&caps[2]) {
            let chat_id = caps[1].to_string();
            return TelegramTarget {
                chat_type: resolve_chat_type(&chat_id),
                chat_id,
                message_thread_id: Some(thread_id),
            };
        }
        return TelegramTarget {
            chat_type: resolve_chat_type(&normalized),
            chat_id: normalized,
            message_thread_id: None,
        };
    }

    if let Some(caps) = COLON_RE.captures(&normalized) {
        if let Some(thread_id) = parse_thread_id(&caps[2]) {
            let chat_id = caps[1].to_string();
            return TelegramTarget {
                chat_type: resolve_chat_type(&chat_id),
                chat_id,
                message_thread_id: Some(thread_id),
            };
        }
    }

    TelegramTarget {
        chat_type: resolve_chat_type(&normalized),
        chat_id: normalized,
        message_thread_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_numeric_ids() {
        let t = parse_telegram_target("123456");
        assert_eq!(t.chat_id, "123456");
        assert_eq!(t.message_thread_id, None);
        assert_eq!(t.chat_type, TelegramChatType::Direct);

        let g = parse_telegram_target("-1001234567890");
        assert_eq!(g.chat_type, TelegramChatType::Group);
    }

    #[test]
    fn explicit_topic_marker_preferred() {
        let t = parse_telegram_target("-100123:topic:42");
        assert_eq!(t.chat_id, "-100123");
        assert_eq!(t.message_thread_id, Some(42));
        assert_eq!(t.chat_type, TelegramChatType::Group);
    }

    #[test]
    fn bare_colon_topic_form() {
        let t = parse_telegram_target("-100123:42");
        assert_eq!(t.chat_id, "-100123");
        assert_eq!(t.message_thread_id, Some(42));
    }

    #[test]
    fn provider_prefixes_stripped() {
        assert_eq!(parse_telegram_target("telegram:123").chat_id, "123");
        assert_eq!(parse_telegram_target("tg:-100999:topic:7").chat_id, "-100999");
        assert_eq!(
            parse_telegram_target("telegram:group:-100777").chat_id,
            "-100777"
        );
        // `group:` without a provider prefix is NOT stripped.
        assert_eq!(parse_telegram_target("group:-1").chat_id, "group:-1");
    }

    #[test]
    fn legacy_group_outbound_normalized() {
        assert_eq!(
            normalize_telegram_outbound_target("group:-100123:topic:5"),
            "-100123:topic:5"
        );
        assert_eq!(normalize_telegram_outbound_target("plain"), "plain");
    }

    #[test]
    fn lookup_target_resolution() {
        assert_eq!(
            normalize_telegram_lookup_target("t.me/somebot").as_deref(),
            Some("@somebot")
        );
        assert_eq!(
            normalize_telegram_lookup_target("https://t.me/somebot").as_deref(),
            Some("@somebot")
        );
        assert_eq!(
            normalize_telegram_lookup_target("@handle_ok").as_deref(),
            Some("@handle_ok")
        );
        assert_eq!(
            normalize_telegram_lookup_target("handle_ok").as_deref(),
            Some("@handle_ok")
        );
        assert_eq!(
            normalize_telegram_lookup_target("telegram:-100123").as_deref(),
            Some("-100123")
        );
        assert_eq!(normalize_telegram_lookup_target("@bad!"), None);
        assert_eq!(normalize_telegram_lookup_target("abc"), None); // < 5 chars
        assert_eq!(normalize_telegram_lookup_target(""), None);
    }

    #[test]
    fn username_chat_type_unknown() {
        let t = parse_telegram_target("@somebody_here");
        assert_eq!(t.chat_type, TelegramChatType::Unknown);
    }
}
