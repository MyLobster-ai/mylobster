use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use tracing::{info, warn};

// ============================================================================
// Feishu / Lark Channel Implementation
// ============================================================================

/// Feishu (Lark) channel integration via the Feishu Open Platform API.
///
/// Feishu is the enterprise collaboration platform by ByteDance (known as
/// Lark internationally). This channel communicates via the Feishu Bot API
/// to send and receive messages.
///
/// API docs: <https://open.feishu.cn/document/server-docs/im-v1/message/create>
///
/// Authentication uses an app_id + app_secret to obtain a `tenant_access_token`
/// via `POST https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal`.
pub struct FeishuChannel {
    /// Feishu app ID from the Feishu Open Platform developer console.
    app_id: Option<String>,
    /// Feishu app secret.
    app_secret: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

/// Feishu Drive comment event types (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeishuDriveEvent {
    CommentCreated {
        comment_id: String,
        doc_token: String,
        content: String,
        reply_to: Option<String>,
    },
    CommentReplied {
        comment_id: String,
        parent_id: String,
        doc_token: String,
        content: String,
    },
}

/// Feishu API base URL.
const FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";

impl FeishuChannel {
    pub fn new() -> Self {
        Self {
            app_id: None,
            app_secret: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Feishu channel.
    pub fn with_config(app_id: String, app_secret: String) -> Self {
        Self {
            app_id: Some(app_id),
            app_secret: Some(app_secret),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Acquire a tenant_access_token from the Feishu Open Platform.
    async fn acquire_tenant_token(&self) -> Result<String> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_id not configured"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_secret not configured"))?;

        let url = format!(
            "{}/auth/v3/tenant_access_token/internal",
            FEISHU_API_BASE,
        );

        let body = serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret,
        });

        let resp = self.client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Feishu tenant_access_token request failed ({}): {}",
                status,
                text
            );
        }

        let result: serde_json::Value = resp.json().await?;
        let code = result["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = result["msg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Feishu token error (code {}): {}", code, msg);
        }

        let token = result["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Feishu: no tenant_access_token in response"))?
            .to_string();

        Ok(token)
    }
}

#[async_trait]
impl ChannelPlugin for FeishuChannel {
    fn id(&self) -> &str {
        "feishu"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Feishu".to_string(),
            description: "Feishu (Lark) channel via Open Platform API".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.app_id.is_none() || self.app_secret.is_none() {
            warn!("Feishu channel enabled but app_id or app_secret not configured");
            return Ok(());
        }

        info!("Feishu channel starting");

        // Verify credentials by acquiring an initial token.
        match self.acquire_tenant_token().await {
            Ok(_) => info!("Feishu: tenant_access_token acquired successfully"),
            Err(e) => warn!("Feishu: failed to acquire initial token: {}", e),
        }

        // TODO: Register an event subscription endpoint to receive incoming
        // messages. Feishu sends events via HTTP POST to the app's event URL.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Feishu channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self.acquire_tenant_token().await?;

        // `to` is a Feishu chat_id (group) or open_id (user).
        // We default to sending to a chat_id. The receive_id_type determines
        // whether `to` is a chat_id, open_id, user_id, or union_id.
        let url = format!(
            "{}/im/v1/messages?receive_id_type=chat_id",
            FEISHU_API_BASE,
        );

        let body = serde_json::json!({
            "receive_id": to,
            "msg_type": "text",
            "content": serde_json::json!({ "text": message }).to_string(),
        });

        info!(chat_id = %to, "Feishu: sending message");

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Feishu send message failed ({}): {}", status, text);
        }

        // Check the Feishu API-level error code.
        let result: serde_json::Value = resp.json().await?;
        let code = result["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = result["msg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Feishu send error (code {}): {}", code, msg);
        }

        Ok(())
    }
}

// ============================================================================
// Extension config (`channels.feishu`, read from
// `config.channels.extensions["feishu"]`; upstream
// `extensions/feishu/src/config-schema.ts` @ v2026.7.1)
// ============================================================================

/// Subset of `channels.feishu` config relevant to the ported behavior.
///
/// Full upstream schema is much larger; unknown keys are ignored on
/// deserialization (serde default behavior for missing, extras dropped).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FeishuExtensionConfig {
    pub enabled: Option<bool>,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    /// `feishu` | `lark` | custom `https://...` domain (default `feishu`).
    pub domain: Option<String>,
    /// Emit reply "blocks" as independent messages (default false).
    pub block_streaming: Option<bool>,
    /// `disabled` | `enabled` — native topic-thread replies.
    pub reply_in_thread: Option<String>,
    /// Streaming cards enabled.
    pub streaming: Option<bool>,
    /// `auto` | `raw` | `card`.
    pub render_mode: Option<String>,
    pub require_mention: Option<bool>,
    pub text_chunk_limit: Option<usize>,
    /// Channel-level TTS override (deep-merged over `messages.tts`).
    pub tts: Option<Value>,
    /// Per-account raw config values (kept raw for TTS deep-merge).
    pub accounts: HashMap<String, Value>,
    pub default_account: Option<String>,
}

