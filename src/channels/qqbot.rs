//! QQ Bot channel (Tencent QQ open-platform bot).
//!
//! Port of OpenClaw `extensions/qqbot` @ v2026.7.1 (behavior baseline
//! v2026.4.27 for group chat / C2C streaming). Live gateway wiring (QQ
//! open-platform WebSocket + HTTP API) is not connected in this port; the
//! load-bearing behavior — group history tracking, @-mention gating, FIFO
//! per-chat queueing, C2C streaming suffix segmentation, the
//! `/bot-group-allways` toggle, markdown-table-safe chunking, reasoning-tag
//! sanitization, SQLite KV state, sandbox media scoping, typing windows,
//! response watchdog and failed-media surfacing — is implemented as testable
//! logic below, with integration points documented on each type.

use crate::config::Config;
use crate::gateway::GatewayState;

use super::feishu::{deep_merge_defined, resolve_effective_tts_config};
use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tracing::info;

// ============================================================================
// Extension config (`channels.qqbot`, read from
// `config.channels.extensions["qqbot"]`; upstream `config-schema.ts`)
// ============================================================================

/// Per-group config (upstream `QQBotGroupSchema`, strict).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QqBotGroupConfig {
    pub require_mention: Option<bool>,
    /// `all` | `safety` | `strict`.
    pub command_level: Option<String>,
    pub ignore_other_mentions: Option<bool>,
    pub history_limit: Option<f64>,
    pub name: Option<String>,
    pub prompt: Option<String>,
}

/// `channels.qqbot` account/channel config (upstream `QQBotAccountSchema` is
/// passthrough; only ported keys are modeled, extras ignored).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QqBotExtensionConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub app_id: Option<String>,
    pub client_secret: Option<String>,
    /// Account-level default for group `requireMention` (settable at runtime
    /// via `/bot-group-allways`); precedence: group > `"*"` > this > `true`.
    pub default_require_mention: Option<bool>,
    pub markdown_support: Option<bool>,
    /// `true` ≡ `{mode:"partial", c2cStreamApi:true}`; or an object with
    /// `mode` and `c2cStreamApi`.
    pub streaming: Option<Value>,
    pub allow_from: Vec<Value>,
    pub group_allow_from: Vec<Value>,
    pub dm_policy: Option<String>,
    pub group_policy: Option<String>,
    pub url_direct_upload: Option<bool>,
    pub groups: HashMap<String, QqBotGroupConfig>,
    /// Raw per-account configs (kept raw for TTS deep-merge + overrides).
    pub accounts: HashMap<String, Value>,
    pub default_account: Option<String>,
    /// Channel-level TTS override (passthrough key; deep-merged by the
    /// framework TTS resolution — see [`resolve_qqbot_account_tts`]).
    pub tts: Option<Value>,
}

impl QqBotExtensionConfig {
    pub fn from_extensions_value(value: Option<&Value>) -> Self {
        value
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

/// Deep-merge account TTS overrides over channel TTS over `messages.tts`
/// (shared helper with Feishu — upstream `resolveEffectiveTtsConfig` +
/// `deepMergeDefined` in `src/tts/tts-config.ts`).
pub fn resolve_qqbot_account_tts(
    base_messages_tts: Option<&Value>,
    config: &QqBotExtensionConfig,
    account_id: &str,
) -> Value {
    let account_tts = config
        .accounts
        .get(account_id)
        .and_then(|a| a.get("tts"))
        .cloned();
    resolve_effective_tts_config(
        base_messages_tts,
        None,
        config.tts.as_ref(),
        account_tts.as_ref(),
    )
}

/// Generic account-over-channel config resolution (upstream
/// `resolveAccountBase`): a **shallow** spread where account keys override
/// top-level channel keys — nested objects are replaced wholesale, except
/// this deep-merge is intentionally reserved for TTS (see above).
pub fn resolve_account_config(channel: &Value, account: &Value) -> Value {
    match (channel, account) {
        (Value::Object(c), Value::Object(a)) => {
            let mut out = c.clone();
            for (k, v) in a {
                out.insert(k.clone(), v.clone());
            }
            Value::Object(out)
        }
        (_, a) if !a.is_null() => a.clone(),
        (c, _) => c.clone(),
    }
}

// ============================================================================
// Group config resolution (upstream `engine/config/group.ts`)
// ============================================================================

pub const DEFAULT_GROUP_HISTORY_LIMIT: usize = 50;
pub const DEFAULT_GROUP_COMMAND_LEVEL: &str = "all";

/// Effective per-group settings after precedence resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedGroupConfig {
    pub require_mention: bool,
    pub ignore_other_mentions: bool,
    pub history_limit: usize,
    pub command_level: String,
    pub display_name: String,
}

/// Precedence per field: specific group > wildcard `"*"` > (requireMention
/// only) account `defaultRequireMention` > hardcoded default.
pub fn resolve_group_config(config: &QqBotExtensionConfig, group_openid: &str) -> ResolvedGroupConfig {
    let specific = config.groups.get(group_openid);
    let wildcard = config.groups.get("*");
    let pick_bool = |f: fn(&QqBotGroupConfig) -> Option<bool>| {
        specific.and_then(f).or_else(|| wildcard.and_then(f))
    };
    let require_mention = pick_bool(|g| g.require_mention)
        .or(config.default_require_mention)
        .unwrap_or(true);
    let ignore_other_mentions = pick_bool(|g| g.ignore_other_mentions).unwrap_or(false);
    let history_limit = specific
        .and_then(|g| g.history_limit)
        .or_else(|| wildcard.and_then(|g| g.history_limit))
        .map(|v| v.max(0.0).floor() as usize)
        .unwrap_or(DEFAULT_GROUP_HISTORY_LIMIT);
    let command_level = specific
        .and_then(|g| g.command_level.clone())
        .or_else(|| wildcard.and_then(|g| g.command_level.clone()))
        .unwrap_or_else(|| DEFAULT_GROUP_COMMAND_LEVEL.to_string());
    let display_name = specific
        .and_then(|g| g.name.clone())
        .unwrap_or_else(|| group_openid.chars().take(8).collect());
    ResolvedGroupConfig {
        require_mention,
        ignore_other_mentions,
        history_limit,
        command_level,
        display_name,
    }
}

// ============================================================================
// Mention detection (upstream `engine/group/mention.ts`)
// ============================================================================

static MENTION_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@!?\w+>").unwrap());

/// A mention entry on an inbound QQ message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QqMention {
    pub member_openid: Option<String>,
    pub id: Option<String>,
    pub user_openid: Option<String>,
    pub nickname: Option<String>,
    pub username: Option<String>,
    pub is_you: bool,
}

impl QqMention {
    /// `member_openid ?? id ?? user_openid` (upstream openid precedence).
    pub fn openid(&self) -> Option<&str> {
        self.member_openid
            .as_deref()
            .or(self.id.as_deref())
            .or(self.user_openid.as_deref())
    }
}

/// Whether the bot itself was mentioned (upstream `detectWasMentioned`):
/// any `is_you` mention, an `GROUP_AT_MESSAGE_CREATE` event, or a custom
/// mention pattern match (invalid patterns silently skipped).
pub fn detect_was_mentioned(
    mentions: &[QqMention],
    event_type: &str,
    mention_patterns: &[String],
    content: &str,
) -> bool {
    if mentions.iter().any(|m| m.is_you) {
        return true;
    }
    if event_type == "GROUP_AT_MESSAGE_CREATE" {
        return true;
    }
    mention_patterns.iter().any(|p| {
        Regex::new(&format!("(?i){p}"))
            .map(|re| re.is_match(content))
            .unwrap_or(false)
    })
}

/// Any mention present (entries or literal `<@openid>` tags).
pub fn has_any_mention(mentions: &[QqMention], content: &str) -> bool {
    !mentions.is_empty() || MENTION_TAG_RE.is_match(content)
}

/// Strip mention tags from text (upstream `stripMentionText`): the bot's own
/// mention is removed (and the result trimmed); other members' mentions are
/// replaced with `@nickname` when a display name is known.
pub fn strip_mention_text(text: &str, mentions: &[QqMention]) -> String {
    let mut out = text.to_string();
    for mention in mentions {
        let Some(openid) = mention.openid() else { continue };
        let pattern = format!("<@!?{}>", regex::escape(openid));
        let Ok(re) = Regex::new(&pattern) else { continue };
        if mention.is_you {
            out = re.replace_all(&out, "").trim().to_string();
        } else if let Some(name) = mention.nickname.as_deref().or(mention.username.as_deref()) {
            out = re.replace_all(&out, format!("@{name}").as_str()).to_string();
        }
    }
    out
}

/// Quoting a bot message counts as an implicit mention (upstream
/// `resolveImplicitMention`).
pub fn resolve_implicit_mention(quoted_ref_is_bot: Option<bool>) -> bool {
    quoted_ref_is_bot == Some(true)
}

// ============================================================================
// Group message gating (upstream `engine/group/message-gating.ts`)
// ============================================================================

/// Gate decision for an inbound group message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupGateAction {
    /// Message @-mentions someone else while `ignoreOtherMentions` is on.
    DropOtherMention,
    /// A control command from an unauthorized sender.
    BlockUnauthorizedCommand,
    /// Mention required but absent.
    SkipNoMention,
    /// Process the message.
    Pass,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GroupGateInput {
    pub require_mention: bool,
    pub ignore_other_mentions: bool,
    pub was_mentioned: bool,
    pub has_any_mention: bool,
    pub implicit_mention: bool,
    pub can_detect_mention: bool,
    pub allow_text_commands: bool,
    pub is_control_command: bool,
    pub command_authorized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupGateDecision {
    pub action: GroupGateAction,
    pub effective_was_mentioned: bool,
    pub should_bypass_mention: bool,
}

/// Exact upstream ordering: other-mention drop → unauthorized-command block →
/// authorized-command mention bypass → mention requirement skip → pass.
pub fn resolve_group_message_gate(input: GroupGateInput) -> GroupGateDecision {
    if input.ignore_other_mentions
        && input.has_any_mention
        && !input.was_mentioned
        && !input.implicit_mention
    {
        return GroupGateDecision {
            action: GroupGateAction::DropOtherMention,
            effective_was_mentioned: false,
            should_bypass_mention: false,
        };
    }
    if input.allow_text_commands && input.is_control_command && !input.command_authorized {
        return GroupGateDecision {
            action: GroupGateAction::BlockUnauthorizedCommand,
            effective_was_mentioned: false,
            should_bypass_mention: false,
        };
    }
    let should_bypass_mention = input.require_mention
        && !input.was_mentioned
        && !input.has_any_mention
        && input.allow_text_commands
        && input.command_authorized
        && input.is_control_command;
    let effective_was_mentioned =
        input.was_mentioned || input.implicit_mention || should_bypass_mention;
    let should_skip =
        input.require_mention && input.can_detect_mention && !effective_was_mentioned;
    GroupGateDecision {
        action: if should_skip {
            GroupGateAction::SkipNoMention
        } else {
            GroupGateAction::Pass
        },
        effective_was_mentioned,
        should_bypass_mention,
    }
}

// ============================================================================
// Group history (upstream `engine/group/history.ts`)
// ============================================================================

pub const MAX_HISTORY_KEYS: usize = 1000;
pub const HISTORY_CTX_START: &str = "[Chat messages since your last reply — CONTEXT ONLY]";
pub const HISTORY_CTX_END: &str = "[CURRENT MESSAGE — reply to this]";
pub const MERGED_CTX_START: &str = "[Merged earlier messages — CONTEXT ONLY]";
pub const MERGED_CTX_END: &str = "[CURRENT MESSAGE — reply using the context above]";

/// One remembered non-@ group message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryEntry {
    pub sender: String,
    pub body: String,
    pub message_id: Option<String>,
}

