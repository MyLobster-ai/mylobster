//! WhatsApp channel — pure-logic port of OpenClaw `extensions/whatsapp/src/*`
//! at v2026.7.1.
//!
//! mylobster carries no Baileys/WhatsApp-Web wire-protocol dependency, so every
//! upstream behavior is ported as testable pure logic, config resolution, and
//! connection-lifecycle state machines that a future live-socket layer calls:
//!
//! - Target normalization + `@newsletter`/group/LID classification
//!   (`normalize-target.ts`, `resolve-outbound-target.ts`, `targets-runtime.ts`)
//! - Quoted-message metadata cache + quoted-image inbound media capture
//!   (`quoted-message.ts`, `inbound/extract.ts`)
//! - Connection teardown ordering, login-wait machine, terminal-reason
//!   classification (`connection-controller.ts`)
//! - Credential durability: atomic writes, symlink rejection, backup restore
//!   (`creds-files.ts`, `creds-persistence.ts`, `auth-store.ts`)
//! - Reconnect backoff policy + bounded catch-up (`reconnect.ts`,
//!   `inbound/monitor.ts`)
//! - Serialized per-account sends + socket-operation timing (`socket-timing.ts`)
//! - Status-reaction lifecycle (`auto-reply/monitor/status-reaction.ts`)
//! - Suffix-only streaming, group visible-reply policy, media admission,
//!   document filenames, reachout timelock, `/tts latest`
//!   (`inbound/media.ts`, `document-filename.ts`, `inbound/monitor.ts`,
//!   `src/auto-reply/reply/commands-tts.ts`)
//!
//! A live-socket integration must:
//! 1. Call [`read_web_creds_with_backup_restore`] before boot and
//!    [`write_web_creds_atomically`] on every `creds.update` (durable persist
//!    happens *before* login is reported successful — see [`LoginWaitMachine`]).
//! 2. Drive [`LoginWaitMachine`] from `connection.update` events during login
//!    and [`classify_long_lived_disconnect`] afterwards.
//! 3. Tear sockets down through [`close_wa_socket`] (graceful `end(error)`
//!    before the raw WebSocket close).
//! 4. Route every outbound send through [`with_serialized_account_send`] and
//!    flush [`OutboundDrainQueue`] on a periodic tick plus before socket close
//!    (bounded by [`DRAIN_FLUSH_TIMEOUT_MS`]).
//! 5. On reconnect, feed missed messages through [`filter_reconnect_catchup`].

use crate::config::{
    Config, WhatsAppAccountConfig, WhatsAppGroupVisibleReplyMode, WhatsAppReconnectConfig,
    WhatsAppSocketTimingConfig, WhatsAppStatusReactionsConfig,
};
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

// ============================================================================
// E.164 + JID normalization / target classification
// Port of `normalize-target.ts` (v2026.7.1).
// ============================================================================

static WHATSAPP_USER_JID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(\d+)(?::\d+)?@s\.whatsapp\.net$").unwrap());
static WHATSAPP_LEGACY_USER_JID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(\d+)@c\.us$").unwrap());
static WHATSAPP_LID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(\d+)@lid$").unwrap());
static NON_WHATSAPP_PROVIDER_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^[a-z][a-z0-9-]*:").unwrap());
static WHATSAPP_NEWSLETTER_JID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^([0-9]+)@newsletter$").unwrap());
static GROUP_LOCAL_PART_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[0-9]+(-[0-9]+)*$").unwrap());
static DIRECT_USER_JID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(\d+)(?::\d+)?@(s\.whatsapp\.net|c\.us|lid|hosted|hosted\.lid)$").unwrap()
});

/// Normalize a phone-number-ish string to E.164 form (`+<digits>`).
///
/// Mirrors the plugin-sdk `normalizeE164` contract used by
/// `normalize-target.ts`: strip everything but digits, prefix `+`. A result of
/// length <= 1 means "no usable digits".
pub fn normalize_e164(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("+{digits}")
}

/// Repeatedly strip leading `whatsapp:` provider prefixes.
pub fn strip_whatsapp_target_prefixes(value: &str) -> String {
    let mut candidate = value.trim().to_string();
    loop {
        let lower = candidate.to_lowercase();
        if let Some(rest) = lower
            .starts_with("whatsapp:")
            .then(|| candidate["whatsapp:".len()..].trim().to_string())
        {
            candidate = rest;
        } else {
            return candidate;
        }
    }
}

/// Normalize a group target (with optional `whatsapp:`/`group:` prefixes) to a
/// canonical `<digits[-digits...]>@g.us` JID. Implements the row-7
/// "`group:`-prefixed JID resolution" behavior.
pub fn normalize_whatsapp_group_jid(value: &str) -> Option<String> {
    let stripped = strip_whatsapp_target_prefixes(value);
    let candidate = if stripped.to_lowercase().starts_with("group:") {
        stripped["group:".len()..].trim().to_string()
    } else {
        stripped
    };
    let lower = candidate.to_lowercase();
    if !lower.ends_with("@g.us") {
        return None;
    }
    let local = &candidate[..candidate.len() - "@g.us".len()];
    if local.is_empty() || local.contains('@') {
        return None;
    }
    GROUP_LOCAL_PART_RE
        .is_match(local)
        .then(|| format!("{local}@g.us"))
}

pub fn is_whatsapp_group_jid(value: &str) -> bool {
    normalize_whatsapp_group_jid(value).is_some()
}

/// `@newsletter` JIDs address WhatsApp channels (broadcast), not DMs.
pub fn is_whatsapp_newsletter_jid(value: &str) -> bool {
    WHATSAPP_NEWSLETTER_JID_RE.is_match(&strip_whatsapp_target_prefixes(value))
}

pub fn is_whatsapp_user_target(value: &str) -> bool {
    let candidate = strip_whatsapp_target_prefixes(value);
    WHATSAPP_USER_JID_RE.is_match(&candidate)
        || WHATSAPP_LEGACY_USER_JID_RE.is_match(&candidate)
        || WHATSAPP_LID_RE.is_match(&candidate)
}

fn extract_user_jid_phone(jid: &str) -> Option<String> {
    for re in [
        &*WHATSAPP_USER_JID_RE,
        &*WHATSAPP_LEGACY_USER_JID_RE,
        &*WHATSAPP_LID_RE,
    ] {
        if let Some(caps) = re.captures(jid) {
            return Some(caps[1].to_string());
        }
    }
    None
}

/// Normalize any WhatsApp target to a canonical form: group JID, newsletter
/// JID, or `+<E.164>` for users. `None` when the value cannot address a
/// WhatsApp conversation.
pub fn normalize_whatsapp_target(value: &str) -> Option<String> {
    let candidate = strip_whatsapp_target_prefixes(value);
    if candidate.is_empty() {
        return None;
    }
    if let Some(group) = normalize_whatsapp_group_jid(&candidate) {
        return Some(group);
    }
    if let Some(caps) = WHATSAPP_NEWSLETTER_JID_RE.captures(&candidate) {
        return Some(format!("{}@newsletter", &caps[1]));
    }
    if is_whatsapp_user_target(&candidate) {
        let phone = extract_user_jid_phone(&candidate)?;
        let normalized = normalize_e164(&phone);
        return (normalized.len() > 1).then_some(normalized);
    }
    if candidate.contains('@') {
        return None;
    }
    if NON_WHATSAPP_PROVIDER_PREFIX_RE.is_match(&candidate) {
        return None;
    }
    let normalized = normalize_e164(&candidate);
    (normalized.len() > 1).then_some(normalized)
}

/// Normalize a single allowFrom entry. `*` passes through; user entries lose
/// the leading `+` (upstream stores digit-only allow entries).
pub fn normalize_whatsapp_allow_from_entry(entry: &str) -> Option<String> {
    if entry == "*" {
        return Some(entry.to_string());
    }
    let normalized = normalize_whatsapp_target(entry)?;
    Some(
        normalized
            .strip_prefix('+')
            .map(str::to_string)
            .unwrap_or(normalized),
    )
}

/// Normalize + dedupe an allowFrom list, dropping unusable entries.
pub fn normalize_whatsapp_allow_from_entries(allow_from: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    allow_from
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .filter_map(normalize_whatsapp_allow_from_entry)
        .filter(|e| seen.insert(e.clone()))
        .collect()
}

pub fn looks_like_whatsapp_target_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.to_lowercase().starts_with("whatsapp:")
        || is_whatsapp_group_jid(trimmed)
        || is_whatsapp_newsletter_jid(trimmed)
        || is_whatsapp_user_target(trimmed)
        || normalize_whatsapp_target(trimmed).is_some()
}

/// Matches direct-user JIDs including hosted/LID domains
/// (`inbound/monitor.ts` `isDirectUserJid`).
pub fn is_direct_user_jid(jid: &str) -> bool {
    DIRECT_USER_JID_RE.is_match(jid.trim())
}

// ============================================================================
// Chat-type classification + session metadata (v2026.5.2 `@newsletter` row)
// ============================================================================

/// Conversation kind derived from a normalized target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhatsAppChatType {
    Direct,
    Group,
    /// `@newsletter` JIDs — WhatsApp channels. Routed with channel session
    /// metadata, never as DMs (v2026.5.2).
    Newsletter,
}

pub fn classify_whatsapp_chat_type(normalized_target: &str) -> WhatsAppChatType {
    if is_whatsapp_newsletter_jid(normalized_target) {
        WhatsAppChatType::Newsletter
    } else if is_whatsapp_group_jid(normalized_target) {
        WhatsAppChatType::Group
    } else {
        WhatsAppChatType::Direct
    }
}

/// Session-metadata `chat_type` label. Newsletter targets are marked
/// `"channel"` — not `"direct"` — so routing never treats them as DMs.
pub fn session_chat_type_label(chat_type: WhatsAppChatType) -> &'static str {
    match chat_type {
        WhatsAppChatType::Direct => "direct",
        WhatsAppChatType::Group => "group",
        WhatsAppChatType::Newsletter => "channel",
    }
}

// ============================================================================
// Outbound target resolution + allowFrom policy
// Port of `resolve-outbound-target.ts` (v2026.7.1).
// ============================================================================

/// A fully-resolved outbound target with its routing classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppTargetResolution {
    /// Canonical target: `+E.164`, `<id>@g.us`, or `<id>@newsletter`.
    pub to: String,
    pub chat_type: WhatsAppChatType,
}

/// Resolve + validate an outbound target against the allowFrom policy.
///
/// Group and newsletter targets bypass the allowlist (the list is a DM
/// policy). An empty allowlist or `*` wildcard admits every user target.
pub fn resolve_whatsapp_outbound_target(
    to: Option<&str>,
    allow_from: Option<&[String]>,
) -> std::result::Result<WhatsAppTargetResolution, String> {
    const MISSING: &str =
        "WhatsApp requires a target: provide <E.164|group JID|newsletter JID>";
    let trimmed = to.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return Err(MISSING.to_string());
    }
    let normalized = normalize_whatsapp_target(trimmed).ok_or_else(|| MISSING.to_string())?;
    let chat_type = classify_whatsapp_chat_type(&normalized);
    if chat_type != WhatsAppChatType::Direct {
        return Ok(WhatsAppTargetResolution {
            to: normalized,
            chat_type,
        });
    }

    let raw_entries: Vec<String> = allow_from
        .unwrap_or(&[])
        .iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();
    let has_wildcard = raw_entries.iter().any(|e| e == "*");
    let allow_list: Vec<String> = raw_entries
        .iter()
        .filter(|e| e.as_str() != "*")
        .filter_map(|e| normalize_whatsapp_target(e))
        .collect();
    if has_wildcard || allow_list.is_empty() || allow_list.contains(&normalized) {
        return Ok(WhatsAppTargetResolution {
            to: normalized,
            chat_type,
        });
    }
    Err(format!(
        "Target \"{normalized}\" is not listed in the configured WhatsApp allowFrom policy."
    ))
}

// ============================================================================
// JID helpers + LID mappings (keyed by authDir)
// Port of `targets-runtime.ts` (v2026.7.1): forward mapping avoids the
// ghost-chat failure mode where sends to `<digits>@s.whatsapp.net` never reach
// LID-internal contacts (#67378).
// ============================================================================