impl FeishuExtensionConfig {
    /// Parse from the flattened extensions map value (missing → default).
    pub fn from_extensions_value(value: Option<&Value>) -> Self {
        value
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

/// Resolve the Feishu Open Platform API base URL from the `domain` config
/// (upstream `resolveApiBase` in `streaming-card.ts`).
pub fn resolve_feishu_api_base(domain: Option<&str>) -> String {
    match domain.map(str::trim) {
        Some("lark") => "https://open.larksuite.com/open-apis".to_string(),
        Some(d) if d.starts_with("http") => {
            format!("{}/open-apis", d.trim_end_matches('/'))
        }
        _ => "https://open.feishu.cn/open-apis".to_string(),
    }
}

// ============================================================================
// TTS deep-merge (upstream `src/tts/tts-config.ts::deepMergeDefined` +
// `resolveEffectiveTtsConfig` @ v2026.7.1). Shared with `channels::qqbot`.
// ============================================================================

/// Keys never copied during merge (prototype-pollution guard upstream).
const BLOCKED_MERGE_KEYS: [&str; 3] = ["__proto__", "prototype", "constructor"];

/// Recursive deep merge of `override` over `base`.
///
/// Semantics match upstream `deepMergeDefined`: plain objects merge key-wise
/// (recursing where both sides are objects); any other value in the override
/// replaces the base wholesale (arrays are replaced, not concatenated).
/// Upstream skips `undefined` override values — in JSON-config land absent
/// keys simply do not appear, so `null` counts as a defined override here.
pub fn deep_merge_defined(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                if BLOCKED_MERGE_KEYS.contains(&k.as_str()) {
                    continue;
                }
                let merged = match out.get(k) {
                    Some(existing) => deep_merge_defined(existing, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => over.clone(),
    }
}

/// Effective TTS config: `messages.tts` ← agent ← channel ← account, each
/// folded in with [`deep_merge_defined`] (upstream `resolveEffectiveTtsConfig`).
///
/// Account-level TTS overrides therefore deep-merge over channel-level TTS
/// rather than replacing it (the account *config* merge upstream is a shallow
/// spread, but effective TTS resolution re-reads both layers and deep-merges).
pub fn resolve_effective_tts_config(
    base: Option<&Value>,
    agent_override: Option<&Value>,
    channel_override: Option<&Value>,
    account_override: Option<&Value>,
) -> Value {
    let empty = json!({});
    let mut merged = base.cloned().unwrap_or_else(|| json!({}));
    for layer in [agent_override, channel_override, account_override] {
        merged = deep_merge_defined(&merged, layer.unwrap_or(&empty));
    }
    merged
}

// ============================================================================
// Streaming cards (upstream `extensions/feishu/src/streaming-card.ts` +
// `reply-dispatcher.ts` @ v2026.7.1)
// ============================================================================

/// Minimum interval between CardKit content updates.
pub const STREAMING_UPDATE_THROTTLE_MS: u64 = 160;
/// Push an update early once at least this many UTF-16 units were appended.
pub const STREAMING_SIGNIFICANT_DELTA_CHARS: usize = 18;
/// Overall streaming-card update timeout: if no delta was delivered for this
/// long the card is closed and delivery falls back to plain messages
/// (parity row "30s streaming-card timeout").
pub const STREAMING_CARD_TIMEOUT_MS: u64 = 30_000;
/// Per-account backoff after a streaming-card start failure.
pub const STREAMING_START_FAILURE_BACKOFF_MS: u64 = 60_000;
/// Card summary text shown while generating.
pub const STREAMING_CARD_PLACEHOLDER_SUMMARY: &str = "[Generating...]";

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Merge a streaming update into previously accumulated text (upstream
/// `mergeStreamingText`): handles restarts, subset/superset payloads and
/// partial suffix/prefix overlap so each CardKit update carries the full
/// merged text.
pub fn merge_streaming_text(previous: &str, next: &str) -> String {
    if next.is_empty() {
        return previous.to_string();
    }
    if previous.is_empty() || next == previous || next.starts_with(previous) {
        return next.to_string();
    }
    if previous.starts_with(next) {
        return previous.to_string();
    }
    if next.contains(previous) {
        return next.to_string();
    }
    if previous.contains(next) {
        return previous.to_string();
    }
    // Largest k where suffix(previous, k) == prefix(next, k).
    let prev_chars: Vec<char> = previous.chars().collect();
    let next_chars: Vec<char> = next.chars().collect();
    let max_overlap = prev_chars.len().min(next_chars.len());
    for overlap in (1..=max_overlap).rev() {
        if prev_chars[prev_chars.len() - overlap..] == next_chars[..overlap] {
            let rest: String = next_chars[overlap..].iter().collect();
            return format!("{previous}{rest}");
        }
    }
    format!("{previous}{next}")
}

/// True when the text ends on a natural sentence boundary (upstream
/// `hasNaturalStreamingBoundary`, CJK + ASCII enders).
pub fn has_natural_streaming_boundary(text: &str) -> bool {
    matches!(
        text.chars().last(),
        Some('\n' | '。' | '！' | '？' | '!' | '?' | '；' | ';' | '：' | ':')
    )
}

/// Whether a streaming update is worth pushing now (upstream
/// `shouldPushStreamingUpdate`).
pub fn should_push_streaming_update(prev: &str, next: &str) -> bool {
    if prev.is_empty() {
        return true;
    }
    if has_natural_streaming_boundary(next) {
        return true;
    }
    utf16_len(next).saturating_sub(utf16_len(prev)) >= STREAMING_SIGNIFICANT_DELTA_CHARS
}

/// Truncate a card summary to `max` UTF-16 units with a `...` tail
/// (upstream `truncateSummary`, default max 50).
pub fn truncate_summary(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    let flat = flat.trim();
    if utf16_len(flat) <= max {
        return flat.to_string();
    }
    let budget = max.saturating_sub(3);
    let mut used = 0usize;
    let mut out = String::new();
    for ch in flat.chars() {
        let w = ch.len_utf16();
        if used + w > budget {
            break;
        }
        used += w;
        out.push(ch);
    }
    format!("{out}...")
}

/// How the streaming card message is created (upstream
/// `resolveStreamingCardSendMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingCardSendMode {
    /// `im.message.reply` to a message (optionally in-thread).
    Reply,
    /// `im.message.create` with an injected `root_id`.
    RootCreate,
    /// Plain `im.message.create`.
    Create,
}

pub fn resolve_streaming_card_send_mode(
    reply_to_message_id: Option<&str>,
    root_id: Option<&str>,
) -> StreamingCardSendMode {
    if reply_to_message_id.map(str::trim).filter(|s| !s.is_empty()).is_some() {
        StreamingCardSendMode::Reply
    } else if root_id.map(str::trim).filter(|s| !s.is_empty()).is_some() {
        StreamingCardSendMode::RootCreate
    } else {
        StreamingCardSendMode::Create
    }
}

/// What the caller should do after offering a streaming delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingCardUpdate {
    /// Nothing to send (no visible change).
    Skip,
    /// Push this merged content now via CardKit.
    SendNow { content: String, sequence: u64, uuid: String },
    /// Too soon — flush after this many milliseconds.
    Throttled { delay_ms: u64 },
}

/// CardKit delivered-delta tracking for one streaming card
/// (upstream `FeishuStreamingCard` state).
///
/// Sequence numbers increment *before* each mutating request and pair with a
/// deterministic idempotency uuid (`s_`/`r_`/`n_`/`c_` prefixes). `sent_text`
/// only advances when the caller confirms delivery, so a failed update is
/// retried with the next merged payload.
#[derive(Debug, Clone)]
pub struct StreamingCardSession {
    pub card_id: String,
    sequence: u64,
    current_text: String,
    pending_text: String,
    sent_text: String,
    last_update_ms: u64,
    last_delivery_ms: u64,
}

impl StreamingCardSession {
    pub fn new(card_id: impl Into<String>, now_ms: u64) -> Self {
        Self {
            card_id: card_id.into(),
            sequence: 1,
            current_text: String::new(),
            pending_text: String::new(),
            sent_text: String::new(),
            last_update_ms: 0,
            last_delivery_ms: now_ms,
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// Idempotency uuid for a content update at sequence `seq`.
    fn uuid(&self, prefix: char, seq: u64) -> String {
        format!("{prefix}_{}_{}", self.card_id, seq)
    }

    /// Offer a new streaming delta. Returns what to do.
    pub fn offer(&mut self, now_ms: u64, text: &str) -> StreamingCardUpdate {
        let merged = merge_streaming_text(&self.pending_text, text);
        let merged = merge_streaming_text(&self.current_text, &merged);
        if merged == self.current_text || merged == self.sent_text {
            return StreamingCardUpdate::Skip;
        }
        self.pending_text = merged.clone();
        let elapsed = now_ms.saturating_sub(self.last_update_ms);
        if self.last_update_ms != 0 && elapsed < STREAMING_UPDATE_THROTTLE_MS {
            return StreamingCardUpdate::Throttled {
                delay_ms: STREAMING_UPDATE_THROTTLE_MS - elapsed,
            };
        }
        if !should_push_streaming_update(&self.current_text, &merged) {
            return StreamingCardUpdate::Skip;
        }
        self.last_update_ms = now_ms;
        self.current_text = merged.clone();
        let seq = self.next_sequence();
        StreamingCardUpdate::SendNow {
            content: merged,
            sequence: seq,
            uuid: self.uuid('s', seq),
        }
    }

    /// Confirm a CardKit update as delivered (advances `sent_text`).
    pub fn mark_delivered(&mut self, text: &str, now_ms: u64) {
        self.sent_text = text.to_string();
        self.last_delivery_ms = now_ms;
    }

    /// A failed update leaves `sent_text` untouched — the next merged payload
    /// retries the whole content (upstream returns `false` and skips the
    /// `sentText` advance).
    pub fn sent_text(&self) -> &str {
        &self.sent_text
    }

    /// True when no delta has been delivered for [`STREAMING_CARD_TIMEOUT_MS`].
    pub fn is_timed_out(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_delivery_ms) >= STREAMING_CARD_TIMEOUT_MS
    }

    /// Close plan: whether the final text can go through the cheap content
    /// update (`sent_text` is a prefix) or needs a full element replace.
    pub fn close(&mut self, final_text: Option<&str>) -> StreamingCardClose {
        let text = match final_text {
            Some(t) => t.to_string(),
            None => merge_streaming_text(&self.current_text, &self.pending_text),
        };
        let use_update = text.starts_with(&self.sent_text);
        let seq = self.next_sequence();
        let uuid = if use_update {
            self.uuid('s', seq)
        } else {
            self.uuid('r', seq)
        };
        let close_seq = self.sequence + 1;
        self.sequence = close_seq;
        StreamingCardClose {
            content: text.clone(),
            use_update,
            content_sequence: seq,
            content_uuid: uuid,
            settings_sequence: close_seq,
            settings_uuid: format!("c_{}_{}", self.card_id, close_seq),
            summary: truncate_summary(&text, 50),
        }
    }
}

/// Result of closing a streaming card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingCardClose {
    pub content: String,
    /// true → `PUT elements/content/content`; false → element replace.
    pub use_update: bool,
    pub content_sequence: u64,
    pub content_uuid: String,
    pub settings_sequence: u64,
    pub settings_uuid: String,
    pub summary: String,
}

/// Initial CardKit card JSON for a streaming card (upstream create body).
pub fn build_streaming_card_json(header_title: Option<&str>) -> Value {
    let mut card = json!({
        "schema": "2.0",
        "config": {
            "streaming_mode": true,
            "summary": { "content": STREAMING_CARD_PLACEHOLDER_SUMMARY },
            "streaming_config": {
                "print_frequency_ms": { "default": 50 },
                "print_step": { "default": 1 }
            }
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": "", "element_id": "content" }
            ]
        }
    });
    if let Some(title) = header_title {
        card["header"] = json!({
            "title": { "tag": "plain_text", "content": title },
            "template": "blue"
        });
    }
    card
}

// ============================================================================
// Card callbacks (upstream `monitor.account.ts::parseFeishuCardActionEventPayload`
// + `card-interaction.ts` @ v2026.7.1)
// ============================================================================

/// Normalized card action event identity, handling both Schema 1 (flat
/// `operator.user_id` string) and Schema 2 (nested
/// `operator.user_id.{open_id,user_id,union_id}` object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuCardActionEvent {
    pub token: String,
    pub open_id: String,
    pub user_id: Option<String>,
    pub union_id: Option<String>,
    pub open_message_id: Option<String>,
    pub chat_id: Option<String>,
    pub tag: String,
    pub value: Value,
}