/// Per-group ring buffers of messages seen since the bot's last reply, with
/// LRU key eviction bounded at [`MAX_HISTORY_KEYS`] groups.
#[derive(Debug, Default)]
pub struct GroupHistoryStore {
    buffers: HashMap<String, Vec<HistoryEntry>>,
    /// Insertion order for LRU key eviction (front = oldest).
    order: VecDeque<String>,
}

impl GroupHistoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn touch(&mut self, group: &str) {
        self.order.retain(|k| k != group);
        self.order.push_back(group.to_string());
        while self.order.len() > MAX_HISTORY_KEYS {
            if let Some(oldest) = self.order.pop_front() {
                self.buffers.remove(&oldest);
            }
        }
    }

    /// Record a pending message (no-op when `limit == 0`); trims the buffer
    /// to `limit` and refreshes the group's LRU position.
    pub fn record(&mut self, group: &str, entry: HistoryEntry, limit: usize) {
        if limit == 0 {
            return;
        }
        let buf = self.buffers.entry(group.to_string()).or_default();
        buf.push(entry);
        while buf.len() > limit {
            buf.remove(0);
        }
        self.touch(group);
    }

    /// Wrap the current message with the buffered context (upstream
    /// `buildPendingHistoryContext`); empty buffer or `limit == 0` returns the
    /// message unchanged.
    pub fn build_context(&self, group: &str, current_message: &str, limit: usize) -> String {
        if limit == 0 {
            return current_message.to_string();
        }
        let Some(buf) = self.buffers.get(group).filter(|b| !b.is_empty()) else {
            return current_message.to_string();
        };
        let history: Vec<String> = buf
            .iter()
            .map(|e| format!("[{}]: {}", e.sender, e.body))
            .collect();
        [
            HISTORY_CTX_START.to_string(),
            history.join("\n"),
            String::new(),
            HISTORY_CTX_END.to_string(),
            current_message.to_string(),
        ]
        .join("\n")
    }

    /// Clear the buffer after every reply attempt (success/timeout/error).
    pub fn clear(&mut self, group: &str) {
        if let Some(buf) = self.buffers.get_mut(group) {
            buf.clear();
        }
    }

    pub fn tracked_groups(&self) -> usize {
        self.buffers.len()
    }
}

/// Wrap merged earlier messages ahead of the current one (upstream
/// `buildMergedMessageContext`).
pub fn build_merged_message_context(preceding: &[String], current_message: &str) -> String {
    if preceding.is_empty() {
        return current_message.to_string();
    }
    [
        MERGED_CTX_START.to_string(),
        preceding.join("\n"),
        MERGED_CTX_END.to_string(),
        current_message.to_string(),
    ]
    .join("\n")
}

// ============================================================================
// FIFO message queue (upstream `engine/gateway/message-queue.ts`)
// ============================================================================

pub const DEFAULT_GLOBAL_QUEUE_SIZE: usize = 1000;
pub const DEFAULT_PER_PEER_QUEUE_SIZE: usize = 20;
pub const DEFAULT_GROUP_QUEUE_SIZE: usize = 50;
pub const DEFAULT_MAX_CONCURRENT_USERS: usize = 10;

/// Peer id builders (`guild:` / `group:` / `dm:`).
pub fn peer_id_guild(channel_id: &str) -> String {
    format!("guild:{channel_id}")
}
pub fn peer_id_group(group_openid: &str) -> String {
    format!("group:{group_openid}")
}
pub fn peer_id_dm(sender_id: &str) -> String {
    format!("dm:{sender_id}")
}
pub fn is_group_peer(peer_id: &str) -> bool {
    peer_id.starts_with("group:") || peer_id.starts_with("guild:")
}

/// A queued inbound message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueuedMessage {
    pub message_id: String,
    pub content: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub sender_is_bot: bool,
    pub event_type: String,
    pub mentions: Vec<QqMention>,
    /// Number of source messages merged into this turn (0/1 = not merged).
    pub merge_count: usize,
}

/// What was evicted to make room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEviction {
    None,
    /// Group queues drop the first bot-authored entry first.
    BotAuthored,
    /// Otherwise the oldest entry.
    Oldest,
}

/// Per-peer FIFO queues with bounded sizes, group-aware eviction and a
/// concurrency cap. Draining is modeled synchronously: `take_batch` hands the
/// whole pending batch for a group peer (commands split from mergeable
/// messages via [`plan_group_batch`]) and one message at a time otherwise.
#[derive(Debug)]
pub struct QqBotMessageQueue {
    queues: HashMap<String, VecDeque<QueuedMessage>>,
    active: HashSet<String>,
    pub max_concurrent_users: usize,
    pub global_queue_size: usize,
    total_enqueued: u64,
}

impl Default for QqBotMessageQueue {
    fn default() -> Self {
        Self {
            queues: HashMap::new(),
            active: HashSet::new(),
            max_concurrent_users: DEFAULT_MAX_CONCURRENT_USERS,
            global_queue_size: DEFAULT_GLOBAL_QUEUE_SIZE,
            total_enqueued: 0,
        }
    }
}

impl QqBotMessageQueue {
    pub fn new() -> Self {
        Self::default()
    }

    fn max_size_for(peer_id: &str) -> usize {
        if is_group_peer(peer_id) {
            DEFAULT_GROUP_QUEUE_SIZE
        } else {
            DEFAULT_PER_PEER_QUEUE_SIZE
        }
    }

    /// Enqueue preserving FIFO order; evicts per upstream policy when full.
    pub fn enqueue(&mut self, peer_id: &str, message: QueuedMessage) -> QueueEviction {
        let max = Self::max_size_for(peer_id);
        let queue = self.queues.entry(peer_id.to_string()).or_default();
        let mut eviction = QueueEviction::None;
        if queue.len() >= max {
            if is_group_peer(peer_id) {
                if let Some(idx) = queue.iter().position(|m| m.sender_is_bot) {
                    queue.remove(idx);
                    eviction = QueueEviction::BotAuthored;
                }
            }
            if eviction == QueueEviction::None {
                queue.pop_front();
                eviction = QueueEviction::Oldest;
            }
        }
        queue.push_back(message);
        self.total_enqueued += 1;
        eviction
    }

    /// Whether a drain may start for this peer (not already active and under
    /// the concurrency cap).
    pub fn try_activate(&mut self, peer_id: &str) -> bool {
        if self.active.contains(peer_id) || self.active.len() >= self.max_concurrent_users {
            return false;
        }
        self.active.insert(peer_id.to_string());
        true
    }

    /// Finish a drain: deactivate and drop the (now empty) queue entry.
    pub fn deactivate(&mut self, peer_id: &str) {
        self.active.remove(peer_id);
        if self.queues.get(peer_id).is_some_and(VecDeque::is_empty) {
            self.queues.remove(peer_id);
        }
    }

    /// Group peers with >1 pending drain the whole batch at once; otherwise
    /// one message (upstream `drainUserQueue`).
    pub fn take_batch(&mut self, peer_id: &str) -> Vec<QueuedMessage> {
        let Some(queue) = self.queues.get_mut(peer_id) else {
            return Vec::new();
        };
        if is_group_peer(peer_id) && queue.len() > 1 {
            return queue.drain(..).collect();
        }
        queue.pop_front().into_iter().collect()
    }

    pub fn pending(&self, peer_id: &str) -> usize {
        self.queues.get(peer_id).map(VecDeque::len).unwrap_or(0)
    }

    pub fn total_enqueued(&self) -> u64 {
        self.total_enqueued
    }
}

/// A group batch drain plan: slash-commands run individually **in order
/// first**, remaining messages merge into one turn (upstream
/// `drainGroupBatch`).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupBatchPlan {
    pub command_turns: Vec<QueuedMessage>,
    pub merged_turn: Option<QueuedMessage>,
}

/// Marks a command turn (trimmed content starts with `/`).
pub fn is_command_turn(content: &str) -> bool {
    content.trim_start().starts_with('/')
}

pub fn plan_group_batch(batch: Vec<QueuedMessage>) -> GroupBatchPlan {
    let (commands, normal): (Vec<_>, Vec<_>) =
        batch.into_iter().partition(|m| is_command_turn(&m.content));
    GroupBatchPlan {
        command_turns: commands,
        merged_turn: merge_group_messages(normal),
    }
}

/// Merge a batch of normal group messages into one turn (upstream
/// `mergeGroupMessages`): `[sender]: content` lines, deduped mentions,
/// `GROUP_AT_MESSAGE_CREATE` sticky event type, identity from the **last**
/// message, `sender_is_bot` only when every source was bot-authored.
pub fn merge_group_messages(batch: Vec<QueuedMessage>) -> Option<QueuedMessage> {
    match batch.len() {
        0 => return None,
        1 => return batch.into_iter().next(),
        _ => {}
    }
    let content = batch
        .iter()
        .map(|m| {
            let sender = m.sender_name.as_deref().unwrap_or(&m.sender_id);
            format!("[{sender}]: {}", m.content)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut seen = HashSet::new();
    let mentions: Vec<QqMention> = batch
        .iter()
        .flat_map(|m| m.mentions.iter().cloned())
        .filter(|m| {
            let key = m.openid().unwrap_or("").to_string();
            seen.insert(key)
        })
        .collect();
    let any_at_you = batch.iter().any(|m| m.event_type == "GROUP_AT_MESSAGE_CREATE");
    let sender_is_bot = batch.iter().all(|m| m.sender_is_bot);
    let count = batch.len();
    let last = batch.into_iter().last()?;
    Some(QueuedMessage {
        message_id: last.message_id,
        content,
        sender_id: last.sender_id,
        sender_name: last.sender_name,
        sender_is_bot,
        event_type: if any_at_you {
            "GROUP_AT_MESSAGE_CREATE".to_string()
        } else {
            last.event_type
        },
        mentions,
        merge_count: count,
    })
}

/// `isMergedTurn` parity.
pub fn is_merged_turn(message: &QueuedMessage) -> bool {
    message.merge_count > 1
}

// ============================================================================
// C2C streaming (upstream `engine/messaging/streaming-c2c.ts`)
// ============================================================================

pub const C2C_THROTTLE_DEFAULT_MS: u64 = 500;
pub const C2C_THROTTLE_MIN_MS: u64 = 300;
pub const C2C_LONG_GAP_THRESHOLD_MS: u64 = 2000;
pub const C2C_BATCH_AFTER_GAP_MS: u64 = 300;
/// Terminal error suffix appended to the final chunk on generation failure.
pub const C2C_ERROR_SUFFIX: &str = "\n\n---\n**Error**: 生成响应时发生错误。";
/// Placeholder sent when a stream is aborted before any content.
pub const C2C_ABORT_PLACEHOLDER: &str = "（已中止）";

/// QQ stream API chunk states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamInputState {
    Generating,
    Done,
}

/// Streaming phases with an explicit transition table (`completed`/`aborted`
/// are terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C2cStreamPhase {
    Idle,
    Streaming,
    Completed,
    Aborted,
}