pub fn to_whatsapp_jid(number: &str) -> String {
    let stripped = strip_whatsapp_target_prefixes(number);
    if stripped.contains('@') {
        return stripped;
    }
    let e164 = normalize_e164(&stripped);
    format!("{}@s.whatsapp.net", &e164[1..])
}

fn lid_mapping_dirs(auth_dir: Option<&Path>, extra_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = auth_dir {
        dirs.push(dir.to_path_buf());
    }
    for dir in extra_dirs {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

fn read_lid_mapping_file(path: &Path) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Forward mapping: phone digits → LID digits (`lid-mapping-{digits}.json`).
pub fn read_lid_forward_mapping(phone_digits: &str, auth_dir: Option<&Path>) -> Option<String> {
    for dir in lid_mapping_dirs(auth_dir, &[]) {
        let path = dir.join(format!("lid-mapping-{phone_digits}.json"));
        if let Some(raw) = read_lid_mapping_file(&path) {
            let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }
    None
}

/// Reverse mapping: LID digits → E.164 (`lid-mapping-{lid}_reverse.json`).
pub fn read_lid_reverse_mapping(lid: &str, auth_dir: Option<&Path>) -> Option<String> {
    for dir in lid_mapping_dirs(auth_dir, &[]) {
        let path = dir.join(format!("lid-mapping-{lid}_reverse.json"));
        if let Some(raw) = read_lid_mapping_file(&path) {
            let normalized = normalize_e164(&raw);
            if normalized.len() > 1 {
                return Some(normalized);
            }
        }
    }
    None
}

/// Persist a forward + reverse LID mapping pair keyed by `auth_dir` so future
/// [`to_whatsapp_jid_with_lid`] / [`jid_to_e164`] calls resolve it. The live
/// socket layer calls this whenever Baileys surfaces a PN↔LID association.
pub fn store_lid_mapping(auth_dir: &Path, phone_digits: &str, lid_digits: &str) -> Result<()> {
    std::fs::create_dir_all(auth_dir)?;
    let forward = auth_dir.join(format!("lid-mapping-{phone_digits}.json"));
    let reverse = auth_dir.join(format!("lid-mapping-{lid_digits}_reverse.json"));
    std::fs::write(forward, serde_json::to_string(lid_digits)?)?;
    std::fs::write(reverse, serde_json::to_string(&format!("+{phone_digits}"))?)?;
    Ok(())
}

/// LID-aware outbound JID resolver: prefer `{lid}@lid` over
/// `{digits}@s.whatsapp.net` when a forward mapping exists under `auth_dir`.
pub fn to_whatsapp_jid_with_lid(number: &str, auth_dir: Option<&Path>) -> String {
    let stripped = strip_whatsapp_target_prefixes(number);
    if stripped.contains('@') {
        return stripped;
    }
    let e164 = normalize_e164(&stripped);
    let digits = &e164[1..];
    match read_lid_forward_mapping(digits, auth_dir) {
        Some(lid) => format!("{lid}@lid"),
        None => format!("{digits}@s.whatsapp.net"),
    }
}

/// Convert a user JID (PN or LID domain) to E.164; LID JIDs require a reverse
/// mapping under `auth_dir`, otherwise `None` (inbound is skipped upstream).
pub fn jid_to_e164(jid: &str, auth_dir: Option<&Path>) -> Option<String> {
    static PN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(\d+)(?::\d+)?@(s\.whatsapp\.net|hosted)$").unwrap());
    static LID_DOMAIN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(\d+)(?::\d+)?@(lid|hosted\.lid)$").unwrap());
    if let Some(caps) = PN_RE.captures(jid) {
        return Some(format!("+{}", &caps[1]));
    }
    let caps = LID_DOMAIN_RE.captures(jid)?;
    read_lid_reverse_mapping(&caps[1], auth_dir)
}

// ============================================================================
// `@digits` mention metadata (v2026.7.1 "plugin modernization" row)
// ============================================================================

/// A parsed `@<digits>` mention with the JID Baileys needs in
/// `contextInfo.mentionedJid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsAppMention {
    pub digits: String,
    pub jid: String,
}

/// Extract `@digits` mentions (>= 5 digits, word-bounded) from outbound text.
/// The live socket layer attaches the JIDs as mention metadata on send.
pub fn extract_at_digit_mentions(text: &str) -> Vec<WhatsAppMention> {
    static MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"@(\d{5,})\b").unwrap());
    let mut seen = std::collections::HashSet::new();
    MENTION_RE
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .filter(|d| seen.insert(d.clone()))
        .map(|digits| WhatsAppMention {
            jid: format!("{digits}@s.whatsapp.net"),
            digits,
        })
        .collect()
}

// ============================================================================
// Quoted-message metadata cache
// Port of `quoted-message.ts` (v2026.7.1): outbound replies stay quoted to
// the triggering message even though the outbound path only carries a bare
// messageId. Bot-authored sends are cached with `from_me = true` so quotes of
// our own messages carry correct authorship metadata.
// ============================================================================

const QUOTED_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const QUOTED_CACHE_MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotedMeta {
    pub participant: Option<String>,
    pub participant_e164: Option<String>,
    pub body: Option<String>,
    pub from_me: bool,
}

#[derive(Debug, Clone)]
struct QuotedCacheEntry {
    meta: QuotedMeta,
    inserted: Instant,
}

type QuotedCacheKey = (String, String, String); // (account, remote_jid, message_id)

static QUOTED_META_CACHE: Lazy<parking_lot::Mutex<HashMap<QuotedCacheKey, QuotedCacheEntry>>> =
    Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

pub fn cache_inbound_message_meta(
    account_id: &str,
    remote_jid: &str,
    message_id: &str,
    meta: QuotedMeta,
) {
    if account_id.is_empty() || remote_jid.is_empty() || message_id.is_empty() {
        return;
    }
    let mut cache = QUOTED_META_CACHE.lock();
    if cache.len() >= QUOTED_CACHE_MAX_ENTRIES {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, e)| e.inserted)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        (
            account_id.to_string(),
            remote_jid.to_string(),
            message_id.to_string(),
        ),
        QuotedCacheEntry {
            meta,
            inserted: Instant::now(),
        },
    );
}

/// Cache metadata for a message the bot itself sent (`from_me = true`), so a
/// later quote of that message carries bot-authored quote metadata (row 7).
pub fn cache_bot_authored_message_meta(
    account_id: &str,
    remote_jid: &str,
    message_id: &str,
    body: Option<String>,
) {
    cache_inbound_message_meta(
        account_id,
        remote_jid,
        message_id,
        QuotedMeta {
            participant: None,
            participant_e164: None,
            body,
            from_me: true,
        },
    );
}

pub fn lookup_inbound_message_meta(
    account_id: &str,
    remote_jid: &str,
    message_id: &str,
) -> Option<QuotedMeta> {
    let key = (
        account_id.to_string(),
        remote_jid.to_string(),
        message_id.to_string(),
    );
    let mut cache = QUOTED_META_CACHE.lock();
    let entry = cache.get(&key)?;
    if entry.inserted.elapsed() > QUOTED_CACHE_TTL {
        cache.remove(&key);
        return None;
    }
    Some(entry.meta.clone())
}

fn normalize_comparable_jid(jid: &str) -> Option<String> {
    static DEVICE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r":\d+").unwrap());
    let normalized = DEVICE_RE.replace(jid.trim(), "").to_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

fn is_plain_group_jid(jid: &str) -> bool {
    jid.ends_with("@g.us")
}

fn comparable_jids_equal(left: &str, right: &str, auth_dir: Option<&Path>) -> bool {
    let (Some(l), Some(r)) = (normalize_comparable_jid(left), normalize_comparable_jid(right))
    else {
        return false;
    };
    if l == r {
        return true;
    }
    matches!(
        (jid_to_e164(&l, auth_dir), jid_to_e164(&r, auth_dir)),
        (Some(le), Some(re)) if le == re
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedMetaLookup {
    pub remote_jid: String,
    pub meta: QuotedMeta,
}

/// Cross-conversation quote lookup: exact conversation first, then a scan
/// over equivalent direct-chat JIDs. Group targets never match other
/// conversations; ambiguous matches (two candidates) resolve to `None`.
pub fn lookup_inbound_message_meta_for_target(
    account_id: &str,
    target_jid: &str,
    message_id: &str,
    auth_dir: Option<&Path>,
) -> Option<QuotedMetaLookup> {
    if account_id.is_empty() || target_jid.is_empty() || message_id.is_empty() {
        return None;
    }
    if let Some(meta) = lookup_inbound_message_meta(account_id, target_jid, message_id) {
        return Some(QuotedMetaLookup {
            remote_jid: target_jid.to_string(),
            meta,
        });
    }
    let cache = QUOTED_META_CACHE.lock();
    let mut matched: Option<QuotedMetaLookup> = None;
    for ((account, remote_jid, id), entry) in cache.iter() {
        if account != account_id || id != message_id {
            continue;
        }
        if entry.inserted.elapsed() > QUOTED_CACHE_TTL {
            continue;
        }
        let same_conversation = comparable_jids_equal(target_jid, remote_jid, auth_dir);
        let candidate_ok = if same_conversation {
            true
        } else if is_plain_group_jid(target_jid) || is_plain_group_jid(remote_jid) {
            false
        } else {
            let participant_match = entry
                .meta
                .participant
                .as_deref()
                .map(|p| comparable_jids_equal(target_jid, p, auth_dir))
                .unwrap_or(false);
            let e164_match = matches!(
                (jid_to_e164(target_jid, auth_dir), entry.meta.participant_e164.as_deref()),
                (Some(t), Some(p)) if t.trim() == p.trim() && !t.trim().is_empty()
            );
            participant_match || e164_match
        };
        if !candidate_ok {
            continue;
        }
        if matched.is_some() {
            return None; // ambiguous
        }
        matched = Some(QuotedMetaLookup {
            remote_jid: remote_jid.clone(),
            meta: entry.meta.clone(),
        });
    }
    matched
}

/// The quote key + preview text a live socket passes to Baileys
/// (`MiscMessageGenerationOptions.quoted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedMessageOptions {
    pub remote_jid: String,
    pub id: String,
    pub from_me: bool,
    pub participant: Option<String>,
    /// Original message text — shown in the quote preview bubble.
    pub message_text: String,
}

pub fn build_quoted_message_options(
    message_id: Option<&str>,
    remote_jid: Option<&str>,
    from_me: bool,
    participant: Option<&str>,
    message_text: Option<&str>,
) -> Option<QuotedMessageOptions> {
    let id = message_id.map(str::trim).filter(|s| !s.is_empty())?;
    let jid = remote_jid.map(str::trim).filter(|s| !s.is_empty())?;
    Some(QuotedMessageOptions {
        remote_jid: jid.to_string(),
        id: id.to_string(),
        from_me,
        participant: participant.map(str::to_string),
        message_text: message_text.unwrap_or("").to_string(),
    })
}

// ============================================================================
// Quoted-image reply media (v2026.5.2 row + v2026.7.1 context/media row)
// ============================================================================

/// Decision for media attached to a *quoted* message in a reply context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotedMediaPlan {
    /// Download the quoted image and save it as **inbound media** so the agent
    /// sees it exactly like directly-attached media (v2026.5.2).
    SaveAsInboundMedia { mimetype: String },
    SkipNotDownloadable,
    SkipTooLarge { max_bytes: u64 },
    SkipUnsupportedType,
}

/// Classify quoted-media from a reply context. Only downloadable images are
/// captured; the live socket layer performs the actual `downloadMediaMessage`
/// call and must re-check the byte size with [`admit_inbound_media`].
pub fn plan_quoted_media_capture(
    quoted_mimetype: Option<&str>,
    has_downloadable_payload: bool,
    declared_size_bytes: Option<u64>,
    max_bytes: u64,
) -> QuotedMediaPlan {
    let Some(mimetype) = quoted_mimetype.map(str::trim).filter(|m| !m.is_empty()) else {
        return QuotedMediaPlan::SkipUnsupportedType;
    };
    if !mimetype.to_lowercase().starts_with("image/") {
        return QuotedMediaPlan::SkipUnsupportedType;
    }
    if !has_downloadable_payload {
        return QuotedMediaPlan::SkipNotDownloadable;
    }
    if let Some(size) = declared_size_bytes {
        if size > max_bytes {
            return QuotedMediaPlan::SkipTooLarge { max_bytes };
        }
    }
    QuotedMediaPlan::SaveAsInboundMedia {
        mimetype: mimetype.to_string(),
    }
}

