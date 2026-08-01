//! Telegram channel implementation (Bot API).
//!
//! Behavior ports from OpenClaw `extensions/telegram` at v2026.7.1:
//! - per-method Bot API timeout guards (60 s outbound text/typing) with safe
//!   config overrides and clamped long-poll timeouts (`telegram_net`)
//! - markdown → safe HTML chunked sends + media caption follow-ups
//!   (`telegram_format`)
//! - command menu registration/clearing in default + group-chat scopes
//!   (`telegram_commands`)
//! - durable streaming-preview edits (no draft state), benign delete-message
//!   400s as no-op warnings, DM voice-note transcript echo, super-group
//!   admin-only command gating.
//!
//! teloxide mapping: teloxide 0.13's `Bot` applies one global reqwest timeout;
//! the upstream per-method guards are implemented here on a raw reqwest client
//! (`TelegramApi`) with `RequestBuilder::timeout` per call. teloxide's
//! throttling adaptor is not used; the sendChatAction guard below provides the
//! upstream-visible 401/transient typing backoff instead.

use crate::config::Config;
use crate::config::TelegramAccountConfig;
use crate::gateway::GatewayState;
use crate::infra::dm_policy;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use super::telegram_commands::{
    build_capped_menu_commands, build_localized_command_variants, format_command_scope_operation,
    TelegramMenuCommand, TELEGRAM_COMMAND_MENU_SCOPES,
};
use super::telegram_format::{
    build_chunked_text_plan, split_telegram_caption, TELEGRAM_TEXT_CHUNK_LIMIT,
};
use super::telegram_net::{
    fingerprint_telegram_bot_token, is_benign_delete_message_error, is_benign_edit_message_error,
    is_html_parse_error, is_message_not_modified_error, is_no_text_to_edit_error,
    is_safe_to_retry_send_error, is_telegram_rate_limit_error, is_thread_not_found_error,
    read_telegram_retry_after_ms, resolve_telegram_long_poll_timeout_seconds,
    resolve_telegram_request_timeout_ms, ChatActionDecision, FallbackLogLevel, TelegramApiError,
    TelegramSendChatActionGuard, TelegramTransportFallback, DEFAULT_MAX_CONSECUTIVE_401,
    TELEGRAM_TRANSPORT_PINNED_IP, TELEGRAM_TRANSPORT_PRIMARY, TELEGRAM_TRANSPORT_STICKY_IPV4,
};
use super::telegram_targets::parse_telegram_target;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, info, warn};

use async_trait::async_trait;

pub const TELEGRAM_DEFAULT_API_ROOT: &str = "https://api.telegram.org";

// ============================================================================
// v2026.4.1: Error Policy Support
// ============================================================================

/// Telegram error reporting policy (v2026.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorPolicy {
    Always,
    Once,
    Silent,
}

impl ErrorPolicy {
    fn from_config(s: Option<&str>) -> Self {
        match s {
            Some("always") => Self::Always,
            Some("silent") => Self::Silent,
            _ => Self::Once,
        }
    }
}

// ============================================================================
// v2026.2.26: Inline Keyboard Support
// ============================================================================

/// Telegram Mini App descriptor for `web_app` buttons (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebAppInfo {
    /// HTTPS URL of the Mini App to open.
    pub url: String,
}

/// Represents an inline keyboard button for Telegram groups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    /// Button label text.
    pub text: String,
    /// Callback data sent when button is pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    /// URL to open when button is pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Mini App to launch when the button is pressed (v2026.7.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_app: Option<WebAppInfo>,
}

/// Represents an inline keyboard markup for Telegram messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    /// Rows of inline keyboard buttons.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    /// Create a single-row keyboard with the given buttons.
    pub fn single_row(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: vec![buttons],
        }
    }

    /// Create a keyboard with one button per row.
    pub fn column(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: buttons.into_iter().map(|b| vec![b]).collect(),
        }
    }
}

// ============================================================================
// Native Command Registration (default + group-chat scopes, v2026.5.2)
// ============================================================================

/// A Telegram bot command that can be registered with BotFather.
pub type BotCommand = TelegramMenuCommand;

/// Result of a command registration attempt.
#[derive(Debug)]
pub struct CommandRegistrationResult {
    pub success: bool,
    pub registered_count: usize,
    pub error: Option<String>,
}

// ============================================================================
// Bot API client with per-method timeout guards
// ============================================================================

/// Raw Telegram Bot API client. Applies the upstream per-method request
/// timeout guards (60 s outbound text/typing, 45 s getUpdates, 15/30 s
/// control-plane) with config `timeoutSeconds` only ever *extending* them.
/// Sticky-transport state: fallback tier, success streak, and primary probe.
#[derive(Debug, Default)]
struct TransportState {
    fallback: TelegramTransportFallback,
    sticky_success_count: u32,
    probe_primary: bool,
}

/// Sticky fallback successes before probing the primary dispatcher again.
const TELEGRAM_STICKY_FALLBACK_PRIMARY_PROBE_SUCCESS_THRESHOLD: u32 = 20;

pub struct TelegramApi {
    client: reqwest::Client,
    base_url: String,
    timeout_seconds: Option<u64>,
    /// Non-reversible token fingerprint for diagnostics and persisted-offset
    /// identity checks (token-rotation offset discard, v2026.7.1).
    token_fingerprint: String,
    /// Cached getMe identity, invalidated on token rotation (v2026.7.1).
    bot_info_cache: Mutex<Option<serde_json::Value>>,
    /// Sticky transport fallback chain: primary → IPv4-only → pinned-IP
    /// (v2026.7.1). IPv4 promotions/recoveries log debug; pinned stays warn.
    transport: Mutex<TransportState>,
    /// IPv4-only client (binds 0.0.0.0, forcing the IPv4 stack).
    ipv4_client: once_cell::sync::OnceCell<reqwest::Client>,
    /// Pinned-IP client (DNS override to a previously resolved IPv4 addr).
    pinned_client: Mutex<Option<reqwest::Client>>,
}