/// One stream API call the caller must perform (`input_mode = REPLACE`,
/// shared `msg_seq` per session, auto-incrementing `index`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2cStreamChunk {
    pub content: String,
    pub state: StreamInputState,
    pub msg_seq: u64,
    pub index: u32,
}

/// C2C streaming controller (suffix segmentation model).
///
/// Each `onPartialReply` payload is the **full cumulative** text. The
/// controller tracks the raw cumulative text, a consumed offset
/// (`sent_index`, advanced when a stream session terminates) and an optional
/// `boundary_prefix`: when a new payload no longer starts with the previous
/// raw text a new reply is assumed and the previous text plus `"\n\n"` is
/// re-prepended so the same stream session continues seamlessly.
#[derive(Debug)]
pub struct C2cStreamController {
    last_raw_full: String,
    boundary_prefix: String,
    /// Byte offset into the effective text already consumed by finished
    /// stream sessions.
    sent_index: usize,
    phase: C2cStreamPhase,
    sent_chunk_count: usize,
    msg_seq: u64,
    index: u32,
    pub throttle_ms: u64,
}

impl C2cStreamController {
    /// `throttle_ms` is clamped up to [`C2C_THROTTLE_MIN_MS`].
    pub fn new(msg_seq: u64, throttle_ms: u64) -> Self {
        Self {
            last_raw_full: String::new(),
            boundary_prefix: String::new(),
            sent_index: 0,
            phase: C2cStreamPhase::Idle,
            sent_chunk_count: 0,
            msg_seq,
            index: 0,
            throttle_ms: throttle_ms.max(C2C_THROTTLE_MIN_MS),
        }
    }

    pub fn phase(&self) -> C2cStreamPhase {
        self.phase
    }

    fn effective_text(&self) -> String {
        format!("{}{}", self.boundary_prefix, self.last_raw_full)
    }

    /// Offer a cumulative partial. Returns the chunk to send (whitespace-only
    /// first chunks defer — no stream opened).
    pub fn on_partial(&mut self, full_text: &str) -> Option<C2cStreamChunk> {
        if matches!(self.phase, C2cStreamPhase::Completed | C2cStreamPhase::Aborted) {
            return None;
        }
        // Reply-boundary detection on the *raw* text (prefix match, not
        // length: normalization is unstable on unclosed tags upstream).
        let mut boundary_prefix = self.boundary_prefix.clone();
        if !self.last_raw_full.is_empty() && !full_text.starts_with(&self.last_raw_full) {
            boundary_prefix = format!("{boundary_prefix}{}\n\n", self.last_raw_full);
        }
        let effective = format!("{boundary_prefix}{full_text}");
        let suffix = effective.get(self.sent_index..).unwrap_or("").to_string();
        if suffix.trim().is_empty() && self.phase == C2cStreamPhase::Idle {
            return None; // whitespace-only chunk defers stream start (no commit)
        }
        self.boundary_prefix = boundary_prefix;
        self.last_raw_full = full_text.to_string();
        self.phase = C2cStreamPhase::Streaming;
        self.sent_chunk_count += 1;
        self.index += 1;
        Some(C2cStreamChunk {
            content: suffix,
            state: StreamInputState::Generating,
            msg_seq: self.msg_seq,
            index: self.index,
        })
    }

    /// Terminate the current stream session (e.g. before an interleaved media
    /// send): emits the DONE chunk and advances `sent_index` so the next
    /// session streams only new text. `next_msg_seq` starts the new session.
    pub fn end_session(&mut self, next_msg_seq: u64) -> Option<C2cStreamChunk> {
        if self.phase != C2cStreamPhase::Streaming {
            return None;
        }
        let effective = self.effective_text();
        let suffix = effective.get(self.sent_index..).unwrap_or("").to_string();
        self.sent_index = effective.len();
        self.index += 1;
        let chunk = C2cStreamChunk {
            content: suffix,
            state: StreamInputState::Done,
            msg_seq: self.msg_seq,
            index: self.index,
        };
        self.msg_seq = next_msg_seq;
        self.index = 0;
        self.phase = C2cStreamPhase::Idle;
        Some(chunk)
    }

    /// Finish the whole stream. `error` appends the terminal error suffix.
    pub fn finish(&mut self, error: bool) -> Option<C2cStreamChunk> {
        if matches!(self.phase, C2cStreamPhase::Completed | C2cStreamPhase::Aborted) {
            return None;
        }
        let effective = self.effective_text();
        let mut suffix = effective.get(self.sent_index..).unwrap_or("").to_string();
        if error {
            suffix.push_str(C2C_ERROR_SUFFIX);
        }
        self.phase = C2cStreamPhase::Completed;
        if suffix.trim().is_empty() && !error && self.sent_chunk_count == 0 {
            return None; // ended while whitespace-only with no open session
        }
        self.sent_index = effective.len();
        self.index += 1;
        Some(C2cStreamChunk {
            content: suffix,
            state: StreamInputState::Done,
            msg_seq: self.msg_seq,
            index: self.index,
        })
    }

    /// Abort (e.g. a `deliver` callback arrived before any partial).
    pub fn abort(&mut self) {
        self.phase = C2cStreamPhase::Aborted;
    }

    /// `shouldFallbackToStatic`: terminal with zero stream chunks sent.
    pub fn should_fallback_to_static(&self) -> bool {
        matches!(self.phase, C2cStreamPhase::Completed | C2cStreamPhase::Aborted)
            && self.sent_chunk_count == 0
    }
}

/// Enablement gate (upstream `shouldUseOfficialC2cStream`): C2C targets only;
/// `streaming: true` or `{c2cStreamApi: true}`.
pub fn should_use_official_c2c_stream(target_type: &str, streaming: Option<&Value>) -> bool {
    if target_type != "c2c" {
        return false;
    }
    match streaming {
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(o)) => o.get("c2cStreamApi").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

// ============================================================================
// `/bot-group-allways` toggle + command auth (upstream
// `engine/commands/builtin/register-group-allways.ts` + `slash-command-auth.ts`)
// ============================================================================

pub const BOT_GROUP_ALLWAYS_COMMAND: &str = "bot-group-allways";

/// Parsed `/bot-group-allways` invocation. `on` = speak freely
/// (`requireMention=false`); `off` = reply only when @-mentioned
/// (`requireMention=true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupAllwaysAction {
    /// No argument: report current state.
    Status,
    /// Set `defaultRequireMention` to this value.
    Set { require_mention: bool },
    Invalid,
}

pub fn parse_bot_group_allways(arg: Option<&str>) -> GroupAllwaysAction {
    match arg.map(str::trim) {
        None | Some("") => GroupAllwaysAction::Status,
        Some("on") => GroupAllwaysAction::Set { require_mention: false },
        Some("off") => GroupAllwaysAction::Set { require_mention: true },
        Some(_) => GroupAllwaysAction::Invalid,
    }
}

/// Apply the toggle to a raw `channels.qqbot` config document: a named
/// account (`accountId != "default"` and present under `accounts`) gets
/// `accounts.<id>.defaultRequireMention`, otherwise the top-level key is set
/// (upstream config write in `register-group-allways.ts`).
pub fn apply_default_require_mention(config: &mut Value, account_id: &str, require_mention: bool) {
    let has_named_account = account_id != "default"
        && config
            .get("accounts")
            .and_then(|a| a.get(account_id))
            .is_some();
    if has_named_account {
        config["accounts"][account_id]["defaultRequireMention"] = json!(require_mention);
    } else {
        config["defaultRequireMention"] = json!(require_mention);
    }
}