// ============================================================================
// Suffix-only streaming (v2026.7.1 "plugin modernization" row)
// ============================================================================

/// Compute the delta to emit for a streamed block: when a provider re-sends a
/// cumulative payload, emit only the **new suffix**, never the cumulative
/// preamble. Non-prefix payloads are distinct blocks and pass through whole.
/// Returns `None` when there is nothing new to send.
pub fn compute_streaming_suffix(already_sent: &str, incoming: &str) -> Option<String> {
    if incoming.is_empty() || incoming == already_sent {
        return None;
    }
    if already_sent.is_empty() {
        return Some(incoming.to_string());
    }
    if let Some(suffix) = incoming.strip_prefix(already_sent) {
        return (!suffix.is_empty()).then(|| suffix.to_string());
    }
    Some(incoming.to_string())
}

// ============================================================================
// Status-reaction lifecycle (v2026.7.1 "plugin modernization" row)
// Port of `auto-reply/monitor/status-reaction.ts` + the plugin-sdk
// `createStatusReactionController` lifecycle: queued → thinking → tool →
// done/error.
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusReactionStage {
    Queued,
    Thinking,
    Tool,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionUpdate {
    Set(String),
    Clear,
}

/// Emoji set for each lifecycle stage. `done`/`error` of `None` clear the
/// reaction on terminal transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReactionEmojis {
    pub queued: String,
    pub thinking: String,
    pub tool: String,
    pub done: Option<String>,
    pub error: Option<String>,
}

impl Default for StatusReactionEmojis {
    fn default() -> Self {
        Self {
            queued: "👀".to_string(),
            thinking: "🤔".to_string(),
            tool: "🛠️".to_string(),
            done: None,
            error: Some("⚠️".to_string()),
        }
    }
}

impl StatusReactionEmojis {
    pub fn from_config(cfg: Option<&WhatsAppStatusReactionsConfig>) -> Self {
        let mut emojis = Self::default();
        if let Some(cfg) = cfg {
            if let Some(q) = &cfg.queued {
                emojis.queued = q.clone();
            }
            if let Some(t) = &cfg.thinking {
                emojis.thinking = t.clone();
            }
            if let Some(t) = &cfg.tool {
                emojis.tool = t.clone();
            }
            if cfg.done.is_some() {
                emojis.done = cfg.done.clone().filter(|s| !s.is_empty());
            }
            if cfg.error.is_some() {
                emojis.error = cfg.error.clone().filter(|s| !s.is_empty());
            }
        }
        emojis
    }
}

/// Ack-reaction gating — port of the plugin-sdk `shouldAckReactionForWhatsApp`
/// decision used by `status-reaction.ts`.
#[allow(clippy::too_many_arguments)]
pub fn should_ack_reaction(
    emoji: &str,
    is_direct: bool,
    is_group: bool,
    direct_enabled: bool,
    group_mode: &str,
    was_mentioned: bool,
    group_activated: bool,
) -> bool {
    if emoji.trim().is_empty() {
        return false;
    }
    if is_direct {
        return direct_enabled;
    }
    if is_group {
        return match group_mode {
            "always" => true,
            "off" => false,
            // "mentions" (default): react when addressed or always-activated.
            _ => was_mentioned || group_activated,
        };
    }
    false
}

/// Status-reaction state machine. The live socket layer maps
/// [`ReactionUpdate`] to `sendMessage(react)` calls; terminal stages bypass
/// the min-interval throttle so the final state always lands.
#[derive(Debug)]
pub struct StatusReactionController {
    emojis: StatusReactionEmojis,
    stage: Option<StatusReactionStage>,
    terminal: bool,
    min_update_interval_ms: u64,
    last_update_at_ms: Option<u64>,
}

impl StatusReactionController {
    pub fn new(emojis: StatusReactionEmojis, min_update_interval_ms: u64) -> Self {
        Self {
            emojis,
            stage: None,
            terminal: false,
            min_update_interval_ms,
            last_update_at_ms: None,
        }
    }

    pub fn stage(&self) -> Option<StatusReactionStage> {
        self.stage
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn emoji_for(&self, stage: StatusReactionStage) -> Option<String> {
        match stage {
            StatusReactionStage::Queued => Some(self.emojis.queued.clone()),
            StatusReactionStage::Thinking => Some(self.emojis.thinking.clone()),
            StatusReactionStage::Tool => Some(self.emojis.tool.clone()),
            StatusReactionStage::Done => self.emojis.done.clone(),
            StatusReactionStage::Error => self.emojis.error.clone(),
        }
    }

    /// Advance the lifecycle at `now_ms`. Returns the reaction update to
    /// perform, or `None` (duplicate stage, throttled, or already terminal).
    pub fn advance_at(
        &mut self,
        stage: StatusReactionStage,
        now_ms: u64,
    ) -> Option<ReactionUpdate> {
        if self.terminal || self.stage == Some(stage) {
            return None;
        }
        let is_terminal_stage =
            matches!(stage, StatusReactionStage::Done | StatusReactionStage::Error);
        // Never move back to Queued after leaving it.
        if stage == StatusReactionStage::Queued && self.stage.is_some() {
            return None;
        }
        self.stage = Some(stage);
        if is_terminal_stage {
            self.terminal = true;
            self.last_update_at_ms = Some(now_ms);
            return Some(match self.emoji_for(stage) {
                Some(emoji) => ReactionUpdate::Set(emoji),
                None => ReactionUpdate::Clear,
            });
        }
        if let Some(last) = self.last_update_at_ms {
            if now_ms.saturating_sub(last) < self.min_update_interval_ms {
                return None; // throttled — stage recorded, no send
            }
        }
        self.last_update_at_ms = Some(now_ms);
        self.emoji_for(stage).map(ReactionUpdate::Set)
    }
}

// ============================================================================
// Socket timing + serialized per-account sends
// Port of `socket-timing.ts` (v2026.7.1).
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhatsAppSocketTiming {
    pub keep_alive_interval_ms: u64,
    pub connect_timeout_ms: u64,
    pub default_query_timeout_ms: u64,
}

pub const DEFAULT_WHATSAPP_SOCKET_TIMING: WhatsAppSocketTiming = WhatsAppSocketTiming {
    keep_alive_interval_ms: 25_000,
    connect_timeout_ms: 60_000,
    default_query_timeout_ms: 60_000,
};

fn positive_ms(value: Option<u64>) -> Option<u64> {
    value.filter(|v| *v > 0)
}

pub fn resolve_whatsapp_socket_timing(
    configured: Option<&WhatsAppSocketTimingConfig>,
) -> WhatsAppSocketTiming {
    WhatsAppSocketTiming {
        keep_alive_interval_ms: positive_ms(configured.and_then(|c| c.keep_alive_interval_ms))
            .unwrap_or(DEFAULT_WHATSAPP_SOCKET_TIMING.keep_alive_interval_ms),
        connect_timeout_ms: positive_ms(configured.and_then(|c| c.connect_timeout_ms))
            .unwrap_or(DEFAULT_WHATSAPP_SOCKET_TIMING.connect_timeout_ms),
        default_query_timeout_ms: positive_ms(configured.and_then(|c| c.default_query_timeout_ms))
            .unwrap_or(DEFAULT_WHATSAPP_SOCKET_TIMING.default_query_timeout_ms),
    }
}

pub fn resolve_socket_operation_timeout_ms(timeout_ms: u64) -> u64 {
    if timeout_ms > 0 {
        timeout_ms
    } else {
        DEFAULT_WHATSAPP_SOCKET_TIMING.default_query_timeout_ms
    }
}

static SEND_MUTEXES: Lazy<DashMap<String, Arc<tokio::sync::Mutex<()>>>> = Lazy::new(DashMap::new);

/// Serialize overlapping WhatsApp Web sends per account (v2026.7.1 durability
/// row): Baileys sockets corrupt message ordering under concurrent sends, so
/// every outbound operation for one account runs behind a FIFO mutex.
pub async fn with_serialized_account_send<T, Fut>(account_id: &str, fut: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let lock = SEND_MUTEXES
        .entry(account_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .value()
        .clone();
    let _guard = lock.lock().await;
    fut.await
}

// ============================================================================
// Reconnect policy + bounded catch-up
// Port of `reconnect.ts` + `inbound/monitor.ts` append-reply window
// (v2026.7.1).
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectPolicy {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub factor: f64,
    pub jitter: f64,
    pub max_attempts: u32,
}

pub const DEFAULT_RECONNECT_POLICY: ReconnectPolicy = ReconnectPolicy {
    initial_ms: 2_000,
    max_ms: 30_000,
    factor: 1.8,
    jitter: 0.25,
    max_attempts: 12,
};

pub fn resolve_reconnect_policy(configured: Option<&WhatsAppReconnectConfig>) -> ReconnectPolicy {
    let mut merged = DEFAULT_RECONNECT_POLICY;
    if let Some(cfg) = configured {
        if let Some(v) = cfg.initial_ms {
            merged.initial_ms = v;
        }
        if let Some(v) = cfg.max_ms {
            merged.max_ms = v;
        }
        if let Some(v) = cfg.factor {
            merged.factor = v;
        }
        if let Some(v) = cfg.jitter {
            merged.jitter = v;
        }
        if let Some(v) = cfg.max_attempts {
            merged.max_attempts = v;
        }
    }
    merged.initial_ms = merged.initial_ms.max(250);
    merged.max_ms = merged.max_ms.max(merged.initial_ms);
    merged.factor = merged.factor.clamp(1.1, 10.0);
    merged.jitter = merged.jitter.clamp(0.0, 1.0);
    merged
}

/// Compute a backoff delay for `attempt` (0-based). `jitter_seed` in `[0, 1)`
/// keeps the function pure — `0.5` yields the undithered midpoint.
pub fn compute_backoff_ms(policy: &ReconnectPolicy, attempt: u32, jitter_seed: f64) -> u64 {
    let base = (policy.initial_ms as f64) * policy.factor.powi(attempt as i32);
    let capped = base.min(policy.max_ms as f64);
    let jitter_span = capped * policy.jitter;
    let jittered = capped + jitter_span * (jitter_seed.clamp(0.0, 1.0) * 2.0 - 1.0);
    jittered.max(0.0).round() as u64
}

pub fn should_attempt_reconnect(policy: &ReconnectPolicy, attempt: u32) -> bool {
    attempt < policy.max_attempts
}

/// A message observed while offline, considered for post-reconnect catch-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupCandidate {
    pub id: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchupWindow {
    pub max_age_ms: u64,
    pub max_count: usize,
}

pub const DEFAULT_CATCHUP_WINDOW: CatchupWindow = CatchupWindow {
    max_age_ms: 10 * 60 * 1000,
    max_count: 50,
};

/// Bounded reconnect catch-up (v2026.7.1 durability row): after a reconnect,
/// only process messages newer than the last processed timestamp, within the
/// age window, keeping at most the `max_count` most recent — never an
/// unbounded replay.
pub fn filter_reconnect_catchup(
    mut candidates: Vec<CatchupCandidate>,
    last_processed_ts_ms: u64,
    now_ms: u64,
    window: &CatchupWindow,
) -> Vec<CatchupCandidate> {
    candidates.sort_by_key(|c| c.timestamp_ms);
    let min_ts = now_ms.saturating_sub(window.max_age_ms);
    let mut kept: Vec<CatchupCandidate> = candidates
        .into_iter()
        .filter(|c| c.timestamp_ms > last_processed_ts_ms && c.timestamp_ms >= min_ts)
        .collect();
    if kept.len() > window.max_count {
        kept.drain(0..kept.len() - window.max_count);
    }
    kept
}

// ============================================================================
// Connection teardown + login-wait state machine
// Port of `connection-controller.ts` (v2026.7.1).
// ============================================================================

pub const LOGGED_OUT_STATUS: u16 = 401;
pub const TIMED_OUT_STATUS: u16 = 408;
pub const CONNECTION_REPLACED_STATUS: u16 = 440;
pub const POST_PAIRING_RESTART_STATUS: u16 = 515;

/// Ordered teardown steps for a long-lived socket (v2026.5.2 row):
/// a graceful Baileys `end(error)` must precede the raw WebSocket close so
/// keep-alive timers and pending queries settle before the transport drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketTeardownStep {
    EndWithError,
    RawClose,
}

pub const SOCKET_TEARDOWN_SEQUENCE: [SocketTeardownStep; 2] =
    [SocketTeardownStep::EndWithError, SocketTeardownStep::RawClose];

/// Abstraction over the two teardown operations a live socket exposes
/// (`sock.end(err)` and `sock.ws.close()`).
pub trait WaSocketTeardown {
    fn end_with_error(&mut self, reason: &str);
    fn raw_ws_close(&mut self);
}

/// Port of `closeWaSocket` (`connection-controller.ts:170`): best-effort
/// graceful end **before** the raw close; both steps swallow errors upstream.
pub fn close_wa_socket<T: WaSocketTeardown>(sock: &mut T) {
    sock.end_with_error("OpenClaw WhatsApp socket close");
    sock.raw_ws_close();
}

/// What to do when a *long-lived* (post-login) connection drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectDecision {
    /// Transient failure: reconnect with backoff (counted against
    /// `max_attempts`).
    ReconnectBackoff,
    /// 515 restart-required: replace the socket immediately, not counted.
    RestartImmediate,
    /// 401 logged out: terminal — stop the restart loop; server invalidated
    /// the session so creds must be cleared before a fresh pairing.
    StopLoggedOut,
    /// 440 connection replaced (another Web session took over): terminal —
    /// stop the restart loop but preserve auth state.
    StopReplaced,
}