impl TelegramApi {
    pub fn new(token: &str, api_root: Option<&str>, timeout_seconds: Option<u64>) -> Self {
        let root = api_root
            .map(|r| r.trim_end_matches('/').to_string())
            .unwrap_or_else(|| TELEGRAM_DEFAULT_API_ROOT.to_string());
        Self {
            // No global client timeout: per-request guards below control it.
            client: reqwest::Client::new(),
            base_url: format!("{root}/bot{token}"),
            timeout_seconds,
            token_fingerprint: fingerprint_telegram_bot_token(token),
            bot_info_cache: Mutex::new(None),
            transport: Mutex::new(TransportState::default()),
            ipv4_client: once_cell::sync::OnceCell::new(),
            pinned_client: Mutex::new(None),
        }
    }

    /// Token fingerprint for persisted-state identity checks.
    pub fn token_fingerprint(&self) -> &str {
        &self.token_fingerprint
    }

    /// Cached getMe identity: the Bot API is only hit on the first call (or
    /// after `invalidate_bot_info`), so identity lookups stay off hot paths.
    pub async fn get_me_cached(&self) -> Result<serde_json::Value, TelegramApiError> {
        if let Some(cached) = self.bot_info_cache.lock().unwrap().clone() {
            return Ok(cached);
        }
        let result = self.call("getMe", &serde_json::json!({})).await?;
        *self.bot_info_cache.lock().unwrap() = Some(result.clone());
        Ok(result)
    }

    /// Drops the cached bot identity (e.g. after token rotation).
    pub fn invalidate_bot_info(&self) {
        *self.bot_info_cache.lock().unwrap() = None;
    }