fn normalize_allow_entries(list: &[Value]) -> Vec<String> {
    list.iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Framework slash-command authorization (upstream
/// `resolveSlashCommandAuth`): the effective list is `commands.allowFrom`
/// when set, else the group allowlist in groups (when non-empty), else the DM
/// allowlist. **Wildcard-only or empty lists deny** — an explicit non-`*`
/// sender entry is required.
pub fn resolve_slash_command_auth(
    sender_id: &str,
    is_group: bool,
    allow_from: &[Value],
    group_allow_from: &[Value],
    commands_allow_from: Option<&[Value]>,
) -> bool {
    let list: Vec<String> = match commands_allow_from {
        Some(l) => normalize_allow_entries(l),
        None => {
            let group = normalize_allow_entries(group_allow_from);
            if is_group && !group.is_empty() {
                group
            } else {
                normalize_allow_entries(allow_from)
            }
        }
    };
    let explicit: Vec<&String> = list.iter().filter(|e| e.as_str() != "*").collect();
    if explicit.is_empty() {
        return false;
    }
    explicit.iter().any(|e| e.as_str() == sender_id)
}

/// Permission-denied reply for unauthorized framework commands (Chinese,
/// upstream literal).
pub fn permission_denied_text(command_name: &str) -> String {
    format!(
        "⛔ 权限不足：请先在 channels.qqbot.{{allowFrom|groupAllowFrom}} 中配置明确的发送者列表后再使用 /{command_name}。"
    )
}

/// `c2cOnly` guard: framework commands require an explicit
/// `qqbot:c2c:<id>` sender form (upstream `isExplicitQQBotC2cFrom`).
pub fn is_explicit_qqbot_c2c_from(from: &str) -> bool {
    from.starts_with("qqbot:c2c:") && from.len() > "qqbot:c2c:".len()
}

// ============================================================================
// Markdown table chunking (upstream
// `engine/messaging/markdown-table-chunking.ts`)
// ============================================================================

/// Hard UTF-8 byte cap for markdown chunks carrying tables.
pub const QQBOT_MARKDOWN_SAFE_CHUNK_BYTE_LIMIT: usize = 3600;
/// Caller-side text chunk limit (upstream `TEXT_CHUNK_LIMIT`).
pub const QQBOT_TEXT_CHUNK_LIMIT: usize = 5000;

pub fn resolve_qqbot_markdown_chunk_limit(limit: usize) -> usize {
    limit.min(QQBOT_MARKDOWN_SAFE_CHUNK_BYTE_LIMIT)
}

static FENCE_LINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\s*)(`{3,}|~{3,})").unwrap());
static TABLE_SEPARATOR_CELL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^:?-+:?$").unwrap());

/// Split a table row into cells honoring `\|` and `\\` escapes (upstream
/// `splitTableRowCells`, GFM-correct).
pub fn split_table_row_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push('\\');
                    current.push(next);
                }
            }
            '|' => cells.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    cells.push(current);
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

/// Trimmed line starts and ends with `|` and has ≥ 2 cells.
pub fn is_table_row_line(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 2 && t.starts_with('|') && t.ends_with('|') && split_table_row_cells(t).len() >= 2
}

/// Every cell matches `:?-+:?`.
pub fn is_table_separator_line(line: &str) -> bool {
    if !is_table_row_line(line) {
        return false;
    }
    split_table_row_cells(line)
        .iter()
        .all(|c| TABLE_SEPARATOR_CELL_RE.is_match(c))
}

fn utf8_len(s: &str) -> usize {
    s.len()
}

/// Final hard splitter: per-character UTF-8 byte accumulation.
pub fn split_by_utf8_byte_limit(text: &str, limit: usize) -> Vec<String> {
    if utf8_len(text) <= limit {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > limit && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[derive(Debug, Default)]
struct TableState {
    header: String,
    separator: String,
}

/// Chunk markdown so tables are never split mid-chunk without their header:
/// every chunk that carries table rows re-emits `header + separator` first;
/// fenced code blocks re-emit their opening fence per chunk and are closed at
/// chunk boundaries; anything still oversized falls to the UTF-8 byte
/// splitter. `limit` is clamped to [`QQBOT_MARKDOWN_SAFE_CHUNK_BYTE_LIMIT`].
pub fn chunk_qqbot_markdown_text(text: &str, limit: usize) -> Vec<String> {
    let limit = resolve_qqbot_markdown_chunk_limit(limit).max(8);
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_bytes = 0usize;
    let mut table: Option<TableState> = None;
    let mut fence_open: Option<String> = None;

    let flush =
        |chunks: &mut Vec<String>, current: &mut Vec<String>, current_bytes: &mut usize,
         fence_open: &Option<String>| {
            if current.is_empty() {
                return;
            }
            let mut body = current.join("\n");
            if let Some(fence) = fence_open {
                // Close the fence so each chunk is self-contained.
                let close = fence.trim_start().chars().take_while(|c| *c == '`' || *c == '~').collect::<String>();
                body.push('\n');
                body.push_str(&close);
            }
            chunks.push(body);
            current.clear();
            *current_bytes = 0;
        };

    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let line_bytes = utf8_len(line) + 1;

        if let Some(fence) = fence_open.clone() {
            // Inside a fence: keep body lines; split → reopen fence.
            if FENCE_LINE_RE.is_match(line) {
                fence_open = None;
            }
            if current_bytes + line_bytes > limit && !current.is_empty() {
                flush(&mut chunks, &mut current, &mut current_bytes, &Some(fence.clone()));
                if fence_open.is_some() {
                    current.push(fence.clone());
                    current_bytes += utf8_len(&fence) + 1;
                }
            }
            current.push(line.to_string());
            current_bytes += line_bytes;
            i += 1;
            continue;
        }

        if FENCE_LINE_RE.is_match(line) {
            table = None;
            fence_open = Some(line.to_string());
            if current_bytes + line_bytes > limit {
                flush(&mut chunks, &mut current, &mut current_bytes, &None);
            }
            current.push(line.to_string());
            current_bytes += line_bytes;
            i += 1;
            continue;
        }

        // Table start: header row followed by separator row.
        if table.is_none()
            && is_table_row_line(line)
            && i + 1 < lines.len()
            && is_table_separator_line(lines[i + 1])
        {
            let header = line.to_string();
            let separator = lines[i + 1].to_string();
            let head_bytes = utf8_len(&header) + utf8_len(&separator) + 2;
            if current_bytes + head_bytes > limit {
                flush(&mut chunks, &mut current, &mut current_bytes, &None);
            }
            current.push(header.clone());
            current.push(separator.clone());
            current_bytes += head_bytes;
            table = Some(TableState { header, separator });
            i += 2;
            continue;
        }

        if let Some(state) = &table {
            if is_table_row_line(line) {
                let single_row_message =
                    utf8_len(&state.header) + utf8_len(&state.separator) + utf8_len(line) + 2;
                if single_row_message > limit {
                    // Row alone (with header) exceeds the limit: render as
                    // `header: cell` field lines through the base splitter.
                    let headers = split_table_row_cells(&state.header);
                    let cells = split_table_row_cells(line);
                    flush(&mut chunks, &mut current, &mut current_bytes, &None);
                    let fields = headers
                        .iter()
                        .zip(cells.iter())
                        .map(|(h, c)| format!("{h}: {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    for piece in split_by_utf8_byte_limit(&fields, limit) {
                        chunks.push(piece);
                    }
                    i += 1;
                    continue;
                }
                if current_bytes + line_bytes > limit {
                    // Flush and restart the table chunk with header+separator.
                    flush(&mut chunks, &mut current, &mut current_bytes, &None);
                    current.push(state.header.clone());
                    current.push(state.separator.clone());
                    current_bytes += utf8_len(&state.header) + utf8_len(&state.separator) + 2;
                }
                current.push(line.to_string());
                current_bytes += line_bytes;
                i += 1;
                continue;
            }
            table = None;
        }

        // Plain text line.
        if line_bytes > limit {
            flush(&mut chunks, &mut current, &mut current_bytes, &None);
            for piece in split_by_utf8_byte_limit(line, limit) {
                chunks.push(piece);
            }
            i += 1;
            continue;
        }
        if current_bytes + line_bytes > limit {
            flush(&mut chunks, &mut current, &mut current_bytes, &None);
        }
        current.push(line.to_string());
        current_bytes += line_bytes;
        i += 1;
    }
    flush(&mut chunks, &mut current, &mut current_bytes, &fence_open);
    chunks.into_iter().filter(|c| !c.trim().is_empty()).collect()
}

// ============================================================================
// Outbound reasoning-tag sanitization (upstream
// `src/shared/text/reasoning-tags.ts`, applied by the base chunker)
// ============================================================================

static QUICK_TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)<\s*/?\s*(?:(?:antml:|mm:)?(?:think(?:ing)?|thought)|antthinking|final)\b")
        .unwrap()
});
static THINKING_TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)<\s*(/?)\s*(?:(?:antml:|mm:)?(?:think(?:ing)?|thought)|antthinking)\b[^<>]*>")
        .unwrap()
});
static FINAL_OPEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<\s*final\s*>").unwrap());
static FINAL_CLOSE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<\s*/\s*final\s*>").unwrap());

/// Strip model reasoning tags from outbound text (strict mode, trim both):
/// `<think>`/`<thinking>`/`<thought>` (optionally `antml:`/`mm:`-prefixed)
/// and `<antthinking>` blocks are removed with their content; `<final>`
/// blocks are unwrapped to their content; orphan close tags drop the
/// preamble before them; tags inside fenced code blocks are preserved.
pub fn strip_reasoning_tags(text: &str) -> String {
    if !QUICK_TAG_RE.is_match(text) {
        return text.to_string();
    }
    // Protect fenced code regions.
    let mut out = String::new();
    let mut rest = text;
    loop {
        match rest.find("```") {
            Some(start) => {
                let (before, from_fence) = rest.split_at(start);
                out.push_str(&strip_reasoning_tags_unprotected(before));
                let after_open = &from_fence[3..];
                match after_open.find("```") {
                    Some(end) => {
                        let fence_len = 3 + end + 3;
                        out.push_str(&from_fence[..fence_len]);
                        rest = &from_fence[fence_len..];
                    }
                    None => {
                        out.push_str(from_fence);
                        rest = "";
                    }
                }
                if rest.is_empty() {
                    break;
                }
            }
            None => {
                out.push_str(&strip_reasoning_tags_unprotected(rest));
                break;
            }
        }
    }
    out.trim().to_string()
}

fn strip_reasoning_tags_unprotected(text: &str) -> String {
    // Unwrap <final>...</final> to its content.
    let mut text = text.to_string();
    if let (Some(open), Some(close)) = (
        FINAL_OPEN_RE.find(&text).map(|m| (m.start(), m.end())),
        FINAL_CLOSE_RE.find(&text).map(|m| (m.start(), m.end())),
    ) {
        if open.1 <= close.0 {
            text = text[open.1..close.0].to_string();
        }
    }
    // Walk thinking tags tracking nesting depth.
    let mut result = String::new();
    let mut depth: i32 = 0;
    let mut last_end = 0usize;
    let mut first_open: Option<usize> = None;
    for caps in THINKING_TAG_RE.captures_iter(&text.clone()) {
        let mat = caps.get(0).unwrap();
        let is_close = &caps[1] == "/";
        if !is_close {
            if depth == 0 {
                result.push_str(&text[last_end..mat.start()]);
                if first_open.is_none() {
                    first_open = Some(mat.end());
                }
            }
            depth += 1;
        } else if depth > 0 {
            depth -= 1;
            if depth == 0 {
                last_end = mat.end();
            }
        } else {
            // Orphan close tag: drop the preamble before it.
            result.clear();
            last_end = mat.end();
        }
    }
    if depth == 0 {
        result.push_str(&text[last_end..]);
    } else if result.trim().is_empty() {
        // Strict mode: unclosed block would yield empty — fall back to the
        // content after the first open tag.
        if let Some(start) = first_open {
            result = text[start..].to_string();
        }
    }
    result.trim().to_string()
}

// ============================================================================
// State → SQLite KV (upstream `state-migrations.ts` +
// `engine/utils/state-keys.ts` / `sqlite-state.ts`)
// ============================================================================

pub const MAX_CREDENTIAL_BACKUPS: usize = 1000;
pub const CREDENTIAL_BACKUP_NAMESPACE: &str = "credential-backups";