pub fn classify_long_lived_disconnect(status_code: Option<u16>) -> DisconnectDecision {
    match status_code {
        Some(LOGGED_OUT_STATUS) => DisconnectDecision::StopLoggedOut,
        Some(CONNECTION_REPLACED_STATUS) => DisconnectDecision::StopReplaced,
        Some(POST_PAIRING_RESTART_STATUS) => DisconnectDecision::RestartImmediate,
        _ => DisconnectDecision::ReconnectBackoff,
    }
}

/// Terminal disconnects end the restart loop (v2026.7.1 durability row:
/// "stop restart loops after logout/replaced").
pub fn is_terminal_disconnect(decision: DisconnectDecision) -> bool {
    matches!(
        decision,
        DisconnectDecision::StopLoggedOut | DisconnectDecision::StopReplaced
    )
}

/// Auth-state preservation policy (v2026.7.1 durability row): every
/// disconnect — including terminal `replaced` — preserves the persisted auth
/// state. Only an explicit server logout (401) clears credentials.
pub fn should_clear_auth_on_disconnect(decision: DisconnectDecision) -> bool {
    matches!(decision, DisconnectDecision::StopLoggedOut)
}

/// Persisted-credentials state observed when a login socket opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredsPersistState {
    Persisted,
    Missing,
    Unstable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginWaitEvent {
    /// The socket reported `connection: "open"`; `creds` is the *durably
    /// persisted* credential state (not the in-memory state).
    ConnectionOpen { creds: CredsPersistState },
    ConnectionError { status_code: Option<u16> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFailureReason {
    /// Creds file flapping while login was reported — fail closed.
    AuthUnstable,
    /// Socket opened but creds never landed on disk: login must not be
    /// reported successful before a durable persist (v2026.7.1).
    AuthNotPersisted,
    /// Logged out twice: relink required.
    LoggedOut,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginWaitAction {
    /// Login complete; `restarted` mirrors upstream's restart bookkeeping.
    Succeed { restarted: bool },
    /// Tear down (unless `close_current` is false — the logged-out path
    /// already closed) and create a replacement socket, optionally clearing
    /// auth state first.
    ReplaceSocket { close_current: bool, clear_auth: bool },
    Fail(LoginFailureReason),
}

/// Login-wait state machine — port of `waitForWhatsAppLogin`
/// (`connection-controller.ts:225`). Each restart class fires at most once:
/// - 515 post-pairing restart,
/// - 408 pairing timeout / expired terminal QR → replacement socket retry,
/// - 401 logged out → clear auth, one fresh-pairing retry.
#[derive(Debug, Default)]
pub struct LoginWaitMachine {
    post_pairing_restarted: bool,
    timeout_restarted: bool,
    logged_out_restarted: bool,
}

impl LoginWaitMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn restarted(&self) -> bool {
        self.post_pairing_restarted || self.timeout_restarted || self.logged_out_restarted
    }

    pub fn on_event(&mut self, event: LoginWaitEvent) -> LoginWaitAction {
        match event {
            LoginWaitEvent::ConnectionOpen { creds } => match creds {
                CredsPersistState::Unstable => {
                    LoginWaitAction::Fail(LoginFailureReason::AuthUnstable)
                }
                CredsPersistState::Missing => {
                    LoginWaitAction::Fail(LoginFailureReason::AuthNotPersisted)
                }
                CredsPersistState::Persisted => LoginWaitAction::Succeed {
                    restarted: self.restarted(),
                },
            },
            LoginWaitEvent::ConnectionError { status_code } => match status_code {
                Some(POST_PAIRING_RESTART_STATUS) if !self.post_pairing_restarted => {
                    self.post_pairing_restarted = true;
                    LoginWaitAction::ReplaceSocket {
                        close_current: true,
                        clear_auth: false,
                    }
                }
                Some(TIMED_OUT_STATUS) if !self.timeout_restarted => {
                    self.timeout_restarted = true;
                    LoginWaitAction::ReplaceSocket {
                        close_current: true,
                        clear_auth: false,
                    }
                }
                Some(LOGGED_OUT_STATUS) => {
                    if self.logged_out_restarted {
                        LoginWaitAction::Fail(LoginFailureReason::LoggedOut)
                    } else {
                        self.logged_out_restarted = true;
                        // Upstream closes the current socket, clears auth via
                        // logoutWeb, then creates the replacement without a
                        // second close.
                        LoginWaitAction::ReplaceSocket {
                            close_current: true,
                            clear_auth: true,
                        }
                    }
                }
                _ => LoginWaitAction::Fail(LoginFailureReason::Other),
            },
        }
    }
}

// ============================================================================
// Credential durability
// Port of `creds-files.ts`, `creds-persistence.ts`, `auth-store.ts`
// (v2026.7.1).
// ============================================================================

pub fn resolve_web_creds_path(auth_dir: &Path) -> PathBuf {
    auth_dir.join("creds.json")
}

pub fn resolve_web_creds_backup_path(auth_dir: &Path) -> PathBuf {
    auth_dir.join("creds.json.bak")
}

/// Reject symlinked or non-regular `creds.json` paths (v2026.7.1 durability
/// row). Uses `std::fs::symlink_metadata` so the symlink itself — not its
/// target — is inspected. Missing files are fine.
pub fn assert_web_creds_regular_file_or_missing(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow::anyhow!(
            "WhatsApp credential file path is unsafe; cannot stat {}: {err}",
            path.display()
        )),
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                anyhow::bail!(
                    "WhatsApp credential file path is unsafe; creds.json must be a regular \
                     file or missing: {}",
                    path.display()
                );
            }
            Ok(())
        }
    }
}

pub fn is_valid_json(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw).is_ok()
}

/// Raw creds read: `None` for missing, unsafe, empty (<= 1 byte), or
/// symlinked files.
pub fn read_web_creds_json_raw(path: &Path) -> Option<String> {
    assert_web_creds_regular_file_or_missing(path).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    (raw.len() > 1).then_some(raw)
}