    pub fn from_account(config: &TelegramAccountConfig) -> Option<Self> {
        let token = config
            .bot_token
            .clone()
            .or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())?;
        Some(Self::new(
            &token,
            config.api_root.as_deref(),
            config.timeout_seconds,
        ))
    }

    fn request_timeout(&self, method: &str) -> Option<Duration> {
        resolve_telegram_request_timeout_ms(&method.to_lowercase(), self.timeout_seconds)
            .map(Duration::from_millis)
    }

    fn api_host(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    }

    fn ipv4_only_client(&self) -> &reqwest::Client {
        self.ipv4_client.get_or_init(|| {
            reqwest::Client::builder()
                .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
    }

    /// Builds (and caches) the pinned-IP client: DNS overridden to an IPv4
    /// address resolved out-of-band, for hosts whose resolver flaps.
    async fn pinned_ip_client(&self) -> reqwest::Client {
        if let Some(client) = self.pinned_client.lock().unwrap().clone() {
            return client;
        }
        let host = self.api_host().unwrap_or_else(|| "api.telegram.org".to_string());
        let resolved = tokio::net::lookup_host((host.as_str(), 443))
            .await
            .ok()
            .and_then(|addrs| addrs.filter(|a| a.is_ipv4()).next());
        let client = match resolved {
            Some(addr) => reqwest::Client::builder()
                .resolve(&host, addr)
                .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
                .build()
                .unwrap_or_else(|_| self.ipv4_only_client().clone()),
            None => self.ipv4_only_client().clone(),
        };
        *self.pinned_client.lock().unwrap() = Some(client.clone());
        client
    }

    async fn client_for_tier(&self, tier: usize) -> reqwest::Client {
        match tier {
            TELEGRAM_TRANSPORT_PRIMARY => self.client.clone(),
            TELEGRAM_TRANSPORT_STICKY_IPV4 => self.ipv4_only_client().clone(),
            _ => self.pinned_ip_client().await,
        }
    }

    fn emit_fallback_event(event: super::telegram_net::FallbackLogEvent) {
        match event.level {
            FallbackLogLevel::Debug => debug!("{}", event.message),
            FallbackLogLevel::Warn => warn!("{}", event.message),
        }
    }

    /// One wire request through a specific client.
    async fn request_once(
        &self,
        client: &reqwest::Client,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, TelegramApiError> {
        let mut request = client.post(format!("{}/{}", self.base_url, method)).json(body);
        if let Some(timeout) = self.request_timeout(method) {
            request = request.timeout(timeout);
        }
        let response = request.send().await.map_err(|e| {
            TelegramApiError::transport(format!("{method} transport error: {e}"), e.is_timeout())
        })?;
        let payload: serde_json::Value = response.json().await.map_err(|e| {
            TelegramApiError::transport(format!("{method} invalid response: {e}"), false)
        })?;
        if payload.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(payload.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
        Err(TelegramApiError {
            error_code: payload.get("error_code").and_then(|v| v.as_i64()),
            description: payload
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown Telegram API error")
                .to_string(),
            retry_after_seconds: payload
                .pointer("/parameters/retry_after")
                .and_then(|v| v.as_u64()),
            transport: false,
            timed_out: false,
        })
    }

    /// Calls a Bot API method, returning the `result` payload on success.
    ///
    /// Transport failures walk the sticky fallback chain (primary →
    /// IPv4-only → pinned-IP) within this call; the reached tier is sticky
    /// for later calls. After 20 sticky successes a primary recovery probe
    /// runs so hosts that healed leave the fallback without a restart.
    pub async fn call(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, TelegramApiError> {
        let (sticky_tier, probing) = {
            let mut state = self.transport.lock().unwrap();
            let probing = state.probe_primary;
            state.probe_primary = false;
            (state.fallback.sticky_attempt_index(), probing)
        };

        let mut tier = if probing { TELEGRAM_TRANSPORT_PRIMARY } else { sticky_tier };
        loop {
            let client = self.client_for_tier(tier).await;
            match self.request_once(&client, method, body).await {
                Ok(result) => {
                    let mut state = self.transport.lock().unwrap();
                    if let Some(event) = state.fallback.record_success(tier) {
                        Self::emit_fallback_event(event);
                        state.sticky_success_count = 0;
                    } else if state.fallback.sticky_attempt_index() > TELEGRAM_TRANSPORT_PRIMARY {
                        state.sticky_success_count += 1;
                        if state.sticky_success_count
                            >= TELEGRAM_STICKY_FALLBACK_PRIMARY_PROBE_SUCCESS_THRESHOLD
                        {
                            state.sticky_success_count = 0;
                            state.probe_primary = true;
                            debug!("fetch fallback: scheduling primary dispatcher recovery probe");
                        }
                    }
                    return Ok(result);
                }
                Err(err) if err.transport && !err.timed_out => {
                    if probing && tier == TELEGRAM_TRANSPORT_PRIMARY && sticky_tier > tier {
                        // Probe failed: return to the sticky tier this call.
                        debug!("fetch fallback: primary recovery probe failed; staying sticky");
                        tier = sticky_tier;
                        continue;
                    }
                    if tier < TELEGRAM_TRANSPORT_PINNED_IP {
                        let next = tier + 1;
                        if let Some(event) = self.transport.lock().unwrap().fallback.promote(next) {
                            Self::emit_fallback_event(event);
                        }
                        tier = next;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Durable send: retries flood-waits (429 `retry_after`, capped, up to 2)
    /// and safe-to-retry transport errors (HTTP 421 misdirected / pre-connect
    /// failures — never post-connect timeouts, which could duplicate sends).
    /// Sends targeting a forum thread fail closed on "message thread not
    /// found" (v2026.7.1 transport row).
    pub async fn call_with_send_retry(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, TelegramApiError> {
        const MAX_FLOOD_WAIT_RETRIES: u32 = 2;
        const MAX_FLOOD_WAIT_MS: u64 = 30_000;
        let mut flood_retries = 0u32;
        let mut transport_retried = false;
        loop {
            match self.call(method, body).await {
                Ok(result) => return Ok(result),
                Err(err) if !err.transport && is_thread_not_found_error(&err.description) => {
                    // Fail closed: never strip the thread id and retry.
                    warn!("telegram {method} failed closed (thread not found): {err}");
                    return Err(err);
                }
                Err(err)
                    if is_telegram_rate_limit_error(&err)
                        && flood_retries < MAX_FLOOD_WAIT_RETRIES =>
                {
                    flood_retries += 1;
                    let wait_ms = read_telegram_retry_after_ms(&err)
                        .unwrap_or(1_000)
                        .min(MAX_FLOOD_WAIT_MS);
                    debug!(
                        "telegram {method} flood-wait: sleeping {wait_ms}ms before retry \
                         ({flood_retries}/{MAX_FLOOD_WAIT_RETRIES})"
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
                Err(err) if !transport_retried && is_safe_to_retry_send_error(&err) => {
                    transport_retried = true;
                    debug!("telegram {method} pre-connect failure; retrying once: {err}");
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// getUpdates with the clamped long-poll `timeout` parameter and the fixed
    /// 45 s HTTP guard, so a configured `timeoutSeconds` lower than the poll
    /// window does not churn HTTPS connections.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
    ) -> Result<serde_json::Value, TelegramApiError> {
        let poll_timeout = resolve_telegram_long_poll_timeout_seconds(self.timeout_seconds);
        let mut body = serde_json::json!({ "timeout": poll_timeout });
        if let Some(offset) = offset {
            body["offset"] = serde_json::json!(offset);
        }
        self.call("getUpdates", &body).await
    }

    pub async fn get_chat_member_status(
        &self,
        chat_id: &str,
        user_id: i64,
    ) -> Result<Option<String>, TelegramApiError> {
        let result = self
            .call(
                "getChatMember",
                &serde_json::json!({ "chat_id": chat_id, "user_id": user_id }),
            )
            .await?;
        Ok(result
            .get("status")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }
}

/// Outcome of a durable message edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableEditOutcome {
    Edited,
    /// Content identical — benign no-op.
    NotModified,
    /// The target has no editable text — benign for previews.
    NoTextToEdit,
    /// Target vanished / not editable — benign for previews.
    MessageGone,
}

impl TelegramApi {
    /// Sends markdown text as safe HTML chunks (≤ 4000 UTF-16 units each),
    /// falling back to plain text per chunk on Bot API HTML parse errors.
    /// Returns the last sent message id.
    pub async fn send_message_chunked(
        &self,
        chat_id: &str,
        markdown_text: &str,
        thread_id: Option<i64>,
        reply_markup: Option<&serde_json::Value>,
    ) -> Result<Option<i64>, TelegramApiError> {
        // Sticker path (v2026.7.1): `sticker:<file_id>` messages deliver as
        // native stickers instead of text.
        if let Some(file_id) = super::telegram_format::parse_sticker_reference(markdown_text) {
            let mut body = serde_json::json!({ "chat_id": chat_id, "sticker": file_id });
            if let Some(thread_id) = thread_id {
                body["message_thread_id"] = serde_json::json!(thread_id);
            }
            let result = self.call_with_send_retry("sendSticker", &body).await?;
            return Ok(result.get("message_id").and_then(|v| v.as_i64()));
        }

        let plan = build_chunked_text_plan(markdown_text, TELEGRAM_TEXT_CHUNK_LIMIT);
        let chunk_count = plan.len();
        let mut last_message_id = None;
        for (index, chunk) in plan.iter().enumerate() {
            let is_last = index + 1 == chunk_count;
            let mut base = serde_json::json!({ "chat_id": chat_id });
            if let Some(thread_id) = thread_id {
                base["message_thread_id"] = serde_json::json!(thread_id);
            }
            if is_last {
                if let Some(markup) = reply_markup {
                    base["reply_markup"] = markup.clone();
                }
            }
            let result = match &chunk.html_text {
                Some(html) => {
                    let mut html_body = base.clone();
                    html_body["text"] = serde_json::json!(html);
                    html_body["parse_mode"] = serde_json::json!("HTML");
                    match self.call_with_send_retry("sendMessage", &html_body).await {
                        Ok(res) => Ok(res),
                        Err(err) if !err.transport && is_html_parse_error(&err.description) => {
                            debug!(
                                "telegram sendMessage HTML parse failed; retrying chunk {} as plain text: {}",
                                index + 1,
                                err.description
                            );
                            let mut plain_body = base.clone();
                            plain_body["text"] = serde_json::json!(chunk.plain_text);
                            self.call_with_send_retry("sendMessage", &plain_body).await
                        }
                        Err(err) => Err(err),
                    }
                }
                None => {
                    let mut plain_body = base.clone();
                    plain_body["text"] = serde_json::json!(chunk.plain_text);
                    self.call_with_send_retry("sendMessage", &plain_body).await
                }
            }?;
            last_message_id = result.get("message_id").and_then(|v| v.as_i64());
        }
        Ok(last_message_id)
    }

    /// Rich-message delivery (`richMessages: true`): renders block-structured
    /// rich HTML chunks (32k units, block/media caps shaping message
    /// boundaries), then wire-splits each rich chunk to the Bot API 4000-unit
    /// limit for delivery. Falls back to the plain chunk plan when rich
    /// rendering fails (rich→plain fallback).
    pub async fn send_rich_markdown(
        &self,
        chat_id: &str,
        markdown_text: &str,
        thread_id: Option<i64>,
    ) -> Result<Option<i64>, TelegramApiError> {
        let rich_chunks = match super::telegram_format::render_rich_message_chunks(
            markdown_text,
            super::telegram_format::TELEGRAM_RICH_TEXT_LIMIT,
        ) {
            Ok(chunks) => chunks,
            Err(err) => {
                debug!("telegram rich render failed ({err}); falling back to plain delivery");
                return self
                    .send_message_chunked(chat_id, markdown_text, thread_id, None)
                    .await;
            }
        };
        let mut last_id = None;
        for rich_chunk in rich_chunks {
            let wire_chunks = super::telegram_format::split_telegram_html_chunks(
                &rich_chunk,
                TELEGRAM_TEXT_CHUNK_LIMIT,
            )
            .unwrap_or_else(|_| vec![rich_chunk.clone()]);
            for html in wire_chunks {
                let mut body = serde_json::json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                });
                if let Some(thread_id) = thread_id {
                    body["message_thread_id"] = serde_json::json!(thread_id);
                }
                let result = match self.call_with_send_retry("sendMessage", &body).await {
                    Err(err) if !err.transport && is_html_parse_error(&err.description) => {
                        let plain = super::telegram_format::telegram_html_to_plain_text(&html);
                        let mut plain_body = serde_json::json!({
                            "chat_id": chat_id,
                            "text": plain,
                        });
                        if let Some(thread_id) = thread_id {
                            plain_body["message_thread_id"] = serde_json::json!(thread_id);
                        }
                        self.call_with_send_retry("sendMessage", &plain_body).await?
                    }
                    other => other?,
                };
                last_id = result.get("message_id").and_then(|v| v.as_i64());
            }
        }
        Ok(last_id)
    }

    /// Sends media with a caption when it fits Telegram's 1024-char caption
    /// limit; otherwise sends the media bare and the full text as a chunked
    /// follow-up message (upstream `splitTelegramCaption` + follow-up send).
    pub async fn send_media_with_follow_up(
        &self,
        chat_id: &str,
        method: &str,
        media_field: &str,
        media_url: &str,
        text: Option<&str>,
        thread_id: Option<i64>,
    ) -> Result<serde_json::Value, TelegramApiError> {
        let split = split_telegram_caption(text);
        let mut body = serde_json::json!({ "chat_id": chat_id, media_field: media_url });
        if let Some(thread_id) = thread_id {
            body["message_thread_id"] = serde_json::json!(thread_id);
        }
        if let Some(caption) = &split.caption {
            body["caption"] = serde_json::json!(caption);
        }
        let result = self.call_with_send_retry(method, &body).await?;
        if let Some(follow_up) = &split.follow_up_text {
            self.send_message_chunked(chat_id, follow_up, thread_id, None)
                .await?;
        }
        Ok(result)
    }

    /// Durable message edit for streaming previews: edits real messages in
    /// place (no Bot API draft state), treating "message is not modified",
    /// "no text to edit", and vanished-message 400s as benign outcomes so a
    /// stream is never killed by a cosmetic edit failure.
    pub async fn edit_message_durable(
        &self,
        chat_id: &str,
        message_id: i64,
        markdown_text: &str,
    ) -> Result<DurableEditOutcome, TelegramApiError> {
        let plan = build_chunked_text_plan(markdown_text, TELEGRAM_TEXT_CHUNK_LIMIT);
        // Edits target one message: use the first chunk (callers overflow into
        // new messages when the preview exceeds one chunk).
        let Some(chunk) = plan.first() else {
            return Ok(DurableEditOutcome::NotModified);
        };
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });
        let attempt = match &chunk.html_text {
            Some(html) => {
                body["text"] = serde_json::json!(html);
                body["parse_mode"] = serde_json::json!("HTML");
                match self.call("editMessageText", &body).await {
                    Err(err) if !err.transport && is_html_parse_error(&err.description) => {
                        let plain_body = serde_json::json!({
                            "chat_id": chat_id,
                            "message_id": message_id,
                            "text": chunk.plain_text,
                        });
                        self.call("editMessageText", &plain_body).await
                    }
                    other => other,
                }
            }
            None => {
                body["text"] = serde_json::json!(chunk.plain_text);
                self.call("editMessageText", &body).await
            }
        };
        match attempt {
            Ok(_) => Ok(DurableEditOutcome::Edited),
            Err(err) if !err.transport && is_message_not_modified_error(&err.description) => {
                Ok(DurableEditOutcome::NotModified)
            }
            Err(err) if !err.transport && is_no_text_to_edit_error(&err.description) => {
                debug!("telegram edit skipped: {}", err.description);
                Ok(DurableEditOutcome::NoTextToEdit)
            }
            Err(err) if !err.transport && is_benign_edit_message_error(&err.description) => {
                warn!("telegram edit no-op ({})", err.description);
                Ok(DurableEditOutcome::MessageGone)
            }
            Err(err) => Err(err),
        }
    }

    /// deleteMessage treating the benign 400 family (already deleted / not
    /// deletable / forbidden) as a no-op warning instead of a failure.
    /// Returns `true` when the message was actually deleted.
    pub async fn delete_message_benign(
        &self,
        chat_id: &str,
        message_id: i64,
    ) -> Result<bool, TelegramApiError> {
        let body = serde_json::json!({ "chat_id": chat_id, "message_id": message_id });
        match self.call("deleteMessage", &body).await {
            Ok(_) => Ok(true),
            Err(err) if !err.transport && is_benign_delete_message_error(&err.description) => {
                warn!(
                    "telegram deleteMessage no-op for {}/{}: {}",
                    chat_id, message_id, err.description
                );
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }
}

// ============================================================================
// Streaming Preview (durable edits — no draft state)
// ============================================================================

/// Tracks the state of a streaming preview message.
///
/// v2026.5.2: previews are durable real messages edited in place — there is no
/// Bot API draft state. Benign edit failures (not modified / message gone) do
/// not finalize or kill the preview stream.
#[derive(Debug)]
pub struct StreamingPreview {
    /// Telegram chat_id where the preview message was sent.
    pub chat_id: i64,
    /// message_id of the preview message being updated.
    pub message_id: i64,
    /// Whether the final edit has been applied.
    pub finalized: bool,
    /// Accumulated text content.
    pub content: String,
}

impl StreamingPreview {
    pub fn new(chat_id: i64, message_id: i64) -> Self {
        Self {
            chat_id,
            message_id,
            finalized: false,
            content: String::new(),
        }
    }

    /// Mark this preview as finalized. After this, no more edits will be sent.
    pub fn finalize(&mut self) {
        self.finalized = true;
    }

    /// Whether more edits should be sent.
    pub fn should_edit(&self) -> bool {
        !self.finalized
    }

    /// Applies a durable content update through the Bot API. Benign edit
    /// outcomes leave the preview open for further edits.
    pub async fn apply_update(
        &mut self,
        api: &TelegramApi,
        content: &str,
    ) -> Result<DurableEditOutcome, TelegramApiError> {
        self.content = content.to_string();
        api.edit_message_durable(&self.chat_id.to_string(), self.message_id, content)
            .await
    }
}

// ============================================================================
// v2026.7.1 feature helpers
// ============================================================================

/// Telegram Bot API poll option cap (v2026.7.1: 10-option poll cap).
pub const TELEGRAM_POLL_MAX_OPTIONS: usize = 10;

/// Clamps poll options to the Bot API cap, logging dropped options.
pub fn clamp_poll_options(options: Vec<String>) -> Vec<String> {
    if options.len() > TELEGRAM_POLL_MAX_OPTIONS {
        warn!(
            "telegram sendPoll: dropping {} options over the {}-option Bot API cap",
            options.len() - TELEGRAM_POLL_MAX_OPTIONS,
            TELEGRAM_POLL_MAX_OPTIONS
        );
    }
    options
        .into_iter()
        .take(TELEGRAM_POLL_MAX_OPTIONS)
        .collect()
}

/// Rich-message delivery flag (`channels.telegram.richMessages`) — default
/// **false** (v2026.7.1: plain HTML chunk delivery unless opted in).
pub fn resolve_rich_messages(account: &TelegramAccountConfig) -> bool {
    account.rich_messages.unwrap_or(false)
}

/// Whether a local file path may be sent through a local Bot API server:
/// only paths under a configured `trustedLocalFileRoots` entry pass, after
/// canonicalization (v2026.7.1). No roots configured = no local paths.
pub fn is_trusted_local_file_path(roots: &[String], path: &std::path::Path) -> bool {
    if roots.is_empty() {
        return false;
    }
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    roots.iter().any(|root| {
        std::path::Path::new(root)
            .canonicalize()
            .map(|canonical_root| canonical.starts_with(&canonical_root))
            .unwrap_or(false)
    })
}

/// DM exec-approval decision (v2026.7.1): the allowlist keeps working with
/// `ask: off` — an allowlisted sender is auto-approved, a non-allowlisted
/// sender still requires approval even when asking is off (upstream #89035:
/// "keep Telegram DM exec approval allowlists working with `ask:off`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecApprovalDecision {
    AutoApproved,
    RequiresApproval,
}

pub fn resolve_dm_exec_approval(
    ask_off: bool,
    allowlist: Option<&[String]>,
    sender_id: &str,
) -> ExecApprovalDecision {
    match allowlist {
        Some(allowlist) if !allowlist.is_empty() => {
            if allowlist.iter().any(|entry| entry == sender_id) {
                ExecApprovalDecision::AutoApproved
            } else {
                // Allowlist configured: ask:off must NOT bypass it.
                ExecApprovalDecision::RequiresApproval
            }
        }
        _ => {
            if ask_off {
                ExecApprovalDecision::AutoApproved
            } else {
                ExecApprovalDecision::RequiresApproval
            }
        }
    }
}

// ============================================================================
// DM voice-note transcript echo (v2026.5.2)
// ============================================================================

/// Default operator-visible transcript echo format (upstream
/// `DEFAULT_ECHO_TRANSCRIPT_FORMAT`).
pub const DEFAULT_ECHO_TRANSCRIPT_FORMAT: &str = "📝 \"{transcript}\"";

/// Formats a transcript echo, replacing the first `{transcript}` placeholder
/// literally (no substitution-pattern parsing of the transcript itself).
pub fn format_echo_transcript(transcript: &str, format: &str) -> String {
    format.replacen("{transcript}", transcript, 1)
}

/// Resolves the transcript echo format from `tools.media.audio` config
/// (`echoTranscript` + `echoFormat`, echo disabled by default). Returns `None`
/// when echoing is disabled.
pub fn resolve_audio_echo_format(config: &Config) -> Option<String> {
    let audio = config.tools.media.as_ref()?.audio.as_ref()?;
    if audio.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    if audio.get("echoTranscript").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    Some(
        audio
            .get("echoFormat")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_ECHO_TRANSCRIPT_FORMAT)
            .to_string(),
    )
}

/// Sends a best-effort echo of a preflighted DM voice-note transcript back to
/// the originating chat, including DM topic thread metadata. Failures are
/// logged, never propagated (echo must not block message handling).
pub async fn send_transcript_echo(
    api: &TelegramApi,
    config: &Config,
    chat_id: &str,
    thread_id: Option<i64>,
    transcript: &str,
) {
    let Some(format) = resolve_audio_echo_format(config) else {
        debug!("telegram: echo-transcript skipped (echoTranscript disabled)");
        return;
    };
    let text = format_echo_transcript(transcript, &format);
    match api
        .send_message_chunked(chat_id, &text, thread_id, None)
        .await
    {
        Ok(_) => debug!("telegram: echo-transcript sent to {chat_id}"),
        Err(err) => debug!("telegram: echo-transcript delivery failed: {err}"),
    }
}

// ============================================================================
// Telegram Channel Implementation
// ============================================================================

/// Telegram channel implementation using the Bot API.
pub struct TelegramChannel {
    enabled: bool,
    bot_token: Option<String>,
    account: TelegramAccountConfig,
    /// v2026.2.26: DM allowlist from config.
    dm_allow_from: Vec<String>,
    /// Error policy for error reporting (v2026.4.1)
    error_policy: ErrorPolicy,
    /// Cooldown tracker for error reports (v2026.4.1)
    last_error_reported: Option<std::time::Instant>,
    /// Error cooldown in ms (v2026.4.1)
    error_cooldown_ms: u64,
    /// Global per-account sendChatAction 401/transient guard (v2026.5.2).
    chat_action_guard: Mutex<TelegramSendChatActionGuard>,
}

impl TelegramChannel {
    pub fn new(config: &Config) -> Self {
        let tg = &config.channels.telegram;
        let account = tg.default_account.clone();
        let bot_token = account.bot_token.clone();
        let enabled = account.enabled.unwrap_or(bot_token.is_some());

        // v2026.2.26: Load DM allowlist (does NOT inherit from parent)
        let dm_allow_from = account.allow_from.clone().unwrap_or_default();

        // v2026.4.1: Load error policy and cooldown from config
        let error_policy = ErrorPolicy::from_config(account.error_policy.as_deref());
        let error_cooldown_ms = account.error_cooldown_ms.unwrap_or(60_000);

        Self {
            enabled,
            bot_token,
            account,
            dm_allow_from,
            error_policy,
            last_error_reported: None,
            error_cooldown_ms,
            chat_action_guard: Mutex::new(TelegramSendChatActionGuard::new(
                DEFAULT_MAX_CONSECUTIVE_401,
                6_000,
            )),
        }
    }

    fn api(&self) -> Result<TelegramApi> {
        TelegramApi::from_account(&self.account)
            .ok_or_else(|| anyhow::anyhow!("Telegram bot token not configured"))
    }

    /// v2026.2.26: Check if a Telegram user is allowed to DM the bot.
    pub fn is_dm_allowed(&self, user_id: &str) -> bool {
        if self.dm_allow_from.is_empty() {
            // No allowlist = allow all DMs
            return true;
        }
        dm_policy::is_source_allowed(&self.dm_allow_from, user_id)
    }

    /// Registers bot commands in the default AND group-chat scopes
    /// (v2026.5.2), applying the Bot API menu budget (≤ 100 commands, 5700
    /// total chars). Gracefully degrades per scope — commands remain usable
    /// even without registration.
    pub async fn register_commands(&self, commands: &[BotCommand]) -> CommandRegistrationResult {
        let api = match self.api() {
            Ok(api) => api,
            Err(_) => {
                return CommandRegistrationResult {
                    success: false,
                    registered_count: 0,
                    error: Some("No bot token configured".to_string()),
                };
            }
        };
        let fitted = build_capped_menu_commands(commands);
        if fitted.description_trimmed {
            debug!("telegram: command menu descriptions trimmed to fit Bot API budget");
        }
        if fitted.text_budget_drop_count > 0 {
            warn!(
                "telegram: dropped {} commands over the Bot API menu budget",
                fitted.text_budget_drop_count
            );
        }
        let mut success = true;
        let mut first_error = None;
        for scope in TELEGRAM_COMMAND_MENU_SCOPES {
            let operation = format_command_scope_operation("setMyCommands", scope);
            let mut body = serde_json::json!({ "commands": fitted.commands });
            if let Some(scope_value) = scope.api_scope() {
                body["scope"] = scope_value;
            }
            match api.call("setMyCommands", &body).await {
                Ok(_) => {
                    info!(
                        "Registered {} Telegram bot commands ({operation})",
                        fitted.commands.len()
                    );
                }
                Err(err) => {
                    // Graceful degradation — log warning but keep going.
                    warn!("Telegram {operation} failed: {err}");
                    success = false;
                    first_error.get_or_insert_with(|| err.to_string());
                }
            }
        }

        // Native command localization (v2026.7.1): one menu variant per
        // locale, registered with language_code in both scopes.
        let (variants, unsupported) = build_localized_command_variants(commands);
        for language in &unsupported {
            warn!("telegram: skipping unsupported command menu language code {language:?}");
        }
        for variant in variants {
            for scope in TELEGRAM_COMMAND_MENU_SCOPES {
                let operation = format!(
                    "{}[{}]",
                    format_command_scope_operation("setMyCommands", scope),
                    variant.language_code
                );
                let mut body = serde_json::json!({
                    "commands": variant.commands,
                    "language_code": variant.language_code,
                });
                if let Some(scope_value) = scope.api_scope() {
                    body["scope"] = scope_value;
                }
                if let Err(err) = api.call("setMyCommands", &body).await {
                    warn!("Telegram {operation} failed: {err}");
                }
            }
        }

        CommandRegistrationResult {
            registered_count: if success { fitted.commands.len() } else { 0 },
            success,
            error: first_error,
        }
    }

    /// Clears the command menu in the default AND group-chat scopes.
    pub async fn clear_commands(&self) -> CommandRegistrationResult {
        let api = match self.api() {
            Ok(api) => api,
            Err(_) => {
                return CommandRegistrationResult {
                    success: false,
                    registered_count: 0,
                    error: Some("No bot token configured".to_string()),
                };
            }
        };
        let mut success = true;
        let mut first_error = None;
        for scope in TELEGRAM_COMMAND_MENU_SCOPES {
            let operation = format_command_scope_operation("deleteMyCommands", scope);
            let mut body = serde_json::json!({});
            if let Some(scope_value) = scope.api_scope() {
                body["scope"] = scope_value;
            }
            match api.call("deleteMyCommands", &body).await {
                Ok(_) => info!("Cleared Telegram bot commands ({operation})"),
                Err(err) => {
                    warn!("Telegram {operation} failed: {err}");
                    success = false;
                    first_error.get_or_insert_with(|| err.to_string());
                }
            }
        }
        CommandRegistrationResult {
            success,
            registered_count: 0,
            error: first_error,
        }
    }

    /// Sends a typing indicator through the 401/transient guard, retrying a
    /// timed-out attempt once (sendChatAction is idempotent, so the transport
    /// fallback retry cannot duplicate messages).
    pub async fn send_typing(&self, chat_id: &str, thread_id: Option<i64>) -> Result<()> {
        let api = self.api()?;
        let now_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        let attempted_at = now_ms();
        let decision = {
            let mut guard = self.chat_action_guard.lock().unwrap();
            guard.begin_attempt(chat_id, "typing", attempted_at)
        };
        let backoff_ms = match decision {
            ChatActionDecision::Suspended | ChatActionDecision::Coalesced => return Ok(()),
            ChatActionDecision::TransientCooldown { remaining_ms } => {
                anyhow::bail!("sendChatAction transient cooldown active for {remaining_ms}ms")
            }
            ChatActionDecision::Proceed { backoff_ms } => backoff_ms,
        };
        if backoff_ms > 0 {
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
        let mut body = serde_json::json!({ "chat_id": chat_id, "action": "typing" });
        if let Some(thread_id) = thread_id {
            body["message_thread_id"] = serde_json::json!(thread_id);
        }
        let mut result = api.call("sendChatAction", &body).await;
        if matches!(&result, Err(err) if err.transport && err.timed_out) {
            // Typing fallback retry: idempotent, safe to retry once.
            debug!("telegram sendChatAction timed out; retrying through transport fallback");
            result = api.call("sendChatAction", &body).await;
        }
        let outcome = match result {
            Ok(_) => {
                let mut guard = self.chat_action_guard.lock().unwrap();
                guard.record_success();
                guard.finish_attempt(chat_id, "typing", attempted_at);
                Ok(())
            }
            Err(err) => {
                let mut guard = self.chat_action_guard.lock().unwrap();
                guard.record_failure(&err, chat_id, "typing", attempted_at, now_ms());
                guard.finish_attempt(chat_id, "typing", attempted_at);
                Err(anyhow::anyhow!(err))
            }
        };
        outcome
    }

    /// v2026.2.26: Send a message with inline keyboard buttons (for groups).
    pub async fn send_message_with_buttons(
        &self,
        chat_id: &str,
        message: &str,
        keyboard: &InlineKeyboardMarkup,
    ) -> Result<()> {
        let api = self.api()?;
        let markup = serde_json::to_value(keyboard)?;
        api.send_message_chunked(chat_id, message, None, Some(&markup))
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }
}

#[async_trait]
impl ChannelPlugin for TelegramChannel {
    fn id(&self) -> &str {
        "telegram"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Telegram".to_string(),
            description: "Telegram Bot API channel".to_string(),
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
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
            ChannelCapability::Stickers,
            ChannelCapability::Voice,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let token = match &self.bot_token {
            Some(t) => t,
            None => {
                warn!("Telegram channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!(
            "Telegram channel starting (token ends ...{})",
            &token[token.len().saturating_sub(4)..]
        );

        // Register command menus in default + group-chat scopes (best-effort).
        if self.account.commands.unwrap_or(true) {
            let commands: Vec<BotCommand> = self
                .account
                .custom_commands
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|c| BotCommand {
                    command: c.command,
                    description: c.description,
                    description_localizations: None,
                })
                .collect();
            if !commands.is_empty() {
                let _ = self.register_commands(&commands).await;
            }
        }

        // Long polling itself is dispatched by the channel manager; getUpdates
        // calls go through `TelegramApi::get_updates`, which clamps the poll
        // window against the 45 s HTTP guard.
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Telegram channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let api = self.api()?;

        // Accepts `telegram:`/`tg:` prefixes, `chatId`, `chatId:topicId`,
        // `chatId:topic:topicId`, and `@username` (v2026.7.1 targets port).
        let target = parse_telegram_target(to);
        if target.chat_id.is_empty() {
            anyhow::bail!("invalid Telegram target: {to}");
        }

        info!(chat_id = %target.chat_id, "Telegram: sending message");
        let thread_id = target.message_thread_id.map(|id| id as i64);
        if resolve_rich_messages(&self.account) {
            api.send_rich_markdown(&target.chat_id, message, thread_id)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
        } else {
            api.send_message_chunked(&target.chat_id, message, thread_id, None)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = TelegramChannel::new(config);
    channel.send_message(to, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_keyboard_single_row() {
        let buttons = vec![
            InlineKeyboardButton {
                text: "Yes".to_string(),
                callback_data: Some("yes".to_string()),
                url: None,
                web_app: None,
            },
            InlineKeyboardButton {
                text: "No".to_string(),
                callback_data: Some("no".to_string()),
                url: None,
                web_app: None,
            },
        ];
        let kb = InlineKeyboardMarkup::single_row(buttons);
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
    }

    #[test]
    fn inline_keyboard_column() {
        let buttons = vec![
            InlineKeyboardButton {
                text: "Option A".to_string(),
                callback_data: Some("a".to_string()),
                url: None,
                web_app: None,
            },
            InlineKeyboardButton {
                text: "Option B".to_string(),
                callback_data: Some("b".to_string()),
                url: None,
                web_app: None,
            },
        ];
        let kb = InlineKeyboardMarkup::column(buttons);
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0].len(), 1);
        assert_eq!(kb.inline_keyboard[1].len(), 1);
    }

    #[test]
    fn inline_keyboard_serialization() {
        let kb = InlineKeyboardMarkup::single_row(vec![InlineKeyboardButton {
            text: "Click".to_string(),
            callback_data: Some("data".to_string()),
            url: None,
            web_app: None,
        }]);

        let json = serde_json::to_value(&kb).unwrap();
        assert!(json["inline_keyboard"].is_array());
        assert_eq!(json["inline_keyboard"][0][0]["text"], "Click");
    }

    #[test]
    fn streaming_preview_lifecycle() {
        let mut preview = StreamingPreview::new(12345, 67890);
        assert!(preview.should_edit());
        assert!(!preview.finalized);

        preview.content.push_str("Hello ");
        preview.content.push_str("World");
        assert_eq!(preview.content, "Hello World");

        preview.finalize();
        assert!(!preview.should_edit());
        assert!(preview.finalized);
    }

    #[test]
    fn dm_allowlist_empty_allows_all() {
        let config = Config::default();
        let channel = TelegramChannel::new(&config);
        assert!(channel.is_dm_allowed("any_user"));
    }

    #[test]
    fn bot_command_serialization() {
        let cmd = BotCommand {
            command: "help".to_string(),
            description: "Show help message".to_string(),
            description_localizations: None,
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["command"], "help");
        assert_eq!(json["description"], "Show help message");
    }

    #[test]
    fn poll_options_capped_at_10() {
        let options: Vec<String> = (0..14).map(|i| format!("opt{i}")).collect();
        let clamped = clamp_poll_options(options);
        assert_eq!(clamped.len(), TELEGRAM_POLL_MAX_OPTIONS);
        assert_eq!(clamped[0], "opt0");
        assert_eq!(clamped[9], "opt9");
        // Under the cap: untouched.
        let two = clamp_poll_options(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(two.len(), 2);
    }

    #[test]
    fn rich_messages_defaults_false() {
        let account = TelegramAccountConfig::default();
        assert!(!resolve_rich_messages(&account));
        let enabled = TelegramAccountConfig {
            rich_messages: Some(true),
            ..Default::default()
        };
        assert!(resolve_rich_messages(&enabled));
    }

    #[test]
    fn web_app_button_serializes() {
        let button = InlineKeyboardButton {
            text: "Open".to_string(),
            callback_data: None,
            url: None,
            web_app: Some(WebAppInfo {
                url: "https://app.example/miniapp".to_string(),
            }),
        };
        let json = serde_json::to_value(&button).unwrap();
        assert_eq!(json["web_app"]["url"], "https://app.example/miniapp");
        assert!(json.get("url").is_none());
    }

    #[test]
    fn api_token_fingerprint_exposed() {
        let api = TelegramApi::new("12345:AAAA", None, None);
        assert_eq!(api.token_fingerprint().len(), 16);
        let rotated = TelegramApi::new("12345:BBBB", None, None);
        assert_ne!(api.token_fingerprint(), rotated.token_fingerprint());
    }

    #[test]
    fn trusted_local_file_roots_enforced() {
        let dir = std::env::temp_dir().join("mylobster-tg-trusted-test");
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let file = inner.join("doc.pdf");
        std::fs::write(&file, b"x").unwrap();

        let roots = vec![dir.to_string_lossy().to_string()];
        assert!(is_trusted_local_file_path(&roots, &file));
        // Outside the root: rejected.
        assert!(!is_trusted_local_file_path(
            &roots,
            std::path::Path::new("/etc/hosts")
        ));
        // No roots configured: no local paths at all.
        assert!(!is_trusted_local_file_path(&[], &file));
        // Nonexistent path: rejected (canonicalization fails closed).
        assert!(!is_trusted_local_file_path(&roots, &dir.join("missing.bin")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dm_exec_approval_allowlist_survives_ask_off() {
        let allow = vec!["42".to_string()];
        // Allowlisted sender auto-approved regardless of ask mode.
        assert_eq!(
            resolve_dm_exec_approval(true, Some(&allow), "42"),
            ExecApprovalDecision::AutoApproved
        );
        assert_eq!(
            resolve_dm_exec_approval(false, Some(&allow), "42"),
            ExecApprovalDecision::AutoApproved
        );
        // ask:off must NOT bypass a configured allowlist (upstream #89035).
        assert_eq!(
            resolve_dm_exec_approval(true, Some(&allow), "77"),
            ExecApprovalDecision::RequiresApproval
        );
        // No allowlist: ask mode decides.
        assert_eq!(
            resolve_dm_exec_approval(true, None, "77"),
            ExecApprovalDecision::AutoApproved
        );
        assert_eq!(
            resolve_dm_exec_approval(false, None, "77"),
            ExecApprovalDecision::RequiresApproval
        );
    }

    #[test]
    fn echo_transcript_format_literal_replacement() {
        assert_eq!(
            format_echo_transcript("hello $1 world", DEFAULT_ECHO_TRANSCRIPT_FORMAT),
            "📝 \"hello $1 world\""
        );
        // Only the first placeholder is replaced; transcript content with
        // braces is preserved literally.
        assert_eq!(
            format_echo_transcript("{transcript}", "A {transcript} B {transcript}"),
            "A {transcript} B {transcript}"
        );
    }

    #[test]
    fn echo_disabled_by_default() {
        let config = Config::default();
        assert!(resolve_audio_echo_format(&config).is_none());
    }

    #[test]
    fn echo_enabled_via_tools_media_audio() {
        let mut config = Config::default();
        config.tools.media = Some(crate::config::MediaToolsConfig {
            audio: Some(serde_json::json!({ "echoTranscript": true })),
            ..Default::default()
        });
        assert_eq!(
            resolve_audio_echo_format(&config).as_deref(),
            Some(DEFAULT_ECHO_TRANSCRIPT_FORMAT)
        );

        // Custom format honored; disabled audio wins.
        config.tools.media = Some(crate::config::MediaToolsConfig {
            audio: Some(serde_json::json!({
                "echoTranscript": true,
                "echoFormat": ">> {transcript}",
                "enabled": false
            })),
            ..Default::default()
        });
        assert!(resolve_audio_echo_format(&config).is_none());
    }
}