/// `buildQQBotStateKey(...parts)` = sha256(JSON array of parts) hex.
pub fn build_qqbot_state_key(parts: &[&str]) -> String {
    let serialized = json!(parts).to_string();
    let digest = Sha256::digest(serialized.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `safeName`: `[^a-zA-Z0-9._-]` → `_`.
pub fn safe_account_file_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

/// A migrated credential backup record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialBackup {
    pub account_id: String,
    pub app_id: String,
    pub client_secret: String,
    pub saved_at: String,
}

impl CredentialBackup {
    pub fn from_value(value: &Value) -> Option<Self> {
        let account_id = value["accountId"].as_str()?.trim().to_string();
        let app_id = value["appId"].as_str()?.trim().to_string();
        let client_secret = value["clientSecret"].as_str()?.trim().to_string();
        if account_id.is_empty() || app_id.is_empty() || client_secret.is_empty() {
            return None;
        }
        Some(Self {
            account_id,
            app_id,
            client_secret,
            saved_at: value["savedAt"]
                .as_str()
                .unwrap_or("1970-01-01T00:00:00.000Z")
                .to_string(),
        })
    }
}

/// SQLite-backed namespaced KV store for QQBot plugin state (integration
/// point for `openSyncKeyedStore`; capacity enforced per namespace).
pub struct QqBotKvStore {
    conn: rusqlite::Connection,
}

impl QqBotKvStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS qqbot_kv (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                saved_at TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (namespace, key)
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM qqbot_kv WHERE namespace = ?1 AND key = ?2")?;
        let mut rows = stmt.query([namespace, key])?;
        match rows.next()? {
            Some(row) => {
                let raw: String = row.get(0)?;
                Ok(serde_json::from_str(&raw).ok())
            }
            None => Ok(None),
        }
    }

    pub fn count(&self, namespace: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM qqbot_kv WHERE namespace = ?1",
            [namespace],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }

    /// Insert only when absent (upstream `registerIfAbsent`), honoring the
    /// namespace capacity. Returns true when inserted.
    pub fn register_if_absent(
        &self,
        namespace: &str,
        key: &str,
        value: &Value,
        max_entries: usize,
    ) -> Result<bool> {
        if self.get(namespace, key)?.is_some() {
            return Ok(false);
        }
        if self.count(namespace)? >= max_entries {
            anyhow::bail!("namespace {namespace} is at capacity ({max_entries})");
        }
        self.conn.execute(
            "INSERT INTO qqbot_kv (namespace, key, value, saved_at) VALUES (?1, ?2, ?3, ?4)",
            [namespace, key, &value.to_string(), ""],
        )?;
        Ok(true)
    }

    pub fn delete(&self, namespace: &str, key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM qqbot_kv WHERE namespace = ?1 AND key = ?2",
            [namespace, key],
        )?;
        Ok(n > 0)
    }
}

/// Report of the credential-backup JSON → SQLite migration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CredentialMigrationReport {
    pub migrated: usize,
    pub skipped: usize,
    pub archived: Vec<PathBuf>,
}

/// `qqbot-credential-backups-json-to-plugin-state`: moves legacy
/// `credential-backup-{safeName}.json` (sorted) then the singleton
/// `credential-backup.json` from `data_dir` into the
/// [`CREDENTIAL_BACKUP_NAMESPACE`] KV slice. Per-account snapshots win per
/// key (processed first; `register_if_absent` keeps the first key seen).
/// Files whose name suffix does not match `safeName(accountId)` are skipped.
/// Successfully migrated sources are archived to `{path}.migrated`.
pub fn migrate_credential_backups(
    data_dir: &Path,
    store: &QqBotKvStore,
) -> Result<CredentialMigrationReport> {
    let mut report = CredentialMigrationReport::default();
    let mut per_account: Vec<PathBuf> = Vec::new();
    let mut singleton: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name == "credential-backup.json" {
                singleton = Some(path);
            } else if name.starts_with("credential-backup-") && name.ends_with(".json") {
                per_account.push(path);
            }
        }
    }
    per_account.sort();
    let ordered = per_account.into_iter().chain(singleton);
    for path in ordered {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            report.skipped += 1;
            continue;
        };
        let Some(backup) = serde_json::from_str::<Value>(&raw)
            .ok()
            .as_ref()
            .and_then(CredentialBackup::from_value)
        else {
            report.skipped += 1;
            continue;
        };
        // Per-account file names must match safeName(accountId).
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name != "credential-backup.json" {
            let expected = format!("credential-backup-{}.json", safe_account_file_name(&backup.account_id));
            if name != expected {
                report.skipped += 1;
                continue;
            }
        }
        let key = build_qqbot_state_key(&["credential-backup", &backup.account_id]);
        let value = json!({
            "accountId": backup.account_id,
            "appId": backup.app_id,
            "clientSecret": backup.client_secret,
            "savedAt": backup.saved_at,
        });
        let inserted = store.register_if_absent(
            CREDENTIAL_BACKUP_NAMESPACE,
            &key,
            &value,
            MAX_CREDENTIAL_BACKUPS,
        )?;
        if inserted {
            report.migrated += 1;
        } else {
            report.skipped += 1;
        }
        let archived = path.with_extension("json.migrated");
        if !archived.exists() && std::fs::rename(&path, &archived).is_ok() {
            report.archived.push(archived);
        }
    }
    Ok(report)
}

// ============================================================================
// Sandbox media send scoping (upstream
// `engine/messaging/trusted-media-path.ts`)
// ============================================================================

/// Root-sandbox outbound local media: a path is trusted only when it resolves
/// (symlinks followed) under one of the allowed roots (QQBot payload/storage
/// roots + the hardened temp root upstream). `allow_missing` permits a
/// not-yet-flushed file (e.g. in-progress TTS) whose parent resolves under a
/// root — callers must re-check existence at send time.
pub fn resolve_trusted_outbound_media_path(
    path: &Path,
    allowed_roots: &[PathBuf],
    allow_missing: bool,
) -> Option<PathBuf> {
    let resolved = match path.canonicalize() {
        Ok(p) => p,
        Err(_) if allow_missing => {
            let parent = path.parent()?.canonicalize().ok()?;
            parent.join(path.file_name()?)
        }
        Err(_) => return None,
    };
    for root in allowed_roots {
        let Ok(root) = root.canonicalize() else { continue };
        if resolved.starts_with(&root) {
            return Some(resolved);
        }
    }
    None
}

// ============================================================================
// C2C typing window (upstream `engine/gateway/typing-keepalive.ts`)
// ============================================================================

pub const TYPING_INTERVAL_MS: u64 = 5_000;
pub const TYPING_INPUT_SECONDS: u32 = 10;
pub const QQ_C2C_PASSIVE_REPLY_LIMIT: u32 = 5;
pub const INITIAL_TYPING_NOTIFY_COUNT: u32 = 1;
pub const FINAL_REPLY_RESERVE_COUNT: u32 = 1;
/// 5 total passive replies − 1 initial notify − 1 reserved for the final
/// reply = 3 renewals.
pub const TYPING_RENEWAL_LIMIT: u32 =
    QQ_C2C_PASSIVE_REPLY_LIMIT - INITIAL_TYPING_NOTIFY_COUNT - FINAL_REPLY_RESERVE_COUNT;

/// Typing keepalive budget: refresh every 5s, each notify declares a 10s
/// window, bounded so a final passive reply slot always remains.
#[derive(Debug, Clone, Copy)]
pub struct TypingBudget {
    renewals_remaining: u32,
}

impl Default for TypingBudget {
    fn default() -> Self {
        Self { renewals_remaining: TYPING_RENEWAL_LIMIT }
    }
}

impl TypingBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one renewal; false once the budget is exhausted (stop the
    /// keepalive interval).
    pub fn try_renew(&mut self) -> bool {
        if self.renewals_remaining == 0 {
            return false;
        }
        self.renewals_remaining -= 1;
        true
    }

    pub fn remaining(&self) -> u32 {
        self.renewals_remaining
    }
}

// ============================================================================
// Response watchdog (upstream `engine/gateway/response-timeout.ts`)
// ============================================================================

pub const DEFAULT_RESPONSE_TIMEOUT_MS: u64 = 300_000;
/// `MAX_TIMER_TIMEOUT_MS` (JS setTimeout cap, 2^31 − 1).
pub const MAX_TIMER_TIMEOUT_MS: u64 = 2_147_483_647;

fn finite_seconds_to_ms(seconds: f64) -> Option<u64> {
    if seconds.is_finite() && seconds > 0.0 {
        Some((seconds * 1000.0) as u64)
    } else {
        None
    }
}

/// Watchdog timeout = `min(max(5min, agents.defaults.timeoutSeconds, max over
/// models.providers.*.timeoutSeconds), MAX_TIMER)`.
pub fn resolve_response_timeout_ms(
    agent_timeout_seconds: Option<f64>,
    provider_timeout_seconds: &[f64],
) -> u64 {
    let mut timeout = DEFAULT_RESPONSE_TIMEOUT_MS;
    if let Some(ms) = agent_timeout_seconds.and_then(finite_seconds_to_ms) {
        timeout = timeout.max(ms);
    }
    if let Some(ms) = provider_timeout_seconds
        .iter()
        .filter_map(|s| finite_seconds_to_ms(*s))
        .max()
    {
        timeout = timeout.max(ms);
    }
    timeout.min(MAX_TIMER_TIMEOUT_MS)
}

/// Reply watchdog: fires (aborting the reply path with "Response timeout")
/// only when no response has been produced by the deadline.
#[derive(Debug, Clone, Copy)]
pub struct ResponseWatchdog {
    pub started_ms: u64,
    pub timeout_ms: u64,
}

impl ResponseWatchdog {
    pub fn new(started_ms: u64, timeout_ms: u64) -> Self {
        Self { started_ms, timeout_ms }
    }

    pub fn fired(&self, now_ms: u64, has_response: bool) -> bool {
        !has_response && now_ms.saturating_sub(self.started_ms) >= self.timeout_ms
    }
}

// ============================================================================
// Failed-media surfacing (upstream `engine/messaging/outbound-types.ts` +
// `outbound-result-helpers.ts`)
// ============================================================================

pub const DEFAULT_MEDIA_SEND_ERROR: &str = "发送失败，请稍后重试。";
pub const OUTBOUND_ERROR_FILE_TOO_LARGE: &str = "file_too_large";
pub const OUTBOUND_ERROR_UPLOAD_DAILY_LIMIT: &str = "upload_daily_limit_exceeded";

/// User-facing text for a failed media send (upstream
/// `resolveUserFacingMediaError`): the raw error is only surfaced for the
/// specific size/quota codes; everything else collapses to the generic retry
/// message so internal errors don't leak.
pub fn resolve_user_facing_media_error(
    error: Option<&str>,
    error_code: Option<&str>,
) -> String {
    match (error, error_code) {
        (Some(e), Some(OUTBOUND_ERROR_FILE_TOO_LARGE | OUTBOUND_ERROR_UPLOAD_DAILY_LIMIT)) => {
            e.to_string()
        }
        _ => DEFAULT_MEDIA_SEND_ERROR.to_string(),
    }
}