/// Atomically replace `creds.json` (temp file + rename in the same
/// directory), backing up the previous valid content to `creds.json.bak`
/// first. File mode 0600 on Unix. This is the "durable persist" step that
/// must complete before login success is reported.
pub fn write_web_creds_atomically(auth_dir: &Path, content: &str) -> Result<()> {
    std::fs::create_dir_all(auth_dir)?;
    let creds_path = resolve_web_creds_path(auth_dir);
    assert_web_creds_regular_file_or_missing(&creds_path)?;
    // Preserve the previous good creds as the malformed-restore source.
    if let Some(existing) = read_web_creds_json_raw(&creds_path) {
        if is_valid_json(&existing) {
            std::fs::write(resolve_web_creds_backup_path(auth_dir), existing)?;
        }
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".creds")
        .tempfile_in(auth_dir)?;
    use std::io::Write as _;
    tmp.write_all(content.as_bytes())?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(&creds_path)
        .map_err(|e| anyhow::anyhow!("failed to persist creds.json atomically: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredsReadOutcome {
    /// `creds.json` was present and valid.
    Primary(String),
    /// `creds.json` was missing/malformed but `creds.json.bak` was valid; the
    /// backup content was restored over the primary file.
    RestoredFromBackup(String),
    Missing,
}

/// Read credentials with malformed-restore (v2026.7.1 durability row): a
/// corrupt/truncated `creds.json` falls back to the last valid backup, which
/// is copied back over the primary path atomically.
pub fn read_web_creds_with_backup_restore(auth_dir: &Path) -> Result<CredsReadOutcome> {
    let creds_path = resolve_web_creds_path(auth_dir);
    assert_web_creds_regular_file_or_missing(&creds_path)?;
    if let Some(raw) = read_web_creds_json_raw(&creds_path) {
        if is_valid_json(&raw) {
            return Ok(CredsReadOutcome::Primary(raw));
        }
    }
    let backup_path = resolve_web_creds_backup_path(auth_dir);
    let Some(backup_raw) = read_web_creds_json_raw(&backup_path) else {
        return Ok(CredsReadOutcome::Missing);
    };
    if !is_valid_json(&backup_raw) {
        return Ok(CredsReadOutcome::Missing);
    }
    write_web_creds_atomically(auth_dir, &backup_raw)?;
    Ok(CredsReadOutcome::RestoredFromBackup(backup_raw))
}

// ============================================================================
// Outbound drain queue (v2026.7.1 durability row)
// ============================================================================

/// Flush deadline for the pre-close drain, mirroring
/// `INBOUND_CLOSE_DRAIN_TIMEOUT_MS` (`inbound/monitor.ts:113`).
pub const DRAIN_FLUSH_TIMEOUT_MS: u64 = 5_000;
/// Suggested periodic drain tick for the live socket layer.
pub const DEFAULT_OUTBOUND_DRAIN_INTERVAL_MS: u64 = 2_000;
const DEFAULT_OUTBOUND_QUEUE_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedOutbound {
    pub to: String,
    pub text: String,
    pub enqueued_at_ms: u64,
}

/// Bounded FIFO of outbound payloads accepted while the socket is down. A
/// live socket drains it on a periodic tick ([`DEFAULT_OUTBOUND_DRAIN_INTERVAL_MS`])
/// and before socket close (bounded by [`DRAIN_FLUSH_TIMEOUT_MS`]). When
/// full, the oldest payload is dropped (returned) rather than blocking.
#[derive(Debug)]
pub struct OutboundDrainQueue {
    inner: parking_lot::Mutex<VecDeque<QueuedOutbound>>,
    max_len: usize,
}

impl Default for OutboundDrainQueue {
    fn default() -> Self {
        Self::new(DEFAULT_OUTBOUND_QUEUE_MAX)
    }
}

impl OutboundDrainQueue {
    pub fn new(max_len: usize) -> Self {
        Self {
            inner: parking_lot::Mutex::new(VecDeque::new()),
            max_len: max_len.max(1),
        }
    }

    /// Enqueue; returns the dropped oldest payload when the bound is hit.
    pub fn enqueue(&self, payload: QueuedOutbound) -> Option<QueuedOutbound> {
        let mut queue = self.inner.lock();
        let dropped = if queue.len() >= self.max_len {
            queue.pop_front()
        } else {
            None
        };
        queue.push_back(payload);
        dropped
    }

    /// Take up to `max` payloads in FIFO order.
    pub fn drain_batch(&self, max: usize) -> Vec<QueuedOutbound> {
        let mut queue = self.inner.lock();
        let take = max.min(queue.len());
        queue.drain(0..take).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

static OUTBOUND_DRAIN_QUEUES: Lazy<DashMap<String, Arc<OutboundDrainQueue>>> =
    Lazy::new(DashMap::new);

/// Per-account outbound drain queue shared between the ChannelPlugin send
/// path and the (future) live socket layer.
pub fn outbound_drain_queue(account_id: &str) -> Arc<OutboundDrainQueue> {
    OUTBOUND_DRAIN_QUEUES
        .entry(account_id.to_string())
        .or_insert_with(|| Arc::new(OutboundDrainQueue::default()))
        .value()
        .clone()
}

// ============================================================================
// Group policy: visible-reply default, history retention, session keys
// Port of `group-policy.ts`, `group-session-key.ts` + inbound-dispatch group
// history handling (v2026.7.1).
// ============================================================================

/// Group visible-reply policy (v2026.7.1 "plugin modernization" row): in
/// groups the default is **message-tool-only** — the agent's implicit text
/// reply is suppressed and only explicit message-tool sends are visible.
/// Direct chats always show replies.
pub fn resolve_group_visible_reply_mode(
    configured: Option<WhatsAppGroupVisibleReplyMode>,
    is_group: bool,
) -> WhatsAppGroupVisibleReplyMode {
    if !is_group {
        return WhatsAppGroupVisibleReplyMode::Always;
    }
    configured.unwrap_or(WhatsAppGroupVisibleReplyMode::MessageToolOnly)
}

/// Whether an implicit (non-message-tool) reply should be visibly delivered.
pub fn implicit_reply_visible(mode: WhatsAppGroupVisibleReplyMode) -> bool {
    matches!(mode, WhatsAppGroupVisibleReplyMode::Always)
}

pub const DEFAULT_ACCOUNT_ID: &str = "default";
const DEFAULT_GROUP_HISTORY_LIMIT: usize = 50;

pub fn normalize_account_id(account_id: Option<&str>) -> String {
    let normalized = account_id.map(str::trim).unwrap_or("").to_lowercase();
    if normalized.is_empty() {
        DEFAULT_ACCOUNT_ID.to_string()
    } else {
        normalized
    }
}

/// Port of `resolveWhatsAppGroupSessionKey`: non-default accounts running the
/// same group get a per-account thread suffix so their sessions never collide.
pub fn resolve_whatsapp_group_session_key(session_key: &str, account_id: Option<&str>) -> String {
    let account = normalize_account_id(account_id);
    if account == DEFAULT_ACCOUNT_ID || !session_key.contains(":group:") {
        return session_key.to_string();
    }
    format!("{session_key}:thread:whatsapp-account-{account}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHistoryEntry {
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub message_id: Option<String>,
}

static GROUP_HISTORY: Lazy<DashMap<(String, String), VecDeque<GroupHistoryEntry>>> =
    Lazy::new(DashMap::new);

/// Record a group message into the retained context window. Keyed by
/// `(account, group JID)` — deliberately **not** by socket — so the context
/// survives send retries and socket reconnects (v2026.7.1 context row).
pub fn record_group_history(
    account_id: &str,
    group_jid: &str,
    entry: GroupHistoryEntry,
    limit: Option<usize>,
) {
    let limit = limit.unwrap_or(DEFAULT_GROUP_HISTORY_LIMIT).max(1);
    let mut history = GROUP_HISTORY
        .entry((normalize_account_id(Some(account_id)), group_jid.to_string()))
        .or_default();
    history.push_back(entry);
    while history.len() > limit {
        history.pop_front();
    }
}

pub fn group_history_snapshot(account_id: &str, group_jid: &str) -> Vec<GroupHistoryEntry> {
    GROUP_HISTORY
        .get(&(normalize_account_id(Some(account_id)), group_jid.to_string()))
        .map(|h| h.iter().cloned().collect())
        .unwrap_or_default()
}

/// Only an explicit logout drops retained group context.
pub fn clear_group_history_for_account(account_id: &str) {
    let account = normalize_account_id(Some(account_id));
    GROUP_HISTORY.retain(|(acct, _), _| *acct != account);
}

// ============================================================================
// Media: inbound admission caps, document filenames, outbound media mode
// Port of `inbound/media.ts`, `document-filename.ts` (v2026.7.1).
// ============================================================================

pub const DEFAULT_MEDIA_MAX_MB: u64 = 50;

pub fn media_max_bytes(media_max_mb: Option<u64>) -> u64 {
    media_max_mb.unwrap_or(DEFAULT_MEDIA_MAX_MB) * 1024 * 1024
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("Media exceeds {}MB limit", .max_bytes / (1024 * 1024))]
pub struct WhatsAppInboundMediaLimitExceeded {
    pub max_bytes: u64,
}

/// Inbound media admission (v2026.7.1 context/media row): declared or actual
/// sizes over the `mediaMaxMb` cap are rejected before download/save.
pub fn admit_inbound_media(
    size_bytes: u64,
    max_bytes: u64,
) -> std::result::Result<(), WhatsAppInboundMediaLimitExceeded> {
    if size_bytes > max_bytes {
        Err(WhatsAppInboundMediaLimitExceeded { max_bytes })
    } else {
        Ok(())
    }
}

/// Reverse of [`resolve_mime_type`]: extension (with dot) for common MIME
/// types (plugin-sdk `extensionForMime`).
pub fn extension_for_mime(mimetype: Option<&str>) -> Option<&'static str> {
    let mime = mimetype?.trim().split(';').next()?.trim().to_lowercase();
    Some(match mime.as_str() {
        "text/html" => ".html",
        "application/xml" | "text/xml" => ".xml",
        "text/css" => ".css",
        "application/javascript" | "text/javascript" => ".js",
        "application/json" => ".json",
        "application/pdf" => ".pdf",
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "video/mp4" => ".mp4",
        "audio/mpeg" => ".mp3",
        "audio/ogg" => ".ogg",
        "text/plain" => ".txt",
        _ => return None,
    })
}

const WHATSAPP_DEFAULT_DOCUMENT_FILE_NAME: &str = "file";

/// Port of `resolveWhatsAppDocumentFileName`: strip ASCII control characters;
/// empty names fall back to `file<.ext-from-mime>` (MIME-derived filenames,
/// v2026.7.1 context/media row).
pub fn resolve_whatsapp_document_file_name(
    file_name: Option<&str>,
    mimetype: Option<&str>,
) -> String {
    let stripped: String = file_name
        .unwrap_or("")
        .chars()
        .filter(|c| {
            let code = *c as u32;
            code > 0x1f && code != 0x7f
        })
        .collect();
    let trimmed = stripped.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    match extension_for_mime(mimetype) {
        Some(ext) => format!("{WHATSAPP_DEFAULT_DOCUMENT_FILE_NAME}{ext}"),
        None => WHATSAPP_DEFAULT_DOCUMENT_FILE_NAME.to_string(),
    }
}

/// How an outbound media payload is packaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundMediaMode {
    Image,
    Video,
    Audio,
    /// Sent as a document with the **original bytes** — no image
    /// re-encode/optimization pass (v2026.7.1 `forceDocument`/`asDocument`).
    Document { preserve_original_bytes: bool },
}

pub fn resolve_outbound_media_mode(
    force_document: bool,
    as_document: bool,
    mimetype: Option<&str>,
) -> OutboundMediaMode {
    if force_document || as_document {
        return OutboundMediaMode::Document {
            preserve_original_bytes: true,
        };
    }
    let mime = mimetype.unwrap_or("").trim().to_lowercase();
    if mime.starts_with("image/") {
        OutboundMediaMode::Image
    } else if mime.starts_with("video/") {
        OutboundMediaMode::Video
    } else if mime.starts_with("audio/") {
        OutboundMediaMode::Audio
    } else {
        OutboundMediaMode::Document {
            preserve_original_bytes: true,
        }
    }
}

// ============================================================================
// Reachout timelock (v2026.7.1 context/media row)
// Port of `inbound/monitor.ts` reachout-timelock gating: while an enforcement
// window is active, *direct* outbound messages are blocked; groups and
// newsletters are unaffected.
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReachoutTimelockState {
    pub is_active: bool,
    pub enforcement_type: Option<String>,
    /// Unix ms when enforcement ends; `None` = indefinite while active.
    pub ends_at_ms: Option<u64>,
}

/// Returns the state only while it is actually enforcing at `now_ms`.
pub fn active_reachout_timelock(
    state: Option<&ReachoutTimelockState>,
    now_ms: u64,
) -> Option<&ReachoutTimelockState> {
    let state = state?;
    if !state.is_active {
        return None;
    }
    match state.ends_at_ms {
        Some(ends) if ends <= now_ms => None,
        _ => Some(state),
    }
}

pub fn format_reachout_timelock_error(state: &ReachoutTimelockState) -> String {
    let mut details: Vec<String> = Vec::new();
    if let Some(t) = &state.enforcement_type {
        details.push(format!("type={t}"));
    }
    if let Some(ends) = state.ends_at_ms {
        details.push(format!("until_ms={ends}"));
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    format!(
        "WhatsApp reachout timelock is active; direct messages are temporarily blocked{suffix}"
    )
}

static REACHOUT_TIMELOCKS: Lazy<DashMap<String, ReachoutTimelockState>> = Lazy::new(DashMap::new);

pub fn set_reachout_timelock(account_id: &str, state: ReachoutTimelockState) {
    REACHOUT_TIMELOCKS.insert(normalize_account_id(Some(account_id)), state);
}

pub fn clear_reachout_timelock(account_id: &str) {
    REACHOUT_TIMELOCKS.remove(&normalize_account_id(Some(account_id)));
}

/// Gate an outbound send: only direct-user targets are blocked while a
/// timelock is enforcing.
pub fn check_reachout_timelock(
    account_id: &str,
    target: &str,
    now_ms: u64,
) -> std::result::Result<(), String> {
    let jid_like = if target.starts_with('+') {
        to_whatsapp_jid(target)
    } else {
        target.to_string()
    };
    if !is_direct_user_jid(&jid_like) {
        return Ok(());
    }
    let account = normalize_account_id(Some(account_id));
    if let Some(state) = REACHOUT_TIMELOCKS.get(&account) {
        if let Some(active) = active_reachout_timelock(Some(state.value()), now_ms) {
            return Err(format_reachout_timelock_error(active));
        }
    }
    Ok(())
}

// ============================================================================
// `/tts latest` read-aloud (v2026.4.25 row)
// Port of `src/auto-reply/reply/commands-tts.ts`: command parsing + selection
// of the latest assistant reply for TTS synthesis handoff.
// ============================================================================

pub const SILENT_REPLY_TOKEN: &str = "NO_REPLY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTtsCommand {
    pub action: String,
    pub args: String,
}

/// Port of `parseTtsCommand`: `/tts` alone maps to `status`; otherwise the
/// first token is the action and the rest are args.
pub fn parse_tts_command(normalized: &str) -> Option<ParsedTtsCommand> {
    if normalized == "/tts" {
        return Some(ParsedTtsCommand {
            action: "status".to_string(),
            args: String::new(),
        });
    }
    let rest = normalized.strip_prefix("/tts ")?.trim();
    if rest.is_empty() {
        return Some(ParsedTtsCommand {
            action: "status".to_string(),
            args: String::new(),
        });
    }
    let mut parts = rest.split_whitespace();
    let action = parts.next().unwrap_or("").to_lowercase();
    let args = parts.collect::<Vec<_>>().join(" ");
    Some(ParsedTtsCommand { action, args })
}

/// `/tts latest` and `/tts read latest` both request a read-aloud of the
/// latest assistant reply.
pub fn is_tts_latest_request(cmd: &ParsedTtsCommand) -> bool {
    cmd.action == "latest" || (cmd.action == "read" && cmd.args.trim().to_lowercase() == "latest")
}

/// Token-only silent replies must never be read aloud.
pub fn is_silent_reply_text(text: &str) -> bool {
    static SILENT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)^\s*NO_REPLY(?:\s+NO_REPLY)*\s*$").unwrap());
    SILENT_RE.is_match(text)
}

/// Select the latest readable assistant reply from a `(role, text)`
/// transcript, skipping empty and silent-token entries.
pub fn select_latest_assistant_reply(entries: &[(String, String)]) -> Option<&str> {
    entries.iter().rev().find_map(|(role, text)| {
        let trimmed = text.trim();
        (role == "assistant" && !trimmed.is_empty() && !is_silent_reply_text(trimmed))
            .then_some(trimmed)
    })
}

pub fn hash_tts_latest_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsLatestOutcome {
    /// Hand the text to the TTS synthesis pipeline.
    Read { text: String, hash: String },
    /// The same reply was already read aloud — dedupe.
    AlreadyRead,
    NoReadableReply,
}

/// Session-scoped dedupe for `/tts latest` (upstream
/// `lastTtsReadLatestHash`).
#[derive(Debug, Default)]
pub struct TtsLatestTracker {
    last_hash: Option<String>,
}

impl TtsLatestTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a `/tts latest` request against a transcript.
    pub fn resolve(&mut self, entries: &[(String, String)]) -> TtsLatestOutcome {
        let Some(text) = select_latest_assistant_reply(entries) else {
            return TtsLatestOutcome::NoReadableReply;
        };
        let hash = hash_tts_latest_text(text);
        if self.last_hash.as_deref() == Some(hash.as_str()) {
            return TtsLatestOutcome::AlreadyRead;
        }
        self.last_hash = Some(hash.clone());
        TtsLatestOutcome::Read {
            text: text.to_string(),
            hash,
        }
    }
}

// ============================================================================
// ChannelPlugin wiring
// ============================================================================

/// WhatsApp channel implementation.
///
/// mylobster carries no live WhatsApp Web socket; this plugin validates and
/// classifies outbound targets, enforces the allowFrom policy and reachout
/// timelock, serializes sends per account, and parks payloads on the
/// per-account [`OutboundDrainQueue`] for a live socket layer to flush.
pub struct WhatsAppChannel {
    enabled: bool,
    account_id: String,
    allow_from: Option<Vec<String>>,
}

impl WhatsAppChannel {
    pub fn new(config: &Config) -> Self {
        let wa: &WhatsAppAccountConfig = &config.channels.whatsapp.default_account;
        Self {
            enabled: wa.enabled.unwrap_or(false),
            account_id: DEFAULT_ACCOUNT_ID.to_string(),
            allow_from: wa.allow_from.clone(),
        }
    }
}

#[async_trait]
impl ChannelPlugin for WhatsAppChannel {
    fn id(&self) -> &str {
        "whatsapp"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "WhatsApp".to_string(),
            description: "WhatsApp Web channel (Baileys-compatible lifecycle)".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Voice,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        info!("WhatsApp channel starting");
        // A live socket layer boots here:
        // 1. `read_web_creds_with_backup_restore(auth_dir)` — restore/verify creds.
        // 2. Create the Baileys-compatible socket with
        //    `resolve_whatsapp_socket_timing(..)`.
        // 3. Drive `LoginWaitMachine` from connection.update events; only report
        //    ready on `Succeed` (durable creds persisted).
        // 4. Attach inbound listeners; on reconnect run `filter_reconnect_catchup`.
        // 5. Start the periodic `outbound_drain_queue(account)` flush tick.
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("WhatsApp channel stopping");
            // A live socket layer must: flush the outbound drain queue (bounded
            // by DRAIN_FLUSH_TIMEOUT_MS), wait for the creds save queue, then
            // tear down via `close_wa_socket` (end(error) before raw close).
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let resolution =
            resolve_whatsapp_outbound_target(Some(to), self.allow_from.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        check_reachout_timelock(&self.account_id, &resolution.to, now_ms)
            .map_err(|e| anyhow::anyhow!(e))?;

        let account_id = self.account_id.clone();
        let target = resolution.to.clone();
        let chat_type = session_chat_type_label(resolution.chat_type);
        let text = message.to_string();
        with_serialized_account_send(&self.account_id, async move {
            let queue = outbound_drain_queue(&account_id);
            let dropped = queue.enqueue(QueuedOutbound {
                to: target.clone(),
                text,
                enqueued_at_ms: now_ms,
            });
            if dropped.is_some() {
                info!(to = %target, "WhatsApp drain queue full; dropped oldest payload");
            }
            info!(
                to = %target,
                chat_type = chat_type,
                queued = queue.len(),
                "WhatsApp: message queued for socket drain"
            );
        })
        .await;
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = WhatsAppChannel::new(config);
    ChannelPlugin::send_message(&channel, to, message).await
}

// v2026.4.1: Support HTML/XML/CSS MIME types + fallback for unknown types
pub fn resolve_mime_type(filename: &str, content_type: Option<&str>) -> String {
    if let Some(ct) = content_type {
        return ct.to_string();
    }
    let ext = filename.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html".to_string(),
        "xml" => "application/xml".to_string(),
        "css" => "text/css".to_string(),
        "js" => "application/javascript".to_string(),
        "json" => "application/json".to_string(),
        "pdf" => "application/pdf".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "mp4" => "video/mp4".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "webp" => "image/webp".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Target normalization ────────────────────────────────────────────

    #[test]
    fn normalizes_user_targets_to_e164() {
        assert_eq!(
            normalize_whatsapp_target("+1 (555) 123-4567").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_whatsapp_target("whatsapp:15551234567").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_whatsapp_target("15551234567@s.whatsapp.net").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_whatsapp_target("15551234567:22@s.whatsapp.net").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_whatsapp_target("15551234567@c.us").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_whatsapp_target("98765@lid").as_deref(),
            Some("+98765")
        );
    }

    #[test]
    fn rejects_non_whatsapp_targets() {
        assert_eq!(normalize_whatsapp_target(""), None);
        assert_eq!(normalize_whatsapp_target("telegram:12345"), None);
        assert_eq!(normalize_whatsapp_target("user@example.com"), None);
        assert_eq!(normalize_whatsapp_target("abc"), None);
    }

    #[test]
    fn normalizes_group_jids_with_prefixes() {
        assert_eq!(
            normalize_whatsapp_group_jid("group:123-456@g.us").as_deref(),
            Some("123-456@g.us")
        );
        assert_eq!(
            normalize_whatsapp_group_jid("whatsapp:group:123@g.us").as_deref(),
            Some("123@g.us")
        );
        assert_eq!(normalize_whatsapp_group_jid("abc@g.us"), None);
        assert_eq!(normalize_whatsapp_group_jid("123@x.us"), None);
        assert!(is_whatsapp_group_jid("120363000000000001@g.us"));
    }

    #[test]
    fn newsletter_targets_classified_as_channel_not_dm() {
        assert!(is_whatsapp_newsletter_jid("120363@newsletter"));
        assert!(is_whatsapp_newsletter_jid("whatsapp:120363@NEWSLETTER"));
        assert!(!is_whatsapp_newsletter_jid("abc@newsletter"));
        let normalized = normalize_whatsapp_target("whatsapp:120363@newsletter").unwrap();
        assert_eq!(normalized, "120363@newsletter");
        let chat_type = classify_whatsapp_chat_type(&normalized);
        assert_eq!(chat_type, WhatsAppChatType::Newsletter);
        assert_eq!(session_chat_type_label(chat_type), "channel");
        assert_ne!(session_chat_type_label(chat_type), "direct");
    }

    #[test]
    fn allow_from_entries_normalized_and_deduped() {
        let entries = vec![
            "+1 555 000 1111".to_string(),
            "15550001111".to_string(),
            "*".to_string(),
            "".to_string(),
            "bogus@example.com".to_string(),
        ];
        assert_eq!(
            normalize_whatsapp_allow_from_entries(&entries),
            vec!["15550001111".to_string(), "*".to_string()]
        );
    }

    // ── Outbound resolution + allowFrom policy ──────────────────────────

    #[test]
    fn outbound_resolution_groups_and_newsletters_bypass_allowlist() {
        let allow = vec!["19990000000".to_string()];
        let group = resolve_whatsapp_outbound_target(Some("123-9@g.us"), Some(&allow)).unwrap();
        assert_eq!(group.chat_type, WhatsAppChatType::Group);
        let nl =
            resolve_whatsapp_outbound_target(Some("555@newsletter"), Some(&allow)).unwrap();
        assert_eq!(nl.chat_type, WhatsAppChatType::Newsletter);
        assert_eq!(nl.to, "555@newsletter");
    }

    #[test]
    fn outbound_resolution_enforces_allowlist_for_users() {
        let allow = vec!["19990000000".to_string()];
        let ok = resolve_whatsapp_outbound_target(Some("+19990000000"), Some(&allow)).unwrap();
        assert_eq!(ok.to, "+19990000000");
        assert_eq!(ok.chat_type, WhatsAppChatType::Direct);
        assert!(resolve_whatsapp_outbound_target(Some("+15551234567"), Some(&allow)).is_err());
        // wildcard admits everyone
        let wild = vec!["*".to_string()];
        assert!(resolve_whatsapp_outbound_target(Some("+15551234567"), Some(&wild)).is_ok());
        // empty allowlist admits everyone
        assert!(resolve_whatsapp_outbound_target(Some("+15551234567"), None).is_ok());
        // missing target
        assert!(resolve_whatsapp_outbound_target(None, None).is_err());
        assert!(resolve_whatsapp_outbound_target(Some("   "), None).is_err());
    }

    // ── LID mappings ────────────────────────────────────────────────────

    #[test]
    fn lid_forward_mapping_prefers_lid_jid() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            to_whatsapp_jid_with_lid("+15551234567", Some(dir.path())),
            "15551234567@s.whatsapp.net"
        );
        store_lid_mapping(dir.path(), "15551234567", "987654321").unwrap();
        assert_eq!(
            to_whatsapp_jid_with_lid("+15551234567", Some(dir.path())),
            "987654321@lid"
        );
        // Reverse mapping resolves the LID JID back to E.164, keyed by authDir.
        assert_eq!(
            jid_to_e164("987654321@lid", Some(dir.path())).as_deref(),
            Some("+15551234567")
        );
        assert_eq!(jid_to_e164("987654321@lid", None), None);
        assert_eq!(
            jid_to_e164("15551234567@s.whatsapp.net", None).as_deref(),
            Some("+15551234567")
        );
    }

    #[test]
    fn at_digit_mentions_extracted_with_jids() {
        let mentions =
            extract_at_digit_mentions("hey @15551234567 and @15551234567 also @999 short");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].digits, "15551234567");
        assert_eq!(mentions[0].jid, "15551234567@s.whatsapp.net");
        assert!(extract_at_digit_mentions("no mentions here").is_empty());
    }

    // ── Quoted-message metadata cache ───────────────────────────────────

    #[test]
    fn quoted_meta_cache_round_trip_and_bot_authored() {
        let account = "test-quote-rt";
        cache_inbound_message_meta(
            account,
            "123@g.us",
            "MSG1",
            QuotedMeta {
                participant: Some("15550001111@s.whatsapp.net".to_string()),
                participant_e164: Some("+15550001111".to_string()),
                body: Some("hello".to_string()),
                from_me: false,
            },
        );
        let meta = lookup_inbound_message_meta(account, "123@g.us", "MSG1").unwrap();
        assert_eq!(meta.body.as_deref(), Some("hello"));
        assert!(!meta.from_me);

        cache_bot_authored_message_meta(account, "123@g.us", "MSG2", Some("bot said".into()));
        let bot = lookup_inbound_message_meta(account, "123@g.us", "MSG2").unwrap();
        assert!(bot.from_me);
        assert_eq!(bot.body.as_deref(), Some("bot said"));

        // Unknown ids and empty keys miss.
        assert!(lookup_inbound_message_meta(account, "123@g.us", "NOPE").is_none());
        cache_inbound_message_meta("", "x", "y", QuotedMeta::default());
        assert!(lookup_inbound_message_meta("", "x", "y").is_none());
    }

    #[test]
    fn quoted_meta_lookup_for_target_matches_equivalent_direct_chats() {
        let account = "test-quote-target";
        cache_inbound_message_meta(
            account,
            "15550002222@s.whatsapp.net",
            "MSGX",
            QuotedMeta {
                participant: Some("15550002222@s.whatsapp.net".to_string()),
                participant_e164: Some("+15550002222".to_string()),
                body: Some("quoted".to_string()),
                from_me: false,
            },
        );
        // Device-scoped variant of the same conversation matches.
        let hit = lookup_inbound_message_meta_for_target(
            account,
            "15550002222:9@s.whatsapp.net",
            "MSGX",
            None,
        )
        .unwrap();
        assert_eq!(hit.remote_jid, "15550002222@s.whatsapp.net");
        assert_eq!(hit.meta.body.as_deref(), Some("quoted"));
        // Group targets never cross-match direct conversations.
        assert!(lookup_inbound_message_meta_for_target(account, "999@g.us", "MSGX", None)
            .is_none());
    }

    #[test]
    fn build_quoted_options_requires_id_and_jid() {
        assert!(build_quoted_message_options(None, Some("1@g.us"), false, None, None).is_none());
        assert!(build_quoted_message_options(Some("  "), Some("1@g.us"), false, None, None)
            .is_none());
        let opts = build_quoted_message_options(
            Some("MID"),
            Some("1@g.us"),
            true,
            Some("2@s.whatsapp.net"),
            Some("preview"),
        )
        .unwrap();
        assert_eq!(opts.id, "MID");
        assert!(opts.from_me);
        assert_eq!(opts.message_text, "preview");
    }

    // ── Quoted-image media plan ─────────────────────────────────────────

    #[test]
    fn quoted_image_media_saved_as_inbound_media() {
        let max = media_max_bytes(Some(50));
        assert_eq!(
            plan_quoted_media_capture(Some("image/jpeg"), true, Some(1024), max),
            QuotedMediaPlan::SaveAsInboundMedia {
                mimetype: "image/jpeg".to_string()
            }
        );
        assert_eq!(
            plan_quoted_media_capture(Some("image/png"), false, None, max),
            QuotedMediaPlan::SkipNotDownloadable
        );
        assert_eq!(
            plan_quoted_media_capture(Some("video/mp4"), true, None, max),
            QuotedMediaPlan::SkipUnsupportedType
        );
        assert_eq!(
            plan_quoted_media_capture(None, true, None, max),
            QuotedMediaPlan::SkipUnsupportedType
        );
        assert_eq!(
            plan_quoted_media_capture(Some("image/jpeg"), true, Some(max + 1), max),
            QuotedMediaPlan::SkipTooLarge { max_bytes: max }
        );
    }

    // ── Suffix-only streaming ───────────────────────────────────────────

    #[test]
    fn streaming_emits_only_new_suffix_never_cumulative() {
        assert_eq!(
            compute_streaming_suffix("", "Hello").as_deref(),
            Some("Hello")
        );
        // Cumulative payload → only the new tail is emitted.
        assert_eq!(
            compute_streaming_suffix("Hello", "Hello world").as_deref(),
            Some(" world")
        );
        // Identical payload → nothing new.
        assert_eq!(compute_streaming_suffix("Hello", "Hello"), None);
        assert_eq!(compute_streaming_suffix("Hello", ""), None);
        // Distinct block passes through whole.
        assert_eq!(
            compute_streaming_suffix("Hello", "Fresh block").as_deref(),
            Some("Fresh block")
        );
    }

    // ── Status reactions ────────────────────────────────────────────────

    #[test]
    fn status_reaction_lifecycle_queued_thinking_tool_done() {
        let mut ctl = StatusReactionController::new(StatusReactionEmojis::default(), 0);
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Queued, 0),
            Some(ReactionUpdate::Set("👀".to_string()))
        );
        // Duplicate stage is a no-op.
        assert_eq!(ctl.advance_at(StatusReactionStage::Queued, 1), None);
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Thinking, 2),
            Some(ReactionUpdate::Set("🤔".to_string()))
        );
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Tool, 3),
            Some(ReactionUpdate::Set("🛠️".to_string()))
        );
        // Tool → Thinking cycling is allowed mid-run.
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Thinking, 4),
            Some(ReactionUpdate::Set("🤔".to_string()))
        );
        // Done with no done-emoji clears the reaction and is terminal.
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Done, 5),
            Some(ReactionUpdate::Clear)
        );
        assert!(ctl.is_terminal());
        assert_eq!(ctl.advance_at(StatusReactionStage::Error, 6), None);
    }

    #[test]
    fn status_reaction_error_and_throttle() {
        let mut ctl = StatusReactionController::new(StatusReactionEmojis::default(), 1000);
        assert!(ctl.advance_at(StatusReactionStage::Queued, 0).is_some());
        // Throttled: within min interval, stage recorded but no send.
        assert_eq!(ctl.advance_at(StatusReactionStage::Thinking, 100), None);
        assert_eq!(ctl.stage(), Some(StatusReactionStage::Thinking));
        // Terminal error bypasses throttle.
        assert_eq!(
            ctl.advance_at(StatusReactionStage::Error, 150),
            Some(ReactionUpdate::Set("⚠️".to_string()))
        );
        assert!(ctl.is_terminal());
    }

    #[test]
    fn ack_reaction_gating() {
        assert!(should_ack_reaction("👀", true, false, true, "mentions", false, false));
        assert!(!should_ack_reaction("👀", true, false, false, "mentions", false, false));
        assert!(!should_ack_reaction("", true, false, true, "mentions", false, false));
        assert!(should_ack_reaction("👀", false, true, true, "always", false, false));
        assert!(!should_ack_reaction("👀", false, true, true, "off", true, true));
        assert!(should_ack_reaction("👀", false, true, true, "mentions", true, false));
        assert!(should_ack_reaction("👀", false, true, true, "mentions", false, true));
        assert!(!should_ack_reaction("👀", false, true, true, "mentions", false, false));
    }

    // ── Socket timing + serialized sends ────────────────────────────────

    #[test]
    fn socket_timing_resolution_ignores_non_positive() {
        let cfg = WhatsAppSocketTimingConfig {
            keep_alive_interval_ms: Some(10_000),
            connect_timeout_ms: Some(0),
            default_query_timeout_ms: None,
        };
        let t = resolve_whatsapp_socket_timing(Some(&cfg));
        assert_eq!(t.keep_alive_interval_ms, 10_000);
        assert_eq!(t.connect_timeout_ms, 60_000);
        assert_eq!(t.default_query_timeout_ms, 60_000);
        assert_eq!(
            resolve_whatsapp_socket_timing(None),
            DEFAULT_WHATSAPP_SOCKET_TIMING
        );
        assert_eq!(resolve_socket_operation_timeout_ms(0), 60_000);
        assert_eq!(resolve_socket_operation_timeout_ms(5), 5);
    }

    #[test]
    fn overlapping_sends_are_serialized_per_account() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let log: Arc<parking_lot::Mutex<Vec<&'static str>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        rt.block_on(async {
            let l1 = log.clone();
            let l2 = log.clone();
            let a = tokio::spawn(with_serialized_account_send("test-serial", async move {
                l1.lock().push("a-start");
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                l1.lock().push("a-end");
            }));
            let b = tokio::spawn(with_serialized_account_send("test-serial", async move {
                l2.lock().push("b-start");
                l2.lock().push("b-end");
            }));
            a.await.unwrap();
            b.await.unwrap();
        });
        let entries = log.lock().clone();
        // Whichever task ran first must fully finish before the other starts.
        let first = entries[0];
        assert_eq!(entries[1], if first == "a-start" { "a-end" } else { "b-end" });
    }

    // ── Reconnect policy + catch-up ─────────────────────────────────────

    #[test]
    fn reconnect_policy_clamped() {
        let cfg = WhatsAppReconnectConfig {
            initial_ms: Some(1),
            max_ms: Some(1),
            factor: Some(100.0),
            jitter: Some(5.0),
            max_attempts: Some(3),
        };
        let p = resolve_reconnect_policy(Some(&cfg));
        assert_eq!(p.initial_ms, 250);
        assert_eq!(p.max_ms, 250);
        assert_eq!(p.factor, 10.0);
        assert_eq!(p.jitter, 1.0);
        assert_eq!(p.max_attempts, 3);
        assert_eq!(resolve_reconnect_policy(None), DEFAULT_RECONNECT_POLICY);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let p = DEFAULT_RECONNECT_POLICY;
        let b0 = compute_backoff_ms(&p, 0, 0.5);
        let b1 = compute_backoff_ms(&p, 1, 0.5);
        let b10 = compute_backoff_ms(&p, 10, 0.5);
        assert_eq!(b0, 2_000);
        assert_eq!(b1, 3_600);
        assert_eq!(b10, 30_000); // capped at max_ms
        assert!(should_attempt_reconnect(&p, 11));
        assert!(!should_attempt_reconnect(&p, 12));
    }

    #[test]
    fn catchup_is_bounded_by_age_and_count() {
        let now: u64 = 1_000_000;
        let window = CatchupWindow {
            max_age_ms: 10_000,
            max_count: 2,
        };
        let mk = |id: &str, ts: u64| CatchupCandidate {
            id: id.to_string(),
            timestamp_ms: ts,
        };
        let candidates = vec![
            mk("old", now - 20_000),      // too old
            mk("processed", now - 9_000), // <= last processed
            mk("a", now - 8_000),
            mk("b", now - 5_000),
            mk("c", now - 1_000),
        ];
        let kept = filter_reconnect_catchup(candidates, now - 9_000, now, &window);
        // Bounded to the 2 most recent eligible messages.
        assert_eq!(
            kept.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    // ── Teardown + login machine ────────────────────────────────────────

    #[test]
    fn socket_teardown_ends_before_raw_close() {
        struct Recorder(Vec<&'static str>);
        impl WaSocketTeardown for Recorder {
            fn end_with_error(&mut self, _reason: &str) {
                self.0.push("end");
            }
            fn raw_ws_close(&mut self) {
                self.0.push("close");
            }
        }
        let mut sock = Recorder(Vec::new());
        close_wa_socket(&mut sock);
        assert_eq!(sock.0, vec!["end", "close"]);
        assert_eq!(
            SOCKET_TEARDOWN_SEQUENCE,
            [SocketTeardownStep::EndWithError, SocketTeardownStep::RawClose]
        );
    }

    #[test]
    fn login_machine_requires_durable_creds_before_success() {
        let mut m = LoginWaitMachine::new();
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionOpen {
                creds: CredsPersistState::Missing
            }),
            LoginWaitAction::Fail(LoginFailureReason::AuthNotPersisted)
        );
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionOpen {
                creds: CredsPersistState::Unstable
            }),
            LoginWaitAction::Fail(LoginFailureReason::AuthUnstable)
        );
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionOpen {
                creds: CredsPersistState::Persisted
            }),
            LoginWaitAction::Succeed { restarted: false }
        );
    }

    #[test]
    fn login_machine_restart_classes_fire_once() {
        // 515 post-pairing restart, then 408 QR/pairing timeout retry.
        let mut m = LoginWaitMachine::new();
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionError {
                status_code: Some(POST_PAIRING_RESTART_STATUS)
            }),
            LoginWaitAction::ReplaceSocket {
                close_current: true,
                clear_auth: false
            }
        );
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionError {
                status_code: Some(TIMED_OUT_STATUS)
            }),
            LoginWaitAction::ReplaceSocket {
                close_current: true,
                clear_auth: false
            }
        );
        // Second 408 is terminal.
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionError {
                status_code: Some(TIMED_OUT_STATUS)
            }),
            LoginWaitAction::Fail(LoginFailureReason::Other)
        );
        // Success after restarts reports restarted=true.
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionOpen {
                creds: CredsPersistState::Persisted
            }),
            LoginWaitAction::Succeed { restarted: true }
        );
    }

    #[test]
    fn login_machine_logged_out_clears_auth_once_then_fails() {
        let mut m = LoginWaitMachine::new();
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionError {
                status_code: Some(LOGGED_OUT_STATUS)
            }),
            LoginWaitAction::ReplaceSocket {
                close_current: true,
                clear_auth: true
            }
        );
        assert_eq!(
            m.on_event(LoginWaitEvent::ConnectionError {
                status_code: Some(LOGGED_OUT_STATUS)
            }),
            LoginWaitAction::Fail(LoginFailureReason::LoggedOut)
        );
    }

    #[test]
    fn long_lived_disconnect_classification() {
        assert_eq!(
            classify_long_lived_disconnect(Some(401)),
            DisconnectDecision::StopLoggedOut
        );
        assert_eq!(
            classify_long_lived_disconnect(Some(440)),
            DisconnectDecision::StopReplaced
        );
        assert_eq!(
            classify_long_lived_disconnect(Some(515)),
            DisconnectDecision::RestartImmediate
        );
        assert_eq!(
            classify_long_lived_disconnect(Some(500)),
            DisconnectDecision::ReconnectBackoff
        );
        assert_eq!(
            classify_long_lived_disconnect(None),
            DisconnectDecision::ReconnectBackoff
        );
        // Restart loops stop only on logout/replaced.
        assert!(is_terminal_disconnect(DisconnectDecision::StopLoggedOut));
        assert!(is_terminal_disconnect(DisconnectDecision::StopReplaced));
        assert!(!is_terminal_disconnect(DisconnectDecision::ReconnectBackoff));
        // Auth is preserved on every terminal disconnect except server logout.
        assert!(should_clear_auth_on_disconnect(DisconnectDecision::StopLoggedOut));
        assert!(!should_clear_auth_on_disconnect(DisconnectDecision::StopReplaced));
        assert!(!should_clear_auth_on_disconnect(DisconnectDecision::ReconnectBackoff));
    }

    // ── Creds durability ────────────────────────────────────────────────

    #[test]
    fn creds_atomic_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        write_web_creds_atomically(dir.path(), r#"{"me":{"id":"1@s.whatsapp.net"}}"#).unwrap();
        let outcome = read_web_creds_with_backup_restore(dir.path()).unwrap();
        assert!(matches!(outcome, CredsReadOutcome::Primary(_)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(resolve_web_creds_path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn malformed_creds_restored_from_backup() {
        let dir = tempfile::tempdir().unwrap();
        write_web_creds_atomically(dir.path(), r#"{"good":1}"#).unwrap();
        // The second atomic write snapshots the first into creds.json.bak;
        // out-of-band corruption of the primary then restores that snapshot.
        write_web_creds_atomically(dir.path(), r#"{"good":2}"#).unwrap();
        std::fs::write(resolve_web_creds_path(dir.path()), "{corrupt").unwrap();
        let outcome = read_web_creds_with_backup_restore(dir.path()).unwrap();
        assert_eq!(
            outcome,
            CredsReadOutcome::RestoredFromBackup(r#"{"good":1}"#.to_string())
        );
        // Primary was repaired in place.
        assert_eq!(
            read_web_creds_json_raw(&resolve_web_creds_path(dir.path())).as_deref(),
            Some(r#"{"good":1}"#)
        );
    }

    #[test]
    fn missing_creds_report_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_web_creds_with_backup_restore(dir.path()).unwrap(),
            CredsReadOutcome::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_creds_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        std::fs::write(&target, r#"{"a":1}"#).unwrap();
        let creds = resolve_web_creds_path(dir.path());
        std::os::unix::fs::symlink(&target, &creds).unwrap();
        assert!(assert_web_creds_regular_file_or_missing(&creds).is_err());
        assert!(read_web_creds_json_raw(&creds).is_none());
        assert!(read_web_creds_with_backup_restore(dir.path()).is_err());
        assert!(write_web_creds_atomically(dir.path(), "{}").is_err());
    }

    // ── Outbound drain queue ────────────────────────────────────────────

    #[test]
    fn drain_queue_bounded_fifo() {
        let q = OutboundDrainQueue::new(2);
        let mk = |n: u64| QueuedOutbound {
            to: format!("+{n}"),
            text: "hi".to_string(),
            enqueued_at_ms: n,
        };
        assert!(q.enqueue(mk(1)).is_none());
        assert!(q.enqueue(mk(2)).is_none());
        // Bound hit → oldest dropped.
        let dropped = q.enqueue(mk(3)).unwrap();
        assert_eq!(dropped.to, "+1");
        assert_eq!(q.len(), 2);
        let batch = q.drain_batch(10);
        assert_eq!(
            batch.iter().map(|p| p.to.as_str()).collect::<Vec<_>>(),
            vec!["+2", "+3"]
        );
        assert!(q.is_empty());
    }

    // ── Group policy ────────────────────────────────────────────────────

    #[test]
    fn group_visible_reply_defaults_to_message_tool_only() {
        assert_eq!(
            resolve_group_visible_reply_mode(None, true),
            WhatsAppGroupVisibleReplyMode::MessageToolOnly
        );
        assert_eq!(
            resolve_group_visible_reply_mode(None, false),
            WhatsAppGroupVisibleReplyMode::Always
        );
        assert_eq!(
            resolve_group_visible_reply_mode(Some(WhatsAppGroupVisibleReplyMode::Always), true),
            WhatsAppGroupVisibleReplyMode::Always
        );
        assert!(!implicit_reply_visible(WhatsAppGroupVisibleReplyMode::MessageToolOnly));
        assert!(implicit_reply_visible(WhatsAppGroupVisibleReplyMode::Always));
    }

    #[test]
    fn group_session_key_scoped_per_account() {
        assert_eq!(
            resolve_whatsapp_group_session_key("wa:group:1@g.us", None),
            "wa:group:1@g.us"
        );
        assert_eq!(
            resolve_whatsapp_group_session_key("wa:group:1@g.us", Some("default")),
            "wa:group:1@g.us"
        );
        assert_eq!(
            resolve_whatsapp_group_session_key("wa:group:1@g.us", Some("Work")),
            "wa:group:1@g.us:thread:whatsapp-account-work"
        );
        assert_eq!(
            resolve_whatsapp_group_session_key("wa:dm:+1555", Some("work")),
            "wa:dm:+1555"
        );
    }

    #[test]
    fn group_history_retained_and_bounded() {
        let account = "test-history";
        let group = "42@g.us";
        for i in 0..5u64 {
            record_group_history(
                account,
                group,
                GroupHistoryEntry {
                    sender: "alice".to_string(),
                    body: format!("m{i}"),
                    timestamp_ms: i,
                    message_id: None,
                },
                Some(3),
            );
        }
        let snapshot = group_history_snapshot(account, group);
        assert_eq!(
            snapshot.iter().map(|e| e.body.as_str()).collect::<Vec<_>>(),
            vec!["m2", "m3", "m4"]
        );
        // Retention is keyed by account+group, so a "reconnect" (new socket)
        // sees the same history; only an explicit clear drops it.
        clear_group_history_for_account(account);
        assert!(group_history_snapshot(account, group).is_empty());
    }

    // ── Media ───────────────────────────────────────────────────────────

    #[test]
    fn media_admission_caps() {
        let max = media_max_bytes(Some(1));
        assert_eq!(max, 1024 * 1024);
        assert!(admit_inbound_media(max, max).is_ok());
        let err = admit_inbound_media(max + 1, max).unwrap_err();
        assert_eq!(err.to_string(), "Media exceeds 1MB limit");
        assert_eq!(media_max_bytes(None), 50 * 1024 * 1024);
    }

    #[test]
    fn document_filename_mime_derived_and_sanitized() {
        assert_eq!(
            resolve_whatsapp_document_file_name(None, Some("application/pdf")),
            "file.pdf"
        );
        assert_eq!(
            resolve_whatsapp_document_file_name(Some(""), Some("image/png")),
            "file.png"
        );
        assert_eq!(resolve_whatsapp_document_file_name(None, None), "file");
        assert_eq!(
            resolve_whatsapp_document_file_name(Some("re\u{7}port\u{1}.pdf  "), None),
            "report.pdf"
        );
        assert_eq!(
            resolve_whatsapp_document_file_name(Some("\u{1}\u{2}"), Some("audio/mpeg")),
            "file.mp3"
        );
    }

    #[test]
    fn outbound_media_mode_force_document_preserves_bytes() {
        assert_eq!(
            resolve_outbound_media_mode(true, false, Some("image/jpeg")),
            OutboundMediaMode::Document {
                preserve_original_bytes: true
            }
        );
        assert_eq!(
            resolve_outbound_media_mode(false, true, Some("image/jpeg")),
            OutboundMediaMode::Document {
                preserve_original_bytes: true
            }
        );
        assert_eq!(
            resolve_outbound_media_mode(false, false, Some("image/jpeg")),
            OutboundMediaMode::Image
        );
        assert_eq!(
            resolve_outbound_media_mode(false, false, Some("video/mp4")),
            OutboundMediaMode::Video
        );
        assert_eq!(
            resolve_outbound_media_mode(false, false, Some("application/pdf")),
            OutboundMediaMode::Document {
                preserve_original_bytes: true
            }
        );
    }

    // ── Reachout timelock ───────────────────────────────────────────────

    #[test]
    fn reachout_timelock_blocks_only_active_direct() {
        let account = "test-timelock";
        let state = ReachoutTimelockState {
            is_active: true,
            enforcement_type: Some("reachout".to_string()),
            ends_at_ms: Some(2_000),
        };
        set_reachout_timelock(account, state.clone());
        // Direct target blocked while enforcing.
        let err = check_reachout_timelock(account, "+15551234567", 1_000).unwrap_err();
        assert!(err.contains("timelock is active"));
        assert!(err.contains("type=reachout"));
        // Groups pass.
        assert!(check_reachout_timelock(account, "1-2@g.us", 1_000).is_ok());
        // Expired window passes.
        assert!(check_reachout_timelock(account, "+15551234567", 3_000).is_ok());
        // Inactive state passes.
        assert!(active_reachout_timelock(
            Some(&ReachoutTimelockState::default()),
            0
        )
        .is_none());
        clear_reachout_timelock(account);
        assert!(check_reachout_timelock(account, "+15551234567", 1_000).is_ok());
    }

    // ── /tts latest ─────────────────────────────────────────────────────

    #[test]
    fn tts_command_parsing() {
        assert_eq!(
            parse_tts_command("/tts"),
            Some(ParsedTtsCommand {
                action: "status".to_string(),
                args: String::new()
            })
        );
        assert_eq!(
            parse_tts_command("/tts   "),
            Some(ParsedTtsCommand {
                action: "status".to_string(),
                args: String::new()
            })
        );
        let latest = parse_tts_command("/tts latest").unwrap();
        assert!(is_tts_latest_request(&latest));
        let read_latest = parse_tts_command("/tts read LATEST").unwrap();
        assert!(is_tts_latest_request(&read_latest));
        let audio = parse_tts_command("/tts audio Hello world").unwrap();
        assert_eq!(audio.action, "audio");
        assert_eq!(audio.args, "Hello world");
        assert!(!is_tts_latest_request(&audio));
        assert_eq!(parse_tts_command("/ttsx"), None);
        assert_eq!(parse_tts_command("hello"), None);
    }

    #[test]
    fn tts_latest_selects_latest_assistant_reply_with_dedupe() {
        let mk = |role: &str, text: &str| (role.to_string(), text.to_string());
        let transcript = vec![
            mk("assistant", "first answer"),
            mk("user", "another question"),
            mk("assistant", "NO_REPLY"),
            mk("assistant", "  "),
            mk("assistant", "final answer"),
            mk("user", "thanks"),
        ];
        assert_eq!(
            select_latest_assistant_reply(&transcript),
            Some("final answer")
        );
        let mut tracker = TtsLatestTracker::new();
        match tracker.resolve(&transcript) {
            TtsLatestOutcome::Read { text, hash } => {
                assert_eq!(text, "final answer");
                assert_eq!(hash, hash_tts_latest_text("final answer"));
            }
            other => panic!("expected Read, got {other:?}"),
        }
        // Same reply again → deduped.
        assert_eq!(tracker.resolve(&transcript), TtsLatestOutcome::AlreadyRead);
        // Silent-only transcript → nothing to read.
        let silent = vec![mk("assistant", "no_reply NO_REPLY")];
        assert_eq!(
            tracker.resolve(&silent),
            TtsLatestOutcome::NoReadableReply
        );
    }

    #[test]
    fn silent_reply_detection() {
        assert!(is_silent_reply_text("NO_REPLY"));
        assert!(is_silent_reply_text("  no_reply  "));
        assert!(is_silent_reply_text("NO_REPLY NO_REPLY"));
        assert!(!is_silent_reply_text("NO_REPLY but more"));
        assert!(!is_silent_reply_text("hello"));
    }

    // ── Misc ────────────────────────────────────────────────────────────

    #[test]
    fn mime_type_round_trip() {
        assert_eq!(resolve_mime_type("a.html", None), "text/html");
        assert_eq!(resolve_mime_type("a.bin", None), "application/octet-stream");
        assert_eq!(resolve_mime_type("a.bin", Some("text/css")), "text/css");
        assert_eq!(extension_for_mime(Some("application/pdf")), Some(".pdf"));
        assert_eq!(extension_for_mime(Some("image/jpeg; q=1")), Some(".jpg"));
        assert_eq!(extension_for_mime(Some("application/unknown")), None);
        assert_eq!(extension_for_mime(None), None);
    }

    #[test]
    fn direct_user_jid_detection() {
        assert!(is_direct_user_jid("15551234567@s.whatsapp.net"));
        assert!(is_direct_user_jid("15551234567:3@s.whatsapp.net"));
        assert!(is_direct_user_jid("999@lid"));
        assert!(is_direct_user_jid("999@hosted.lid"));
        assert!(!is_direct_user_jid("1-2@g.us"));
        assert!(!is_direct_user_jid("120363@newsletter"));
    }
}