fn first_string(candidates: &[&Value]) -> Option<String> {
    for v in candidates {
        if let Some(s) = v.as_str() {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Parse a card action callback payload; returns `None` unless `token`,
/// operator open_id, action tag and an object `action.value` are all present
/// (upstream rejects the event otherwise).
pub fn parse_feishu_card_action_event(payload: &Value) -> Option<FeishuCardActionEvent> {
    let event = payload.get("event").unwrap_or(payload);
    let operator = &event["operator"];
    let context = &event["context"];
    let action = &event["action"];
    let value = &action["value"];

    let token = first_string(&[&payload["token"], &event["token"]])?;
    // Schema 2 nests identity under operator.user_id as an object.
    let open_id = first_string(&[
        &operator["open_id"],
        &operator["user_id"]["open_id"],
        &value["open_id"],
        &context["open_id"],
    ])?;
    let user_id = first_string(&[
        &operator["user_id"],
        &operator["user_id"]["user_id"],
        &value["user_id"],
        &context["user_id"],
    ]);
    let union_id = first_string(&[&operator["union_id"], &operator["user_id"]["union_id"]]);
    // Prefer context.open_message_id: value.open_message_id may be a
    // temporary `card-action-c-*` id.
    let open_message_id = first_string(&[
        &context["open_message_id"],
        &value["open_message_id"],
        &event["open_message_id"],
    ]);
    let chat_id = first_string(&[&context["chat_id"], &context["open_chat_id"]]);
    let tag = first_string(&[&action["tag"]])?;
    if !value.is_object() {
        return None;
    }
    Some(FeishuCardActionEvent {
        token,
        open_id,
        user_id,
        union_id,
        open_message_id,
        chat_id,
        tag,
        value: value.clone(),
    })
}

/// Version tag on structured card-interaction envelopes.
pub const FEISHU_CARD_INTERACTION_VERSION: &str = "ocf1";
/// Card action token dedupe TTL.
pub const FEISHU_CARD_ACTION_TOKEN_TTL_MS: i64 = 15 * 60_000;

/// Decoded card interaction envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuCardInteraction {
    /// Pre-envelope button — fall back to plain text dispatch.
    Legacy { text: String },
    /// Envelope failed validation.
    Invalid { reason: &'static str },
    /// Valid `ocf1` envelope.
    Action {
        kind: String,
        action: String,
        quick: Option<String>,
        meta: Option<Value>,
        session_key: Option<String>,
    },
}

/// Decode `action.value` of a card callback (upstream
/// `decodeFeishuCardInteractionValue`). Constraint block `c` checks expiry
/// (`stale`), expected user (`wrong_user`) and chat (`wrong_conversation`).
pub fn decode_feishu_card_interaction(
    value: &Value,
    operator_open_id: &str,
    chat_id: Option<&str>,
    now_ms: i64,
) -> FeishuCardInteraction {
    if value["oc"].as_str() != Some(FEISHU_CARD_INTERACTION_VERSION) {
        let text = value["text"]
            .as_str()
            .or_else(|| value["command"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        return FeishuCardInteraction::Legacy { text };
    }
    let kind = match value["k"].as_str() {
        Some(k @ ("button" | "quick" | "meta")) => k.to_string(),
        _ => return FeishuCardInteraction::Invalid { reason: "malformed" },
    };
    let action = match value["a"].as_str().map(str::trim) {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => return FeishuCardInteraction::Invalid { reason: "malformed" },
    };
    let constraints = &value["c"];
    if constraints.is_object() {
        if let Some(expiry) = constraints["e"].as_i64() {
            if expiry < now_ms {
                return FeishuCardInteraction::Invalid { reason: "stale" };
            }
        }
        if let Some(expected_user) = constraints["u"].as_str() {
            if expected_user != operator_open_id {
                return FeishuCardInteraction::Invalid { reason: "wrong_user" };
            }
        }
        if let Some(expected_chat) = constraints["h"].as_str() {
            if chat_id != Some(expected_chat) {
                return FeishuCardInteraction::Invalid {
                    reason: "wrong_conversation",
                };
            }
        }
    }
    FeishuCardInteraction::Action {
        kind,
        action,
        quick: value["q"].as_str().map(str::to_string),
        meta: if value["m"].is_null() {
            None
        } else {
            Some(value["m"].clone())
        },
        session_key: constraints["s"].as_str().map(str::to_string),
    }
}

// ============================================================================
// Block streaming (upstream `reply-dispatcher.ts` blockStreaming @ v2026.7.1)
// ============================================================================

/// Default outbound chunk limit (upstream `resolveTextChunkLimit` fallback).
pub const FEISHU_TEXT_CHUNK_LIMIT: usize = 4000;

/// `channels.feishu.blockStreaming` — default **false** (blocks dropped).
pub fn resolve_block_streaming(config: &FeishuExtensionConfig) -> bool {
    config.block_streaming == Some(true)
}

/// Plan delivery of a streaming "block": when block streaming is enabled each
/// block is chunked and sent as independent `im.message` messages; when
/// disabled block chunks are dropped entirely (only the final reply is sent).
pub fn plan_block_delivery(
    block_text: &str,
    block_streaming_enabled: bool,
    chunk_limit: usize,
) -> Option<Vec<String>> {
    if !block_streaming_enabled {
        return None;
    }
    let limit = if chunk_limit == 0 {
        FEISHU_TEXT_CHUNK_LIMIT
    } else {
        chunk_limit
    };
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_units = 0usize;
    for line in block_text.split_inclusive('\n') {
        let line_units = utf16_len(line);
        if current_units + line_units > limit && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_units = 0;
        }
        if line_units > limit {
            // Oversized single line: hard split on UTF-16 budget.
            let mut used = 0usize;
            let mut piece = String::new();
            for ch in line.chars() {
                let w = ch.len_utf16();
                if used + w > limit && !piece.is_empty() {
                    chunks.push(std::mem::take(&mut piece));
                    used = 0;
                }
                used += w;
                piece.push(ch);
            }
            if !piece.is_empty() {
                current = piece;
                current_units = used;
            }
            continue;
        }
        current.push_str(line);
        current_units += line_units;
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    Some(chunks)
}

// ============================================================================
// Native card JSON detection (upstream `native-card.ts` @ v2026.7.1)
// ============================================================================

/// Header template color whitelist (upstream `FEISHU_CARD_TEMPLATES`).
pub const FEISHU_CARD_TEMPLATES: [&str; 13] = [
    "blue", "green", "red", "orange", "purple", "indigo", "wathet", "turquoise", "yellow", "grey",
    "carmine", "violet", "lime",
];

static CARD_PREFIX_SEPARATOR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s+\{").unwrap());

fn escape_card_markdown(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Detect outbound text that is a Feishu card JSON document and sanitize it
/// into a sendable card (upstream `readNativeFeishuCardJson`). Returns `None`
/// when the text is not card JSON (send as plain text instead).
pub fn read_native_feishu_card_json(text: &str, response_prefix: Option<&str>) -> Option<Value> {
    let mut trimmed = text.trim();
    if let Some(prefix) = response_prefix.filter(|p| !p.is_empty()) {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            // Only strip the prefix when a separator precedes the JSON.
            if CARD_PREFIX_SEPARATOR_RE.is_match(rest) {
                trimmed = rest.trim_start();
            }
        }
    }
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    if !parsed.is_object() {
        return None;
    }
    sanitize_native_feishu_card(&parsed)
}

fn sanitize_card_elements(elements: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for el in elements {
        match el["tag"].as_str() {
            Some("hr") => out.push(json!({ "tag": "hr" })),
            Some("markdown") => {
                if let Some(content) = el["content"].as_str() {
                    out.push(json!({
                        "tag": "markdown",
                        "content": escape_card_markdown(content)
                    }));
                }
            }
            Some("div") => {
                let text = &el["text"];
                match text["tag"].as_str() {
                    Some("lark_md") | Some("plain_text") => {
                        if let Some(content) = text["content"].as_str() {
                            out.push(json!({
                                "tag": "markdown",
                                "content": escape_card_markdown(content)
                            }));
                        }
                    }
                    _ => {}
                }
            }
            Some("button") => {
                if let Some(button) = sanitize_card_button(el) {
                    out.push(button);
                }
            }
            Some("action") => {
                if let Some(actions) = el["actions"].as_array() {
                    let buttons: Vec<Value> =
                        actions.iter().filter_map(sanitize_card_button).collect();
                    if !buttons.is_empty() {
                        out.push(json!({ "tag": "action", "actions": buttons }));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn sanitize_card_button(el: &Value) -> Option<Value> {
    let text = el["text"]["content"].as_str()?;
    let style = match el["type"].as_str() {
        Some("danger") => "danger",
        Some("primary") | Some("success") => "primary",
        _ => "default",
    };
    // `open_url` behavior: http(s) only. `callback` only for ocf1 envelopes.
    if let Some(url) = el["url"].as_str() {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return None;
        }
        return Some(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": text },
            "type": style,
            "behaviors": [{ "type": "open_url", "default_url": url }]
        }));
    }
    let value = &el["value"];
    if value["oc"].as_str() == Some(FEISHU_CARD_INTERACTION_VERSION) {
        return Some(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": text },
            "type": style,
            "behaviors": [{ "type": "callback", "value": value.clone() }]
        }));
    }
    None
}

fn sanitize_native_feishu_card(card: &Value) -> Option<Value> {
    // Unwrap `{type:"interactive", card:{...}}`.
    let card = if card["type"].as_str() == Some("interactive") && card["card"].is_object() {
        &card["card"]
    } else {
        card
    };
    let elements = card["body"]["elements"]
        .as_array()
        .or_else(|| card["elements"].as_array())?;
    let sanitized = sanitize_card_elements(elements);
    if sanitized.is_empty() {
        return None;
    }
    let mut out = json!({
        "schema": "2.0",
        "config": { "width_mode": "fill" },
        "body": { "elements": sanitized }
    });
    if let Some(title) = card["header"]["title"]["content"].as_str() {
        let template = card["header"]["template"]
            .as_str()
            .filter(|t| FEISHU_CARD_TEMPLATES.contains(t))
            .unwrap_or("blue");
        out["header"] = json!({
            "title": { "tag": "plain_text", "content": title },
            "template": template
        });
    }
    Some(out)
}

// ============================================================================
// No-visible-reply fallback (upstream `reply-dispatcher.ts` @ v2026.7.1)
// ============================================================================

pub const NO_VISIBLE_REPLY_FALLBACK_TEXT: &str = "⚠️ This reply completed without visible content. The turn may have been interrupted; please retry or ask me to recover from recent context.";

/// Whether the fallback plain message should be sent after an accepted turn
/// (upstream `ensureNoVisibleReplyFallback`): only for dispatched turns with
/// no visible reply, and never when the turn was intentionally silent.
pub fn should_send_no_visible_reply_fallback(
    dispatched: bool,
    visible_reply_sent: bool,
    skipped_final_reason: Option<&str>,
) -> bool {
    dispatched && !visible_reply_sent && skipped_final_reason != Some("silent")
}

// ============================================================================
// Topic threading (upstream `thread-bindings.ts` + `reply-dispatcher.ts`)
// ============================================================================

/// `replyInThread` config resolution: an explicit per-turn thread reply always
/// wins; otherwise the `replyInThread` channel/group config decides.
pub fn resolve_effective_reply_in_thread(thread_reply_mode: bool, reply_in_thread_cfg: bool) -> bool {
    thread_reply_mode || reply_in_thread_cfg
}

/// When a threaded reply may fall back to a top-level reply (upstream
/// `allowTopLevelReplyFallback`).
pub fn allow_top_level_reply_fallback(
    reply_in_thread: bool,
    thread_reply_mode: bool,
    root_id: Option<&str>,
    send_reply_to_message_id: Option<&str>,
) -> bool {
    reply_in_thread
        && thread_reply_mode
        && root_id.is_some()
        && send_reply_to_message_id.is_some()
        && root_id != send_reply_to_message_id
}

/// A conversation → session thread binding record (upstream
/// `FeishuThreadBindingRecord`).
#[derive(Debug, Clone)]
pub struct ThreadBindingRecord {
    pub account_id: String,
    pub conversation_id: String,
    pub target_session_key: String,
    pub bound_at_ms: i64,
    pub last_activity_at_ms: i64,
}

impl ThreadBindingRecord {
    /// Expiry: `min(last_activity + idle, bound_at + max_age)`; a zero/absent
    /// component disables that bound (upstream expiry computation).
    pub fn expires_at(&self, idle_timeout_ms: Option<i64>, max_age_ms: Option<i64>) -> Option<i64> {
        let idle = idle_timeout_ms
            .filter(|v| *v > 0)
            .map(|v| self.last_activity_at_ms + v);
        let age = max_age_ms.filter(|v| *v > 0).map(|v| self.bound_at_ms + v);
        match (idle, age) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

// ============================================================================
// Mentions (upstream `mention.ts` + `bot-content.ts` @ v2026.7.1)
// ============================================================================

/// A mention entry on an inbound Feishu message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeishuMention {
    pub key: String,
    pub open_id: Option<String>,
    pub name: Option<String>,
}

/// `@all` broadcast mention detection (upstream `isFeishuBroadcastMention`).
pub fn is_feishu_broadcast_mention(mention: &FeishuMention) -> bool {
    let key = mention.key.to_lowercase();
    key == "@all" || key == "@_all" || mention.open_id.as_deref() == Some("all")
}

/// Fail-closed bot mention check (upstream `checkBotMentioned`): with no
/// known bot Open ID this returns **false** — the message is treated as not
/// mentioning the bot rather than guessing.
pub fn check_bot_mentioned(mentions: &[FeishuMention], bot_open_id: Option<&str>) -> bool {
    let bot = match bot_open_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => b,
        None => return false,
    };
    mentions
        .iter()
        .any(|m| !is_feishu_broadcast_mention(m) && m.open_id.as_deref() == Some(bot))
}

/// Fail-closed mention-forward request check (upstream
/// `isMentionForwardRequest`): DMs forward when any non-bot user is
/// mentioned; groups require BOTH a bot mention and another user mention.
/// With no bot Open ID this always returns false.
pub fn is_mention_forward_request(
    mentions: &[FeishuMention],
    bot_open_id: Option<&str>,
    is_group: bool,
) -> bool {
    if mentions.is_empty() {
        return false;
    }
    let bot = match bot_open_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => b,
        None => return false, // fail closed
    };
    let non_bot_user_mentioned = mentions.iter().any(|m| {
        !is_feishu_broadcast_mention(m)
            && m.open_id.as_deref().is_some_and(|id| id != bot && !id.is_empty())
    });
    if !is_group {
        return non_bot_user_mentioned;
    }
    check_bot_mentioned(mentions, Some(bot)) && non_bot_user_mentioned
}

/// Card mention markup for an Open ID (upstream `<at id=X></at>`).
pub fn format_card_mention(open_id: &str) -> String {
    format!("<at id={open_id}></at>")
}

// ============================================================================
// Rate-limit retry (upstream `comment-shared.ts::requestFeishuApi` +
// `typing.ts` @ v2026.7.1)
// ============================================================================

/// API-body codes retried on message sends (230020 = per-chat limit,
/// 11232 = tenant 100/min & 5/sec limit).
pub const FEISHU_SEND_RATE_LIMIT_CODES: [i64; 2] = [230020, 11232];
/// Max retries after the initial attempt (3 attempts total).
pub const FEISHU_SEND_MAX_RETRIES: u32 = 2;
/// Linear backoff base.
pub const FEISHU_SEND_RETRY_BASE_MS: u64 = 500;
/// Reaction/typing API codes that trip the keepalive breaker
/// (99991400 = RPS limit, 99991403 = monthly quota).
pub const FEISHU_BACKOFF_CODES: [i64; 3] = [99991400, 99991403, 429];

/// Classify a send failure (HTTP status and/or Feishu body code) for retry.
///
/// HTTP 429 always retries and takes priority; body codes 230020/11232 retry;
/// 230006 is recognized but explicitly **not** retryable (upstream removed it
/// from the retry set), as is everything else.
pub fn feishu_send_rate_limit_code(http_status: Option<u16>, api_code: Option<i64>) -> Option<i64> {
    if http_status == Some(429) {
        return Some(429);
    }
    api_code.filter(|c| FEISHU_SEND_RATE_LIMIT_CODES.contains(c))
}

/// Linear retry delay before attempt `attempt` (1-based): 500ms, 1000ms, ...
pub fn feishu_send_retry_delay_ms(attempt: u32) -> u64 {
    u64::from(attempt) * FEISHU_SEND_RETRY_BASE_MS
}

/// Typing/reaction backoff-code classifier (upstream `FeishuBackoffError`).
pub fn is_feishu_backoff_code(code: i64) -> bool {
    FEISHU_BACKOFF_CODES.contains(&code)
}

// ============================================================================
// Redelivery dedupe (upstream `dedupe-key.ts` + `dedup.ts` @ v2026.7.1)
// ============================================================================

pub const FEISHU_DEDUP_TTL_MS: i64 = 24 * 60 * 60 * 1000;
pub const FEISHU_DEDUP_MEMORY_MAX: usize = 1000;
pub const FEISHU_DEDUP_STORE_MAX_ENTRIES: usize = 10_000;

static CREATE_TIME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d+$").unwrap());

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn collect_media_keys(content: &Value, parts: &mut Vec<String>) {
    match content {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "image_key" || k == "file_key" {
                    if let Some(s) = v.as_str() {
                        parts.push(format!("{k}:{s}"));
                    }
                }
                collect_media_keys(v, parts);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_media_keys(item, parts);
            }
        }
        _ => {}
    }
}

/// Dedupe key for a redelivered Feishu message (upstream
/// `resolveFeishuMessageDedupeKey`).
///
/// Media messages key on `message_id` + media keys. Text messages use a
/// *retry identity* — sender + chat + create_time + content hash — because
/// Feishu redelivers the same logical text under a fresh `message_id`; when
/// any identity field is missing the plain `message_id` is used.
pub fn resolve_feishu_message_dedupe_key(
    message_id: &str,
    message_type: &str,
    content_json: &str,
    sender_id: Option<&str>,
    chat_id: Option<&str>,
    create_time: Option<&str>,
) -> Option<String> {
    let message_id = message_id.trim();
    if message_id.is_empty() {
        return None;
    }
    let is_media = matches!(
        message_type,
        "post" | "image" | "file" | "audio" | "sticker" | "video" | "media"
    );
    if is_media {
        let mut parts: Vec<String> = vec![message_id.to_string()];
        if let Ok(content) = serde_json::from_str::<Value>(content_json) {
            let mut media = Vec::new();
            collect_media_keys(&content, &mut media);
            parts.extend(media);
        }
        return Some(json!(parts).to_string());
    }
    // Text retry identity.
    let (sender, chat, created) = match (sender_id, chat_id, create_time) {
        (Some(s), Some(c), Some(t))
            if !s.is_empty() && !c.is_empty() && CREATE_TIME_RE.is_match(t) =>
        {
            (s, c, t)
        }
        _ => return Some(message_id.to_string()),
    };
    let content_hash: String = sha256_hex(content_json).chars().take(32).collect();
    Some(json!(["text-retry", sender, chat, created, content_hash]).to_string())
}

// ============================================================================
// Self-echo drop (upstream `monitor.message-handler.ts` @ v2026.7.1)
// ============================================================================

/// True when an inbound message was authored by the bot itself and must be
/// dropped before consuming a dedupe claim or debounce slot.
pub fn is_feishu_self_echo(bot_open_id: Option<&str>, sender_open_id: Option<&str>) -> bool {
    match (
        bot_open_id.map(str::trim).filter(|s| !s.is_empty()),
        sender_open_id.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(bot), Some(sender)) => bot == sender,
        _ => false,
    }
}

// ============================================================================
// Bitable (upstream `bitable.ts` + `accounts.ts` tools merge @ v2026.7.1)
// ============================================================================

/// `bitable`/`base` alias gating (upstream tools-config merge): explicit
/// `bitable=false`, or `base=false` with `bitable` unset, disables the tools;
/// `bitable` unset inherits `base`; default enabled.
pub fn resolve_bitable_enabled(tools: &Value) -> bool {
    let bitable = tools.get("bitable").and_then(Value::as_bool);
    let base = tools.get("base").and_then(Value::as_bool);
    match (bitable, base) {
        (Some(false), _) => false,
        (None, Some(b)) => b,
        (Some(true), _) => true,
        (None, None) => true,
    }
}

/// Non-empty write schema validation (#94547 semantics): bitable record
/// writes must carry a non-empty `fields` object.
pub fn validate_bitable_write_fields(fields: &Value) -> std::result::Result<(), String> {
    match fields {
        Value::Object(map) if !map.is_empty() => Ok(()),
        Value::Object(_) => Err("bitable write requires a non-empty fields object".to_string()),
        _ => Err("bitable write fields must be an object".to_string()),
    }
}

/// Bitable list-records page-size clamp (upstream max 500, default 100).
pub fn clamp_bitable_page_size(requested: Option<i64>) -> u32 {
    match requested {
        Some(v) if v >= 1 && v <= 500 => v as u32,
        Some(v) if v > 500 => 500,
        _ => 100,
    }
}

// ============================================================================
// Wiki / drive pagination (upstream `wiki.ts` / `drive.ts` @ v2026.7.1)
// ============================================================================

pub const WIKI_PAGE_SIZE: u32 = 50;

/// Wiki page-size clamp: 1..=50, default 50 (upstream error text: "page_size
/// must be a positive integer between 1 and 50").
pub fn clamp_wiki_page_size(requested: Option<i64>) -> std::result::Result<u32, String> {
    match requested {
        None => Ok(WIKI_PAGE_SIZE),
        Some(v) if (1..=50).contains(&v) => Ok(v as u32),
        Some(_) => Err("page_size must be a positive integer between 1 and 50".to_string()),
    }
}

/// Drive folder-list page-size clamp: 1..=200.
pub fn clamp_drive_page_size(requested: Option<i64>) -> std::result::Result<u32, String> {
    match requested {
        None => Ok(200),
        Some(v) if (1..=200).contains(&v) => Ok(v as u32),
        Some(_) => Err("page_size must be a positive integer between 1 and 200".to_string()),
    }
}

/// Drive comment page-size clamp: 1..=100.
pub fn clamp_comment_page_size(requested: Option<i64>) -> u32 {
    match requested {
        Some(v) if (1..=100).contains(&v) => v as u32,
        _ => 100,
    }
}

/// A single page cursor returned to the caller (upstream helpers are
/// single-page / caller-driven — no internal loop).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeishuPageCursor {
    pub has_more: bool,
    pub page_token: Option<String>,
}

impl FeishuPageCursor {
    pub fn from_response(resp: &Value) -> Self {
        let token = resp["page_token"]
            .as_str()
            .or_else(|| resp["next_page_token"].as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        Self {
            has_more: resp["has_more"].as_bool().unwrap_or(false) && token.is_some(),
            page_token: token,
        }
    }

    /// Whether the root drive folder (`""` or `"0"`) is being listed — the
    /// cursor must not be forwarded there (only valid on concrete folders).
    pub fn forwardable_for_folder(folder_token: &str) -> bool {
        !(folder_token.is_empty() || folder_token == "0")
    }
}

// ============================================================================
// CJK filename recovery (upstream `media.ts` @ v2026.7.1)
// ============================================================================

static EAST_ASIAN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}]").unwrap());

/// Recover a UTF-8 filename that was mis-decoded as Latin-1 in a
/// Content-Disposition header (upstream `recoverUtf8FileNameFromLatin1Header`).
///
/// Re-encodes the string's code points ≤ 0xFF as bytes and re-decodes them as
/// UTF-8; the recovered form is used only when it differs, decodes cleanly
/// (no U+FFFD) and contains an East Asian script character.
pub fn recover_utf8_filename_from_latin1(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    for ch in value.chars() {
        let cp = ch as u32;
        if cp > 0xFF {
            return value.to_string(); // not a Latin-1 mojibake candidate
        }
        bytes.push(cp as u8);
    }
    match String::from_utf8(bytes) {
        Ok(recovered)
            if recovered != value
                && !recovered.contains('\u{FFFD}')
                && EAST_ASIAN_RE.is_match(&recovered) =>
        {
            recovered
        }
        _ => value.to_string(),
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Decode a filename from a Content-Disposition header value (upstream
/// `decodeDispositionFileName`): RFC 5987 `filename*=UTF-8''…` first, then
/// plain `filename="…"` with Latin-1 → UTF-8 recovery.
pub fn decode_disposition_filename(header: &str) -> Option<String> {
    static EXT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)filename\*=UTF-8''([^;]+)"#).unwrap());
    static PLAIN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)filename="?([^";]+)"?"#).unwrap());
    if let Some(caps) = EXT_RE.captures(header) {
        if let Some(decoded) = percent_decode(caps[1].trim().trim_matches('"')) {
            return Some(decoded);
        }
    }
    PLAIN_RE
        .captures(header)
        .map(|caps| recover_utf8_filename_from_latin1(caps[1].trim()))
}