/// Daily 2G upload-limit message (upstream literal).
pub fn format_daily_limit_message(dir: &str, name: &str, size: &str) -> String {
    format!(
        "QQBot每天发送文件有累计2G的限制，如果着急的话，可以直接来我的主机copy下载，文件目录`{dir}/{name}`（{size}）"
    )
}

/// Oversized-file message (upstream literal).
pub fn format_too_large_message(type_name: &str, file_size: &str, limit_mb: u64) -> String {
    format!("{type_name}过大（{file_size}），超过了{limit_mb}M，暂时不能通过QQ直接发给你。")
}

/// Whether the failure-fallback text chunk must be sent after auto media
/// delivery (upstream `outbound-deliver.ts`): no visible text/inline image
/// and nothing was sent.
pub fn should_send_media_failure_fallback(
    has_visible_text_or_inline_image: bool,
    sent_media_count: usize,
) -> bool {
    !has_visible_text_or_inline_image && sent_media_count == 0
}

// ============================================================================
// Channel plugin
// ============================================================================

pub struct QqBotChannel {
    config: QqBotExtensionConfig,
}

impl QqBotChannel {
    pub fn new(config: &Config) -> Self {
        Self {
            config: QqBotExtensionConfig::from_extensions_value(
                config.channels.extensions.get("qqbot"),
            ),
        }
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled.unwrap_or(false)
    }

    /// Effective TTS config for an account (deep-merge helper shared with
    /// Feishu).
    pub fn account_tts(&self, base: Option<&Value>, account_id: &str) -> Value {
        resolve_qqbot_account_tts(base, &self.config, account_id)
    }

    /// Raw account config resolution (shallow account-over-channel spread).
    pub fn account_config(&self, account_id: &str) -> Value {
        let channel = serde_json::to_value(json!({
            "appId": self.config.app_id,
            "clientSecret": self.config.client_secret,
            "defaultRequireMention": self.config.default_require_mention,
        }))
        .unwrap_or_else(|_| json!({}));
        let account = self
            .config
            .accounts
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        deep_merge_defined(&json!({}), &resolve_account_config(&channel, &account))
    }
}