/// Sanitize a filename for upload (upstream `sanitizeFileNameForUpload`):
/// replaces control chars, `"` and `\` with `_`, preserving the UTF-8
/// display name (percent-encoding here was a v2026.3.2 regression).
pub fn sanitize_filename_for_upload(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() || c == '"' || c == '\\' { '_' } else { c })
        .collect()
}

// ============================================================================
// Voice replies (upstream `media.ts` voice pipeline @ v2026.7.1)
// ============================================================================

pub const FEISHU_VOICE_SAMPLE_RATE_HZ: u32 = 48_000;
pub const FEISHU_VOICE_BITRATE: &str = "64k";
pub const FEISHU_VOICE_PROBE_TIMEOUT_MS: u64 = 5_000;

/// Audio containers ffmpeg can transcode to Feishu native opus voice.
pub const FEISHU_TRANSCODABLE_AUDIO_EXTS: [&str; 12] = [
    ".aac", ".aiff", ".alac", ".amr", ".caf", ".flac", ".m4a", ".mp3", ".oga", ".wav", ".webm",
    ".wma",
];

fn file_ext_lower(name: &str) -> Option<String> {
    name.rfind('.').map(|i| name[i..].to_lowercase())
}

/// Native Feishu voice detection (`.opus`/`.ogg` or `audio/ogg`/`audio/opus`).
pub fn is_feishu_native_voice_audio(file_name: Option<&str>, content_type: Option<&str>) -> bool {
    if let Some(ext) = file_name.and_then(file_ext_lower) {
        if ext == ".opus" || ext == ".ogg" {
            return true;
        }
    }
    matches!(
        content_type.map(|c| c.to_lowercase()),
        Some(ref c) if c.starts_with("audio/ogg") || c.starts_with("audio/opus")
    )
}

/// Whether the input is worth handing to ffmpeg for voice transcode.
pub fn is_likely_transcodable_audio(file_name: Option<&str>, content_type: Option<&str>) -> bool {
    if let Some(ext) = file_name.and_then(file_ext_lower) {
        if FEISHU_TRANSCODABLE_AUDIO_EXTS.contains(&ext.as_str()) {
            return true;
        }
    }
    content_type
        .map(|c| c.to_lowercase().starts_with("audio/"))
        .unwrap_or(false)
}

/// Outcome of the voice routing decision (upstream `prepareFeishuVoiceMedia`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeishuVoiceDecision {
    /// Already native opus/ogg voice — upload as `audio` untouched.
    PassThrough,
    /// Transcode to 48kHz mono libopus Ogg via **external ffmpeg**
    /// (integration point: this port shells out, args from
    /// [`feishu_voice_transcode_args`]); on ffmpeg failure degrade to file.
    Transcode,
    /// Send as a plain file attachment.
    SendAsFile,
}

/// Decide voice handling: native voice passes through; otherwise transcode
/// only when `audioAsVoice` is requested and the input looks transcodable.
pub fn resolve_feishu_voice_decision(
    audio_as_voice: bool,
    file_name: Option<&str>,
    content_type: Option<&str>,
) -> FeishuVoiceDecision {
    if is_feishu_native_voice_audio(file_name, content_type) {
        return FeishuVoiceDecision::PassThrough;
    }
    if audio_as_voice && is_likely_transcodable_audio(file_name, content_type) {
        return FeishuVoiceDecision::Transcode;
    }
    FeishuVoiceDecision::SendAsFile
}