#[async_trait]
impl ChannelPlugin for QqBotChannel {
    fn id(&self) -> &str {
        "qqbot"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "QQ Bot".to_string(),
            description: "Tencent QQ open-platform bot channel".to_string(),
            enabled: self.is_enabled(),
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::Groups,
            ChannelCapability::TypingIndicators,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if self.is_enabled() {
            // Integration point: connect to the QQ open-platform gateway
            // (WebSocket) here; inbound events feed `QqBotMessageQueue` and
            // the group gate, outbound replies go through the markdown table
            // chunker + reasoning-tag sanitizer and (for C2C) the streaming
            // controller.
            info!("QQBot channel starting");
        }
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let sanitized = strip_reasoning_tags(message);
        let chunks = chunk_qqbot_markdown_text(&sanitized, QQBOT_TEXT_CHUNK_LIMIT);
        info!(
            to = to,
            chunks = chunks.len(),
            "QQBot: send prepared (gateway transport not wired in this port)"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Group config / mention gating ----------------------------------

    fn config_with_groups() -> QqBotExtensionConfig {
        QqBotExtensionConfig {
            default_require_mention: Some(false),
            groups: HashMap::from([
                (
                    "g1".to_string(),
                    QqBotGroupConfig { require_mention: Some(true), history_limit: Some(7.9), ..Default::default() },
                ),
                (
                    "*".to_string(),
                    QqBotGroupConfig { ignore_other_mentions: Some(true), ..Default::default() },
                ),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn group_config_precedence() {
        let cfg = config_with_groups();
        let g1 = resolve_group_config(&cfg, "g1");
        assert!(g1.require_mention); // specific beats defaultRequireMention
        assert!(g1.ignore_other_mentions); // inherited from wildcard
        assert_eq!(g1.history_limit, 7); // floored
        let other = resolve_group_config(&cfg, "someother");
        assert!(!other.require_mention); // defaultRequireMention=false
        assert_eq!(other.history_limit, DEFAULT_GROUP_HISTORY_LIMIT);
        assert_eq!(other.display_name, "someothe"); // first 8 chars
        // No config at all → require_mention default true.
        let bare = resolve_group_config(&QqBotExtensionConfig::default(), "g");
        assert!(bare.require_mention);
    }

    #[test]
    fn mention_detection_and_stripping() {
        let mentions = vec![QqMention {
            member_openid: Some("BOT123".into()),
            is_you: true,
            ..Default::default()
        }];
        assert!(detect_was_mentioned(&mentions, "GROUP_MESSAGE_CREATE", &[], "hi"));
        assert!(detect_was_mentioned(&[], "GROUP_AT_MESSAGE_CREATE", &[], "hi"));
        assert!(detect_was_mentioned(&[], "X", &["laoban".to_string()], "hey LAOBAN help"));
        // Invalid pattern silently skipped.
        assert!(!detect_was_mentioned(&[], "X", &["([".to_string()], "hey"));
        assert!(has_any_mention(&[], "yo <@!W123> hello"));
        assert!(!has_any_mention(&[], "no mentions"));
        assert_eq!(strip_mention_text("<@!BOT123> do the thing", &mentions), "do the thing");
        let other = vec![QqMention {
            id: Some("U9".into()),
            nickname: Some("Nine".into()),
            ..Default::default()
        }];
        assert_eq!(strip_mention_text("ask <@U9> too", &other), "ask @Nine too");
        assert!(resolve_implicit_mention(Some(true)));
        assert!(!resolve_implicit_mention(None));
    }

    #[test]
    fn group_gate_ordering_and_bypass() {
        // Other-mention drop first.
        let d = resolve_group_message_gate(GroupGateInput {
            ignore_other_mentions: true,
            has_any_mention: true,
            require_mention: true,
            can_detect_mention: true,
            ..Default::default()
        });
        assert_eq!(d.action, GroupGateAction::DropOtherMention);
        // Unauthorized command blocked.
        let d = resolve_group_message_gate(GroupGateInput {
            allow_text_commands: true,
            is_control_command: true,
            command_authorized: false,
            ..Default::default()
        });
        assert_eq!(d.action, GroupGateAction::BlockUnauthorizedCommand);
        // Authorized control command bypasses the mention requirement.
        let d = resolve_group_message_gate(GroupGateInput {
            require_mention: true,
            can_detect_mention: true,
            allow_text_commands: true,
            is_control_command: true,
            command_authorized: true,
            ..Default::default()
        });
        assert_eq!(d.action, GroupGateAction::Pass);
        assert!(d.should_bypass_mention);
        assert!(d.effective_was_mentioned);
        // No mention → skip.
        let d = resolve_group_message_gate(GroupGateInput {
            require_mention: true,
            can_detect_mention: true,
            ..Default::default()
        });
        assert_eq!(d.action, GroupGateAction::SkipNoMention);
        // Implicit mention passes.
        let d = resolve_group_message_gate(GroupGateInput {
            require_mention: true,
            can_detect_mention: true,
            implicit_mention: true,
            ..Default::default()
        });
        assert_eq!(d.action, GroupGateAction::Pass);
    }

    // ---- History ---------------------------------------------------------

    #[test]
    fn history_record_context_and_clear() {
        let mut store = GroupHistoryStore::new();
        store.record("g1", HistoryEntry { sender: "A".into(), body: "one".into(), ..Default::default() }, 2);
        store.record("g1", HistoryEntry { sender: "B".into(), body: "two".into(), ..Default::default() }, 2);
        store.record("g1", HistoryEntry { sender: "C".into(), body: "three".into(), ..Default::default() }, 2);
        let ctx = store.build_context("g1", "current msg", 2);
        assert!(ctx.starts_with(HISTORY_CTX_START));
        assert!(!ctx.contains("[A]: one")); // trimmed to limit 2
        assert!(ctx.contains("[B]: two"));
        assert!(ctx.contains("[C]: three"));
        assert!(ctx.ends_with("current msg"));
        assert!(ctx.contains(HISTORY_CTX_END));
        store.clear("g1");
        assert_eq!(store.build_context("g1", "m", 2), "m");
        // limit 0 disables.
        store.record("g2", HistoryEntry::default(), 0);
        assert_eq!(store.tracked_groups(), 1);
        // Merged context wrapper.
        let merged = build_merged_message_context(&["[A]: hi".to_string()], "now");
        assert!(merged.starts_with(MERGED_CTX_START));
        assert!(merged.contains(MERGED_CTX_END));
        assert_eq!(build_merged_message_context(&[], "now"), "now");
    }

    #[test]
    fn history_key_lru_eviction() {
        let mut store = GroupHistoryStore::new();
        for i in 0..(MAX_HISTORY_KEYS + 5) {
            store.record(
                &format!("g{i}"),
                HistoryEntry { sender: "s".into(), body: "b".into(), ..Default::default() },
                5,
            );
        }
        assert_eq!(store.tracked_groups(), MAX_HISTORY_KEYS);
        // Oldest keys evicted.
        assert_eq!(store.build_context("g0", "m", 5), "m");
    }

    // ---- Queue -----------------------------------------------------------

    fn msg(id: &str, content: &str, bot: bool) -> QueuedMessage {
        QueuedMessage {
            message_id: id.to_string(),
            content: content.to_string(),
            sender_id: format!("s-{id}"),
            sender_name: Some(format!("N{id}")),
            sender_is_bot: bot,
            event_type: "GROUP_MESSAGE_CREATE".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn queue_fifo_order_and_eviction() {
        let mut q = QqBotMessageQueue::new();
        let peer = peer_id_group("gA");
        assert!(is_group_peer(&peer));
        assert!(!is_group_peer(&peer_id_dm("u1")));
        for i in 0..DEFAULT_GROUP_QUEUE_SIZE {
            assert_eq!(q.enqueue(&peer, msg(&format!("m{i}"), "x", i == 3)), QueueEviction::None);
        }
        // Full group queue evicts the first bot-authored message.
        assert_eq!(q.enqueue(&peer, msg("overflow", "x", false)), QueueEviction::BotAuthored);
        // Now no bot messages left → oldest evicted.
        assert_eq!(q.enqueue(&peer, msg("overflow2", "x", false)), QueueEviction::Oldest);
        assert_eq!(q.pending(&peer), DEFAULT_GROUP_QUEUE_SIZE);
        // Batch drain preserves FIFO order.
        assert!(q.try_activate(&peer));
        assert!(!q.try_activate(&peer)); // already active
        let batch = q.take_batch(&peer);
        assert_eq!(batch.first().unwrap().message_id, "m1"); // m0 evicted as oldest? no: m3 (bot) evicted first, then m0
        let ids: Vec<_> = batch.iter().map(|m| m.message_id.as_str()).collect();
        // m3 was evicted (bot), then m0 (oldest); order of the rest intact.
        assert!(!ids.contains(&"m3"));
        assert!(!ids.contains(&"m0"));
        let pos1 = ids.iter().position(|i| *i == "m1").unwrap();
        let pos2 = ids.iter().position(|i| *i == "m2").unwrap();
        let posn = ids.iter().position(|i| *i == "overflow2").unwrap();
        assert!(pos1 < pos2 && pos2 < posn);
        q.deactivate(&peer);
        assert_eq!(q.pending(&peer), 0);
        // DM peers drain one at a time.
        let dm = peer_id_dm("u1");
        q.enqueue(&dm, msg("d1", "x", false));
        q.enqueue(&dm, msg("d2", "x", false));
        assert_eq!(q.take_batch(&dm).len(), 1);
        assert_eq!(q.take_batch(&dm)[0].message_id, "d2");
    }

    #[test]
    fn queue_concurrency_cap() {
        let mut q = QqBotMessageQueue { max_concurrent_users: 2, ..Default::default() };
        assert!(q.try_activate("dm:a"));
        assert!(q.try_activate("dm:b"));
        assert!(!q.try_activate("dm:c"));
        q.deactivate("dm:a");
        assert!(q.try_activate("dm:c"));
    }

    #[test]
    fn group_batch_commands_first_then_merge() {
        let batch = vec![
            msg("m1", "hello", false),
            msg("m2", "/status", false),
            msg("m3", "world", false),
        ];
        let plan = plan_group_batch(batch);
        assert_eq!(plan.command_turns.len(), 1);
        assert_eq!(plan.command_turns[0].message_id, "m2");
        let merged = plan.merged_turn.unwrap();
        assert!(is_merged_turn(&merged));
        assert_eq!(merged.merge_count, 2);
        assert_eq!(merged.content, "[Nm1]: hello\n[Nm3]: world");
        assert_eq!(merged.message_id, "m3"); // identity from last
        assert!(!merged.sender_is_bot);
    }

    #[test]
    fn merge_group_messages_mentions_and_event_type() {
        let mut a = msg("a", "one", true);
        a.mentions = vec![QqMention { member_openid: Some("X".into()), ..Default::default() }];
        let mut b = msg("b", "two", true);
        b.mentions = vec![
            QqMention { member_openid: Some("X".into()), ..Default::default() },
            QqMention { id: Some("Y".into()), ..Default::default() },
        ];
        b.event_type = "GROUP_AT_MESSAGE_CREATE".to_string();
        let merged = merge_group_messages(vec![a, b]).unwrap();
        assert_eq!(merged.mentions.len(), 2); // deduped by openid
        assert_eq!(merged.event_type, "GROUP_AT_MESSAGE_CREATE");
        assert!(merged.sender_is_bot); // all bot
        // Single-element batch returned as-is.
        let one = merge_group_messages(vec![msg("z", "solo", false)]).unwrap();
        assert_eq!(one.merge_count, 0);
        assert!(merge_group_messages(vec![]).is_none());
    }

    // ---- C2C streaming ---------------------------------------------------

    #[test]
    fn c2c_suffix_segmentation_and_sessions() {
        let mut c = C2cStreamController::new(1, 100);
        assert_eq!(c.throttle_ms, C2C_THROTTLE_MIN_MS); // clamped up
        // Whitespace-only first chunk defers.
        assert!(c.on_partial("  ").is_none());
        assert_eq!(c.phase(), C2cStreamPhase::Idle);
        let chunk = c.on_partial("Hello").unwrap();
        assert_eq!(chunk.content, "Hello");
        assert_eq!(chunk.state, StreamInputState::Generating);
        assert_eq!((chunk.msg_seq, chunk.index), (1, 1));
        let chunk = c.on_partial("Hello world").unwrap();
        assert_eq!(chunk.content, "Hello world"); // REPLACE mode: full session suffix
        assert_eq!(chunk.index, 2);
        // End session (e.g. media interleave): DONE chunk + suffix consumed.
        let done = c.end_session(2).unwrap();
        assert_eq!(done.state, StreamInputState::Done);
        assert_eq!(done.content, "Hello world");
        // New session streams only the new suffix.
        let chunk = c.on_partial("Hello world and more").unwrap();
        assert_eq!(chunk.content, " and more");
        assert_eq!((chunk.msg_seq, chunk.index), (2, 1));
        let fin = c.finish(false).unwrap();
        assert_eq!(fin.state, StreamInputState::Done);
        assert_eq!(fin.content, " and more");
        assert_eq!(c.phase(), C2cStreamPhase::Completed);
        assert!(!c.should_fallback_to_static());
        // Terminal: further partials ignored.
        assert!(c.on_partial("Hello world and more!").is_none());
    }

    #[test]
    fn c2c_reply_boundary_prefix_merge() {
        let mut c = C2cStreamController::new(7, 500);
        c.on_partial("First reply.").unwrap();
        // New payload does not extend the previous raw text → boundary.
        let chunk = c.on_partial("Second").unwrap();
        assert_eq!(chunk.content, "First reply.\n\nSecond");
        // Subsequent growth keeps the prefix.
        let chunk = c.on_partial("Second part").unwrap();
        assert_eq!(chunk.content, "First reply.\n\nSecond part");
    }

    #[test]
    fn c2c_fallback_and_error_paths() {
        let mut c = C2cStreamController::new(1, 500);
        c.abort(); // deliver arrived before any partial
        assert!(c.should_fallback_to_static());
        assert!(c.finish(false).is_none());

        let mut c = C2cStreamController::new(1, 500);
        c.on_partial("partial").unwrap();
        let fin = c.finish(true).unwrap();
        assert!(fin.content.ends_with(C2C_ERROR_SUFFIX));
        assert!(!c.should_fallback_to_static());
    }

    #[test]
    fn c2c_enablement_gate() {
        assert!(should_use_official_c2c_stream("c2c", Some(&json!(true))));
        assert!(should_use_official_c2c_stream("c2c", Some(&json!({"mode": "partial", "c2cStreamApi": true}))));
        assert!(!should_use_official_c2c_stream("c2c", Some(&json!({"mode": "partial"}))));
        assert!(!should_use_official_c2c_stream("c2c", Some(&json!(false))));
        assert!(!should_use_official_c2c_stream("group", Some(&json!(true))));
        assert!(!should_use_official_c2c_stream("c2c", None));
    }

    // ---- /bot-group-allways ---------------------------------------------

    #[test]
    fn bot_group_allways_parsing_and_write() {
        assert_eq!(parse_bot_group_allways(None), GroupAllwaysAction::Status);
        assert_eq!(parse_bot_group_allways(Some(" ")), GroupAllwaysAction::Status);
        assert_eq!(
            parse_bot_group_allways(Some("on")),
            GroupAllwaysAction::Set { require_mention: false }
        );
        assert_eq!(
            parse_bot_group_allways(Some("off")),
            GroupAllwaysAction::Set { require_mention: true }
        );
        assert_eq!(parse_bot_group_allways(Some("maybe")), GroupAllwaysAction::Invalid);

        // Default account → top-level key.
        let mut cfg = json!({"appId": "1"});
        apply_default_require_mention(&mut cfg, "default", false);
        assert_eq!(cfg["defaultRequireMention"], false);
        // Named account present → account-level key.
        let mut cfg = json!({"accounts": {"work": {"appId": "2"}}});
        apply_default_require_mention(&mut cfg, "work", true);
        assert_eq!(cfg["accounts"]["work"]["defaultRequireMention"], true);
        assert!(cfg.get("defaultRequireMention").is_none());
        // Named but absent account → top-level.
        let mut cfg = json!({});
        apply_default_require_mention(&mut cfg, "ghost", true);
        assert_eq!(cfg["defaultRequireMention"], true);
    }

    #[test]
    fn slash_command_auth_requires_explicit_entries() {
        let allow = vec![json!("123"), json!(456)];
        assert!(resolve_slash_command_auth("123", false, &allow, &[], None));
        assert!(resolve_slash_command_auth("456", false, &allow, &[], None));
        assert!(!resolve_slash_command_auth("789", false, &allow, &[], None));
        // Wildcard-only or empty → deny.
        assert!(!resolve_slash_command_auth("123", false, &[json!("*")], &[], None));
        assert!(!resolve_slash_command_auth("123", false, &[], &[], None));
        // Group prefers groupAllowFrom when non-empty.
        assert!(resolve_slash_command_auth("g1", true, &allow, &[json!("g1")], None));
        assert!(!resolve_slash_command_auth("123", true, &allow, &[json!("g1")], None));
        // commands.allowFrom overrides everything.
        assert!(resolve_slash_command_auth("c1", true, &allow, &[json!("g1")], Some(&[json!("c1")])));
        assert!(permission_denied_text("bot-group-allways").contains("/bot-group-allways"));
        assert!(is_explicit_qqbot_c2c_from("qqbot:c2c:12345"));
        assert!(!is_explicit_qqbot_c2c_from("qqbot:group:g"));
        assert!(is_command_turn("  /status"));
        assert!(!is_command_turn("status /x"));
    }

    // ---- Markdown table chunking ----------------------------------------

    #[test]
    fn table_chunks_always_carry_header() {
        let header = "| Name | Value |";
        let sep = "| --- | --- |";
        let rows: Vec<String> = (0..60)
            .map(|i| format!("| item-{i} | {} |", "v".repeat(60)))
            .collect();
        let table = format!("{header}\n{sep}\n{}", rows.join("\n"));
        let chunks = chunk_qqbot_markdown_text(&table, 4000);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= QQBOT_MARKDOWN_SAFE_CHUNK_BYTE_LIMIT);
            let lines: Vec<&str> = chunk.lines().collect();
            // Every table chunk restarts with header + separator.
            assert_eq!(lines[0], header, "chunk missing header: {chunk}");
            assert_eq!(lines[1], sep);
            // No row split mid-line.
            for row in &lines[2..] {
                assert!(is_table_row_line(row), "partial row leaked: {row}");
            }
        }
        // All rows preserved exactly once.
        let total_rows: usize = chunks.iter().map(|c| c.lines().count() - 2).sum();
        assert_eq!(total_rows, 60);
    }

    #[test]
    fn oversized_row_renders_as_field_lines() {
        let text = format!(
            "| H1 | H2 |\n| --- | --- |\n| tiny | {} |",
            "x".repeat(5000)
        );
        let chunks = chunk_qqbot_markdown_text(&text, 4000);
        assert!(chunks.iter().any(|c| c.contains("H1: tiny")));
    }

    #[test]
    fn fences_reopened_across_chunks_and_byte_limit() {
        let body: Vec<String> = (0..300).map(|i| format!("line {i} {}", "y".repeat(20))).collect();
        let text = format!("```rust\n{}\n```", body.join("\n"));
        let chunks = chunk_qqbot_markdown_text(&text, 4000);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            // Each chunk is fence-balanced.
            let fence_count = chunk.lines().filter(|l| l.trim_start().starts_with("```")).count();
            assert_eq!(fence_count % 2, 0, "unbalanced fence in chunk: {chunk}");
        }
        // Limit clamp + escaped pipes.
        assert_eq!(resolve_qqbot_markdown_chunk_limit(5000), 3600);
        assert_eq!(resolve_qqbot_markdown_chunk_limit(1000), 1000);
        assert_eq!(split_table_row_cells(r"| a\|b | c |"), vec![r"a\|b", "c"]);
        assert!(is_table_separator_line("| :--- | ---: |"));
        assert!(!is_table_separator_line("| a | b |"));
        let pieces = split_by_utf8_byte_limit(&"字".repeat(100), 30);
        assert!(pieces.iter().all(|p| p.len() <= 30));
        assert_eq!(pieces.concat(), "字".repeat(100));
    }

    // ---- Reasoning tags ---------------------------------------------------

    #[test]
    fn reasoning_tags_stripped() {
        assert_eq!(
            strip_reasoning_tags("<think>internal</think>The answer is 42."),
            "The answer is 42."
        );
        assert_eq!(
            strip_reasoning_tags("<thinking>x</thinking>ok"),
            "ok"
        );
        assert_eq!(strip_reasoning_tags("<final>done</final>"), "done");
        // Orphan close drops the preamble.
        assert_eq!(strip_reasoning_tags("leaked preamble</think>answer"), "answer");
        // Unclosed block falls back to content after the open tag.
        assert_eq!(strip_reasoning_tags("<thinking>only body"), "only body");
        // Nested blocks removed entirely.
        assert_eq!(
            strip_reasoning_tags("<think>a<think>b</think>c</think>out"),
            "out"
        );
        // Tags inside fenced code preserved.
        let code = "```\n<think>not stripped</think>\n```";
        assert_eq!(strip_reasoning_tags(code), code);
        // No tags → untouched fast path.
        assert_eq!(strip_reasoning_tags("plain"), "plain");
    }

    // ---- SQLite KV + migration -------------------------------------------

    #[test]
    fn state_key_and_kv_roundtrip() {
        let k1 = build_qqbot_state_key(&["credential-backup", "default"]);
        let k2 = build_qqbot_state_key(&["credential-backup", "default"]);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
        assert_ne!(k1, build_qqbot_state_key(&["credential-backup", "other"]));

        let store = QqBotKvStore::open_in_memory().unwrap();
        assert!(store.register_if_absent("ns", "k", &json!({"a": 1}), 10).unwrap());
        assert!(!store.register_if_absent("ns", "k", &json!({"a": 2}), 10).unwrap());
        assert_eq!(store.get("ns", "k").unwrap().unwrap()["a"], 1);
        assert_eq!(store.count("ns").unwrap(), 1);
        assert!(store.delete("ns", "k").unwrap());
        assert!(store.get("ns", "k").unwrap().is_none());
        // Capacity enforcement.
        let store = QqBotKvStore::open_in_memory().unwrap();
        assert!(store.register_if_absent("ns", "a", &json!(1), 1).unwrap());
        assert!(store.register_if_absent("ns", "b", &json!(2), 1).is_err());
    }

    #[test]
    fn credential_backup_migration() {
        let dir = tempfile::tempdir().unwrap();
        let store = QqBotKvStore::open_in_memory().unwrap();
        // Valid per-account file.
        std::fs::write(
            dir.path().join("credential-backup-work.json"),
            r#"{"accountId":"work","appId":"111","clientSecret":"s1"}"#,
        )
        .unwrap();
        // Singleton for the same account: per-account snapshot wins.
        std::fs::write(
            dir.path().join("credential-backup.json"),
            r#"{"accountId":"work","appId":"222","clientSecret":"s2","savedAt":"2026-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        // Mismatched filename suffix → skipped.
        std::fs::write(
            dir.path().join("credential-backup-evil.json"),
            r#"{"accountId":"other","appId":"333","clientSecret":"s3"}"#,
        )
        .unwrap();
        // Invalid content → skipped.
        std::fs::write(dir.path().join("credential-backup-bad.json"), "{").unwrap();

        let report = migrate_credential_backups(dir.path(), &store).unwrap();
        assert_eq!(report.migrated, 1);
        assert_eq!(report.skipped, 3); // singleton dup + mismatch + invalid
        let key = build_qqbot_state_key(&["credential-backup", "work"]);
        let stored = store.get(CREDENTIAL_BACKUP_NAMESPACE, &key).unwrap().unwrap();
        assert_eq!(stored["appId"], "111"); // per-account file won
        assert_eq!(stored["savedAt"], "1970-01-01T00:00:00.000Z"); // epoch default
        // Migrated source archived.
        assert!(!dir.path().join("credential-backup-work.json").exists());
        assert!(dir.path().join("credential-backup-work.json.migrated").exists());
        assert_eq!(safe_account_file_name("a/b:c"), "a_b_c");
    }

    // ---- Sandbox media ----------------------------------------------------

    #[test]
    fn trusted_media_path_scoping() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_file = root.path().join("clip.ogg");
        std::fs::write(&inside_file, b"x").unwrap();
        let outside_file = outside.path().join("evil.ogg");
        std::fs::write(&outside_file, b"x").unwrap();
        let roots = vec![root.path().to_path_buf()];
        assert!(resolve_trusted_outbound_media_path(&inside_file, &roots, false).is_some());
        assert!(resolve_trusted_outbound_media_path(&outside_file, &roots, false).is_none());
        // Missing file rejected unless allow_missing (in-progress TTS temp).
        let pending = root.path().join("pending.ogg");
        assert!(resolve_trusted_outbound_media_path(&pending, &roots, false).is_none());
        assert!(resolve_trusted_outbound_media_path(&pending, &roots, true).is_some());
        let pending_outside = outside.path().join("pending.ogg");
        assert!(resolve_trusted_outbound_media_path(&pending_outside, &roots, true).is_none());
    }

    // ---- Typing / watchdog / media errors ---------------------------------

    #[test]
    fn typing_budget_renewals() {
        assert_eq!(TYPING_RENEWAL_LIMIT, 3);
        let mut budget = TypingBudget::new();
        assert!(budget.try_renew());
        assert!(budget.try_renew());
        assert!(budget.try_renew());
        assert!(!budget.try_renew()); // exhausted: final reply slot reserved
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn watchdog_timeout_resolution() {
        // Floor at 5 min.
        assert_eq!(resolve_response_timeout_ms(None, &[]), 300_000);
        assert_eq!(resolve_response_timeout_ms(Some(60.0), &[120.0]), 300_000);
        // Larger configured timeouts win.
        assert_eq!(resolve_response_timeout_ms(Some(600.0), &[]), 600_000);
        assert_eq!(resolve_response_timeout_ms(Some(400.0), &[900.0, 500.0]), 900_000);
        // Non-finite ignored; timer-safe cap.
        assert_eq!(resolve_response_timeout_ms(Some(f64::NAN), &[f64::INFINITY]), 300_000);
        assert_eq!(
            resolve_response_timeout_ms(Some(1e10), &[]),
            MAX_TIMER_TIMEOUT_MS
        );
        let wd = ResponseWatchdog::new(0, 300_000);
        assert!(!wd.fired(299_999, false));
        assert!(wd.fired(300_000, false));
        assert!(!wd.fired(300_000, true)); // response arrived → never fires
    }

    #[test]
    fn failed_media_surfacing() {
        assert_eq!(
            resolve_user_facing_media_error(Some("boom"), None),
            DEFAULT_MEDIA_SEND_ERROR
        );
        assert_eq!(
            resolve_user_facing_media_error(Some("too big"), Some(OUTBOUND_ERROR_FILE_TOO_LARGE)),
            "too big"
        );
        assert_eq!(
            resolve_user_facing_media_error(Some("daily"), Some(OUTBOUND_ERROR_UPLOAD_DAILY_LIMIT)),
            "daily"
        );
        assert!(format_daily_limit_message("/data", "f.zip", "1.2G").contains("`/data/f.zip`"));
        assert!(format_too_large_message("视频", "120MB", 100).contains("100M"));
        assert!(should_send_media_failure_fallback(false, 0));
        assert!(!should_send_media_failure_fallback(true, 0));
        assert!(!should_send_media_failure_fallback(false, 1));
    }

    // ---- TTS / config ------------------------------------------------------

    #[test]
    fn account_tts_deep_merge() {
        let cfg = QqBotExtensionConfig {
            tts: Some(json!({"provider": "openai", "providers": {"openai": {"voice": "alloy", "speed": 1}}})),
            accounts: HashMap::from([(
                "work".to_string(),
                json!({"tts": {"providers": {"openai": {"voice": "verse"}}}}),
            )]),
            ..Default::default()
        };
        let base = json!({"enabled": true, "maxTextLength": 800});
        let tts = resolve_qqbot_account_tts(Some(&base), &cfg, "work");
        assert_eq!(tts["enabled"], true);
        assert_eq!(tts["provider"], "openai");
        assert_eq!(tts["providers"]["openai"]["voice"], "verse"); // account wins
        assert_eq!(tts["providers"]["openai"]["speed"], 1); // channel preserved
        assert_eq!(tts["maxTextLength"], 800);
        // Unknown account → channel TTS only.
        let tts = resolve_qqbot_account_tts(Some(&base), &cfg, "nope");
        assert_eq!(tts["providers"]["openai"]["voice"], "alloy");
        // Shallow account config spread replaces scalars.
        let merged = resolve_account_config(&json!({"appId": "1", "x": {"a": 1}}), &json!({"x": {"b": 2}}));
        assert_eq!(merged["appId"], "1");
        assert_eq!(merged["x"], json!({"b": 2})); // replaced wholesale, not deep-merged
    }

    #[test]
    fn extension_config_parsing() {
        let value = json!({
            "enabled": true,
            "appId": "102345",
            "defaultRequireMention": false,
            "streaming": {"mode": "partial", "c2cStreamApi": true},
            "groups": {"*": {"requireMention": true}},
            "accounts": {"work": {"appId": "999"}}
        });
        let cfg = QqBotExtensionConfig::from_extensions_value(Some(&value));
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.default_require_mention, Some(false));
        assert!(cfg.groups.contains_key("*"));
        assert!(should_use_official_c2c_stream("c2c", cfg.streaming.as_ref()));
        assert!(QqBotExtensionConfig::from_extensions_value(None).app_id.is_none());
    }
}