/// ffmpeg argv for the Feishu voice transcode (upstream
/// `transcodeToFeishuVoiceOpus`). Requires an external `ffmpeg` binary; the
/// duration probe additionally needs `ffprobe` (`-show_entries
/// format=duration`, 5s timeout).
pub fn feishu_voice_transcode_args(input: &str, output: &str, max_duration_secs: u32) -> Vec<String> {
    [
        "-hide_banner", "-loglevel", "error", "-y", "-i", input, "-vn", "-sn", "-dn", "-t",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        max_duration_secs.to_string(),
        "-ar".into(),
        FEISHU_VOICE_SAMPLE_RATE_HZ.to_string(),
        "-ac".into(),
        "1".into(),
        "-c:a".into(),
        "libopus".into(),
        "-b:a".into(),
        FEISHU_VOICE_BITRATE.into(),
        "-f".into(),
        "ogg".into(),
        output.to_string(),
    ])
    .collect()
}

/// Convert an ffprobe duration (seconds) to the upload `duration` millis
/// (upstream `probeMediaDurationMs` post-processing): non-finite or ≤ 0 →
/// `None`; else `max(1, round(seconds*1000))`.
pub fn voice_duration_ms_from_seconds(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(((seconds * 1000.0).round() as u64).max(1))
}

// ============================================================================
// Per-chat sequential queue (upstream `sequential-queue.ts` +
// `sequential-key.ts` @ v2026.7.1)
// ============================================================================

/// Default per-task timeout (upstream `taskTimeoutMs` default 5 min).
pub const FEISHU_SEQUENTIAL_TASK_TIMEOUT_MS: u64 = 5 * 60_000;

/// Parallel lanes per chat (abort-control and btw-requests bypass the main
/// lane; upstream `getFeishuSequentialKey` suffixes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeishuSequentialLane {
    Main,
    Control,
    Btw,
}

/// Sequential-queue key: `feishu:{account}:{chat|unknown}` with an optional
/// lane suffix.
pub fn feishu_sequential_key(
    account_id: &str,
    chat_id: Option<&str>,
    lane: FeishuSequentialLane,
) -> String {
    let chat = chat_id.filter(|c| !c.is_empty()).unwrap_or("unknown");
    let base = format!("feishu:{account_id}:{chat}");
    match lane {
        FeishuSequentialLane::Main => base,
        FeishuSequentialLane::Control => format!("{base}:control"),
        FeishuSequentialLane::Btw => format!("{base}:btw"),
    }
}

/// Per-key FIFO bookkeeping with self-cleaning eviction: tasks on the same
/// key run in order, different keys are independent, and a key's entry is
/// evicted as soon as its chain drains (upstream deletes the map entry when
/// the tail promise settles). A timed-out task unblocks the chain but is not
/// aborted (checked via [`Self::task_timed_out`]).
#[derive(Debug, Default)]
pub struct FeishuSequentialQueue {
    queues: HashMap<String, VecDeque<u64>>,
    next_id: u64,
}

impl FeishuSequentialQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a task; returns `(task_id, position)` (0 = runnable now).
    pub fn enqueue(&mut self, key: &str) -> (u64, usize) {
        self.next_id += 1;
        let id = self.next_id;
        let queue = self.queues.entry(key.to_string()).or_default();
        queue.push_back(id);
        (id, queue.len() - 1)
    }

    /// The task currently allowed to run on `key`.
    pub fn head(&self, key: &str) -> Option<u64> {
        self.queues.get(key).and_then(|q| q.front().copied())
    }

    /// Complete (or time out) a task; evicts the key once its chain drains.
    /// Returns the next runnable task on that key, if any.
    pub fn complete(&mut self, key: &str, task_id: u64) -> Option<u64> {
        let queue = self.queues.get_mut(key)?;
        queue.retain(|id| *id != task_id);
        if queue.is_empty() {
            self.queues.remove(key);
            return None;
        }
        queue.front().copied()
    }

    /// Whether the key has been evicted (no pending chain).
    pub fn is_evicted(&self, key: &str) -> bool {
        !self.queues.contains_key(key)
    }

    pub fn pending(&self, key: &str) -> usize {
        self.queues.get(key).map(VecDeque::len).unwrap_or(0)
    }

    /// Bounded-run timeout check: `timeout_ms == 0` disables the bound.
    pub fn task_timed_out(started_ms: u64, now_ms: u64, timeout_ms: u64) -> bool {
        timeout_ms > 0 && now_ms.saturating_sub(started_ms) >= timeout_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TTS deep merge -------------------------------------------------

    #[test]
    fn deep_merge_recurses_objects_and_replaces_scalars() {
        let base = json!({"provider": "openai", "providers": {"openai": {"voice": "a", "rate": 1}}});
        let over = json!({"providers": {"openai": {"voice": "b"}}, "auto": "always"});
        let merged = deep_merge_defined(&base, &over);
        assert_eq!(merged["provider"], "openai");
        assert_eq!(merged["providers"]["openai"]["voice"], "b");
        assert_eq!(merged["providers"]["openai"]["rate"], 1);
        assert_eq!(merged["auto"], "always");
    }

    #[test]
    fn deep_merge_replaces_arrays_and_blocks_proto_keys() {
        let base = json!({"list": [1, 2, 3]});
        let over = json!({"list": [9], "__proto__": {"polluted": true}});
        let merged = deep_merge_defined(&base, &over);
        assert_eq!(merged["list"], json!([9]));
        assert!(merged.get("__proto__").is_none());
    }

    #[test]
    fn effective_tts_folds_account_over_channel() {
        let base = json!({"enabled": true, "provider": "edge", "maxTextLength": 500});
        let channel = json!({"provider": "openai", "providers": {"openai": {"voice": "x"}}});
        let account = json!({"providers": {"openai": {"voice": "y"}}});
        let out = resolve_effective_tts_config(Some(&base), None, Some(&channel), Some(&account));
        assert_eq!(out["enabled"], true);
        assert_eq!(out["provider"], "openai");
        assert_eq!(out["providers"]["openai"]["voice"], "y");
        assert_eq!(out["maxTextLength"], 500);
    }

    // ---- Streaming cards ------------------------------------------------

    #[test]
    fn merge_streaming_text_handles_growth_subset_and_overlap() {
        assert_eq!(merge_streaming_text("", "abc"), "abc");
        assert_eq!(merge_streaming_text("abc", "abcdef"), "abcdef");
        assert_eq!(merge_streaming_text("abcdef", "abc"), "abcdef");
        // Partial overlap: suffix of prev == prefix of next.
        assert_eq!(merge_streaming_text("hello wor", "world!"), "hello world!");
        // Disjoint → concatenated.
        assert_eq!(merge_streaming_text("foo", "bar"), "foobar");
        // next contains prev.
        assert_eq!(merge_streaming_text("bc", "abcd"), "abcd");
    }

    #[test]
    fn streaming_update_gating() {
        assert!(should_push_streaming_update("", "x"));
        assert!(should_push_streaming_update("abc", "abcd。"));
        assert!(!should_push_streaming_update("abc", "abcd"));
        assert!(should_push_streaming_update("a", &"a".repeat(19)));
        assert!(has_natural_streaming_boundary("done?\n"));
        assert!(has_natural_streaming_boundary("好了。"));
        assert!(!has_natural_streaming_boundary("still going"));
    }

    #[test]
    fn truncate_summary_flattens_and_caps() {
        assert_eq!(truncate_summary("short\ntext", 50), "short text");
        let long = "x".repeat(60);
        let out = truncate_summary(&long, 50);
        assert_eq!(out.chars().count(), 50);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn streaming_card_session_sequences_and_retry() {
        let mut s = StreamingCardSession::new("card1", 0);
        match s.offer(1_000, "hello。") {
            StreamingCardUpdate::SendNow { content, sequence, uuid } => {
                assert_eq!(content, "hello。");
                assert_eq!(sequence, 2); // starts at 1, incremented before send
                assert_eq!(uuid, "s_card1_2");
            }
            other => panic!("expected SendNow, got {other:?}"),
        }
        // Failed delivery: sent_text not advanced, next merged payload retries all.
        assert_eq!(s.sent_text(), "");
        // Too-soon update throttles.
        match s.offer(1_050, "hello。world。") {
            StreamingCardUpdate::Throttled { delay_ms } => assert_eq!(delay_ms, 110),
            other => panic!("expected Throttled, got {other:?}"),
        }
        s.mark_delivered("hello。", 1_100);
        assert!(!s.is_timed_out(1_200));
        assert!(s.is_timed_out(1_100 + STREAMING_CARD_TIMEOUT_MS));
        let close = s.close(Some("hello。world。"));
        assert!(close.use_update); // final text extends sent prefix
        assert!(close.settings_uuid.starts_with("c_card1_"));
        assert_eq!(close.summary, "hello。world。");
    }

    #[test]
    fn streaming_card_send_mode_precedence() {
        assert_eq!(
            resolve_streaming_card_send_mode(Some("om_x"), Some("om_root")),
            StreamingCardSendMode::Reply
        );
        assert_eq!(
            resolve_streaming_card_send_mode(None, Some("om_root")),
            StreamingCardSendMode::RootCreate
        );
        assert_eq!(resolve_streaming_card_send_mode(None, None), StreamingCardSendMode::Create);
    }

    // ---- Card callbacks -------------------------------------------------

    #[test]
    fn card_action_schema2_nested_operator_user_id() {
        let payload = json!({
            "event": {
                "token": "tok1",
                "operator": { "user_id": { "open_id": "ou_abc", "user_id": "u_1", "union_id": "un_1" } },
                "context": { "open_message_id": "om_1", "chat_id": "oc_1" },
                "action": { "tag": "button", "value": { "a": "go" } }
            }
        });
        let parsed = parse_feishu_card_action_event(&payload).expect("parses");
        assert_eq!(parsed.open_id, "ou_abc");
        assert_eq!(parsed.user_id.as_deref(), Some("u_1"));
        assert_eq!(parsed.union_id.as_deref(), Some("un_1"));
        assert_eq!(parsed.chat_id.as_deref(), Some("oc_1"));
        assert_eq!(parsed.open_message_id.as_deref(), Some("om_1"));
    }

    #[test]
    fn card_action_schema1_flat_operator() {
        let payload = json!({
            "token": "tok2",
            "event": {
                "operator": { "open_id": "ou_flat", "user_id": "u_flat" },
                "context": {},
                "action": { "tag": "button", "value": { "a": "x" } }
            }
        });
        let parsed = parse_feishu_card_action_event(&payload).expect("parses");
        assert_eq!(parsed.open_id, "ou_flat");
        assert_eq!(parsed.user_id.as_deref(), Some("u_flat"));
        // Missing open_id → reject.
        let bad = json!({"token": "t", "event": {"operator": {}, "action": {"tag": "button", "value": {}}}});
        assert!(parse_feishu_card_action_event(&bad).is_none());
    }

    #[test]
    fn card_interaction_envelope_validation() {
        let good = json!({"oc": "ocf1", "k": "button", "a": "approve", "c": {"u": "ou_1", "e": 10_000i64}});
        match decode_feishu_card_interaction(&good, "ou_1", None, 5_000) {
            FeishuCardInteraction::Action { action, kind, .. } => {
                assert_eq!(action, "approve");
                assert_eq!(kind, "button");
            }
            other => panic!("expected Action, got {other:?}"),
        }
        match decode_feishu_card_interaction(&good, "ou_1", None, 20_000) {
            FeishuCardInteraction::Invalid { reason } => assert_eq!(reason, "stale"),
            other => panic!("{other:?}"),
        }
        match decode_feishu_card_interaction(&good, "ou_other", None, 5_000) {
            FeishuCardInteraction::Invalid { reason } => assert_eq!(reason, "wrong_user"),
            other => panic!("{other:?}"),
        }
        let legacy = json!({"text": "hi"});
        match decode_feishu_card_interaction(&legacy, "ou_1", None, 0) {
            FeishuCardInteraction::Legacy { text } => assert_eq!(text, "hi"),
            other => panic!("{other:?}"),
        }
    }

    // ---- Block streaming + native cards ---------------------------------

    #[test]
    fn block_streaming_disabled_drops_blocks() {
        assert!(plan_block_delivery("hello", false, 4000).is_none());
        let chunks = plan_block_delivery("hello\nworld", true, 4000).unwrap();
        assert_eq!(chunks, vec!["hello\nworld".to_string()]);
        // Splits across the limit at line boundaries.
        let text = format!("{}\n{}", "a".repeat(30), "b".repeat(30));
        let chunks = plan_block_delivery(&text, true, 32).unwrap();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn native_card_json_detection() {
        let card = r#"{"header":{"title":{"content":"T"},"template":"green"},"elements":[{"tag":"markdown","content":"hi"},{"tag":"hr"}]}"#;
        let parsed = read_native_feishu_card_json(card, None).expect("card detected");
        assert_eq!(parsed["schema"], "2.0");
        assert_eq!(parsed["header"]["template"], "green");
        assert_eq!(parsed["body"]["elements"].as_array().unwrap().len(), 2);
        // Not JSON → None; JSON without supported elements → None.
        assert!(read_native_feishu_card_json("plain text", None).is_none());
        assert!(read_native_feishu_card_json(r#"{"elements":[]}"#, None).is_none());
        // Prefix stripped only with separator before '{'.
        let prefixed = format!("BOT: {card}");
        assert!(read_native_feishu_card_json(&prefixed, Some("BOT:")).is_some());
        // Invalid template falls back to blue.
        let bad_tpl = r#"{"header":{"title":{"content":"T"},"template":"neon"},"elements":[{"tag":"hr"}]}"#;
        let parsed = read_native_feishu_card_json(bad_tpl, None).unwrap();
        assert_eq!(parsed["header"]["template"], "blue");
    }

    // ---- Fallback / threading -------------------------------------------

    #[test]
    fn no_visible_reply_fallback_gating() {
        assert!(should_send_no_visible_reply_fallback(true, false, None));
        assert!(!should_send_no_visible_reply_fallback(true, true, None));
        assert!(!should_send_no_visible_reply_fallback(true, false, Some("silent")));
        assert!(!should_send_no_visible_reply_fallback(false, false, None));
    }

    #[test]
    fn reply_in_thread_resolution() {
        assert!(resolve_effective_reply_in_thread(true, false));
        assert!(resolve_effective_reply_in_thread(false, true));
        assert!(!resolve_effective_reply_in_thread(false, false));
        assert!(allow_top_level_reply_fallback(true, true, Some("om_r"), Some("om_m")));
        assert!(!allow_top_level_reply_fallback(true, true, Some("om_r"), Some("om_r")));
        assert!(!allow_top_level_reply_fallback(false, true, Some("om_r"), Some("om_m")));
    }

    #[test]
    fn thread_binding_expiry() {
        let rec = ThreadBindingRecord {
            account_id: "default".into(),
            conversation_id: "oc_1".into(),
            target_session_key: "s1".into(),
            bound_at_ms: 1_000,
            last_activity_at_ms: 5_000,
        };
        assert_eq!(rec.expires_at(Some(10_000), Some(100_000)), Some(15_000));
        assert_eq!(rec.expires_at(Some(10_000), Some(2_000)), Some(3_000));
        assert_eq!(rec.expires_at(None, None), None);
        assert_eq!(rec.expires_at(Some(0), None), None);
    }

    // ---- Mentions --------------------------------------------------------

    #[test]
    fn mention_checks_fail_closed_without_bot_open_id() {
        let mentions = vec![FeishuMention {
            key: "@_user_1".into(),
            open_id: Some("ou_x".into()),
            name: Some("X".into()),
        }];
        assert!(!check_bot_mentioned(&mentions, None));
        assert!(!check_bot_mentioned(&mentions, Some("  ")));
        assert!(!is_mention_forward_request(&mentions, None, false));
        // DM with a non-bot user mentioned forwards.
        assert!(is_mention_forward_request(&mentions, Some("ou_bot"), false));
        // Group needs both a bot mention and another user mention.
        assert!(!is_mention_forward_request(&mentions, Some("ou_bot"), true));
        let both = vec![
            FeishuMention { key: "@_user_1".into(), open_id: Some("ou_bot".into()), name: None },
            FeishuMention { key: "@_user_2".into(), open_id: Some("ou_x".into()), name: None },
        ];
        assert!(is_mention_forward_request(&both, Some("ou_bot"), true));
        // Broadcasts dropped.
        let all = vec![FeishuMention { key: "@all".into(), open_id: Some("all".into()), name: None }];
        assert!(!check_bot_mentioned(&all, Some("all")));
        assert_eq!(format_card_mention("ou_1"), "<at id=ou_1></at>");
    }

    // ---- Rate limiting ---------------------------------------------------

    #[test]
    fn rate_limit_classifier() {
        assert_eq!(feishu_send_rate_limit_code(Some(429), None), Some(429));
        assert_eq!(feishu_send_rate_limit_code(Some(429), Some(230006)), Some(429));
        assert_eq!(feishu_send_rate_limit_code(None, Some(230020)), Some(230020));
        assert_eq!(feishu_send_rate_limit_code(None, Some(11232)), Some(11232));
        // 230006 is recognized but NOT retryable.
        assert_eq!(feishu_send_rate_limit_code(None, Some(230006)), None);
        assert_eq!(feishu_send_rate_limit_code(Some(500), Some(0)), None);
        // Linear backoff 0/500/1000.
        assert_eq!(feishu_send_retry_delay_ms(0), 0);
        assert_eq!(feishu_send_retry_delay_ms(1), 500);
        assert_eq!(feishu_send_retry_delay_ms(2), 1000);
        assert!(is_feishu_backoff_code(99991400));
        assert!(is_feishu_backoff_code(429));
        assert!(!is_feishu_backoff_code(230020));
    }

    // ---- Dedupe / self-echo ----------------------------------------------

    #[test]
    fn dedupe_key_text_retry_identity() {
        let a = resolve_feishu_message_dedupe_key(
            "om_1",
            "text",
            r#"{"text":"hi"}"#,
            Some("ou_s"),
            Some("oc_c"),
            Some("1710000000000"),
        )
        .unwrap();
        let b = resolve_feishu_message_dedupe_key(
            "om_2", // redelivered under a fresh message_id
            "text",
            r#"{"text":"hi"}"#,
            Some("ou_s"),
            Some("oc_c"),
            Some("1710000000000"),
        )
        .unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with(r#"["text-retry""#));
        // Missing identity → falls back to message_id.
        let c = resolve_feishu_message_dedupe_key("om_3", "text", "{}", None, None, None).unwrap();
        assert_eq!(c, "om_3");
        // Non-numeric create_time → fallback.
        let d = resolve_feishu_message_dedupe_key(
            "om_4", "text", "{}", Some("s"), Some("c"), Some("not-a-number"),
        )
        .unwrap();
        assert_eq!(d, "om_4");
        // Empty message id → None.
        assert!(resolve_feishu_message_dedupe_key("  ", "text", "{}", None, None, None).is_none());
    }

    #[test]
    fn dedupe_key_media_includes_media_keys() {
        let key = resolve_feishu_message_dedupe_key(
            "om_5",
            "image",
            r#"{"image_key":"img_abc"}"#,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(key.contains("om_5"));
        assert!(key.contains("image_key:img_abc"));
    }

    #[test]
    fn self_echo_detection() {
        assert!(is_feishu_self_echo(Some("ou_bot"), Some("ou_bot")));
        assert!(!is_feishu_self_echo(Some("ou_bot"), Some("ou_user")));
        assert!(!is_feishu_self_echo(None, Some("ou_user")));
        assert!(!is_feishu_self_echo(Some(""), Some("")));
    }

    // ---- Bitable / pagination --------------------------------------------

    #[test]
    fn bitable_gating_and_write_schema() {
        assert!(resolve_bitable_enabled(&json!({})));
        assert!(!resolve_bitable_enabled(&json!({"bitable": false})));
        assert!(!resolve_bitable_enabled(&json!({"base": false})));
        assert!(resolve_bitable_enabled(&json!({"bitable": true, "base": false})));
        assert!(validate_bitable_write_fields(&json!({"Name": "x"})).is_ok());
        assert!(validate_bitable_write_fields(&json!({})).is_err());
        assert!(validate_bitable_write_fields(&json!("nope")).is_err());
    }

    #[test]
    fn pagination_clamps() {
        assert_eq!(clamp_wiki_page_size(None).unwrap(), 50);
        assert_eq!(clamp_wiki_page_size(Some(10)).unwrap(), 10);
        assert!(clamp_wiki_page_size(Some(51)).is_err());
        assert!(clamp_wiki_page_size(Some(0)).is_err());
        assert_eq!(clamp_drive_page_size(Some(200)).unwrap(), 200);
        assert!(clamp_drive_page_size(Some(500)).is_err());
        assert_eq!(clamp_comment_page_size(Some(1000)), 100);
        let cursor = FeishuPageCursor::from_response(&json!({"has_more": true, "page_token": "pt"}));
        assert!(cursor.has_more);
        assert_eq!(cursor.page_token.as_deref(), Some("pt"));
        assert!(!FeishuPageCursor::forwardable_for_folder(""));
        assert!(!FeishuPageCursor::forwardable_for_folder("0"));
        assert!(FeishuPageCursor::forwardable_for_folder("fld_x"));
    }

    // ---- CJK filename recovery -------------------------------------------

    #[test]
    fn cjk_filename_mojibake_recovery() {
        // UTF-8 bytes of "报告.pdf" mis-read as Latin-1.
        let original = "报告.pdf";
        let mojibake: String = original.bytes().map(|b| b as char).collect();
        assert_eq!(recover_utf8_filename_from_latin1(&mojibake), original);
        // Plain ASCII untouched.
        assert_eq!(recover_utf8_filename_from_latin1("report.pdf"), "report.pdf");
        // Already-CJK input (chars > 0xFF) untouched.
        assert_eq!(recover_utf8_filename_from_latin1("报告.pdf"), "报告.pdf");
    }

    #[test]
    fn disposition_filename_decoding_and_sanitize() {
        assert_eq!(
            decode_disposition_filename("attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf")
                .as_deref(),
            Some("报告.pdf")
        );
        assert_eq!(
            decode_disposition_filename(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
        assert_eq!(sanitize_filename_for_upload("a\"b\\c\u{7}.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_filename_for_upload("报告.pdf"), "报告.pdf");
    }

    // ---- Voice -----------------------------------------------------------

    #[test]
    fn voice_decision_and_duration() {
        assert_eq!(
            resolve_feishu_voice_decision(false, Some("a.ogg"), None),
            FeishuVoiceDecision::PassThrough
        );
        assert_eq!(
            resolve_feishu_voice_decision(true, Some("a.mp3"), None),
            FeishuVoiceDecision::Transcode
        );
        assert_eq!(
            resolve_feishu_voice_decision(false, Some("a.mp3"), None),
            FeishuVoiceDecision::SendAsFile
        );
        assert_eq!(
            resolve_feishu_voice_decision(true, Some("doc.pdf"), Some("application/pdf")),
            FeishuVoiceDecision::SendAsFile
        );
        assert_eq!(voice_duration_ms_from_seconds(1.5), Some(1500));
        assert_eq!(voice_duration_ms_from_seconds(0.0001), Some(1));
        assert_eq!(voice_duration_ms_from_seconds(0.0), None);
        assert_eq!(voice_duration_ms_from_seconds(f64::NAN), None);
        let args = feishu_voice_transcode_args("in.mp3", "voice.ogg", 300);
        assert!(args.contains(&"libopus".to_string()));
        assert!(args.contains(&"48000".to_string()));
    }

    // ---- Sequential queue ------------------------------------------------

    #[test]
    fn sequential_queue_fifo_and_eviction() {
        let mut q = FeishuSequentialQueue::new();
        let key = feishu_sequential_key("default", Some("oc_1"), FeishuSequentialLane::Main);
        assert_eq!(key, "feishu:default:oc_1");
        let (t1, p1) = q.enqueue(&key);
        let (t2, p2) = q.enqueue(&key);
        assert_eq!((p1, p2), (0, 1));
        assert_eq!(q.head(&key), Some(t1));
        // FIFO drain order.
        assert_eq!(q.complete(&key, t1), Some(t2));
        assert_eq!(q.complete(&key, t2), None);
        // Key evicted once its chain drains.
        assert!(q.is_evicted(&key));
        // Lanes are independent keys.
        let ctrl = feishu_sequential_key("default", Some("oc_1"), FeishuSequentialLane::Control);
        assert_eq!(ctrl, "feishu:default:oc_1:control");
        let btw = feishu_sequential_key("default", None, FeishuSequentialLane::Btw);
        assert_eq!(btw, "feishu:default:unknown:btw");
        // Timeout bound: 0 disables.
        assert!(FeishuSequentialQueue::task_timed_out(0, FEISHU_SEQUENTIAL_TASK_TIMEOUT_MS, FEISHU_SEQUENTIAL_TASK_TIMEOUT_MS));
        assert!(!FeishuSequentialQueue::task_timed_out(0, u64::MAX, 0));
    }

    // ---- Config ----------------------------------------------------------

    #[test]
    fn extension_config_and_api_base() {
        let value = json!({
            "enabled": true,
            "appId": "cli_x",
            "appSecret": "sec",
            "domain": "lark",
            "blockStreaming": true,
            "replyInThread": "enabled",
            "tts": {"provider": "openai"},
            "accounts": {"main": {"tts": {"provider": "edge"}}}
        });
        let cfg = FeishuExtensionConfig::from_extensions_value(Some(&value));
        assert_eq!(cfg.app_id.as_deref(), Some("cli_x"));
        assert!(resolve_block_streaming(&cfg));
        assert_eq!(cfg.reply_in_thread.as_deref(), Some("enabled"));
        assert_eq!(resolve_feishu_api_base(cfg.domain.as_deref()), "https://open.larksuite.com/open-apis");
        assert_eq!(resolve_feishu_api_base(None), "https://open.feishu.cn/open-apis");
        assert_eq!(
            resolve_feishu_api_base(Some("https://feishu.corp.example/")),
            "https://feishu.corp.example/open-apis"
        );
        // Account TTS deep-merges over channel TTS.
        let account_tts = cfg.accounts["main"]["tts"].clone();
        let merged = resolve_effective_tts_config(None, None, cfg.tts.as_ref(), Some(&account_tts));
        assert_eq!(merged["provider"], "edge");
        let missing = FeishuExtensionConfig::from_extensions_value(None);
        assert!(missing.app_id.is_none());
    }
}
