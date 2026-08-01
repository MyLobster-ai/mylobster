use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

// ============================================================================
// Google Chat Channel Implementation
// ============================================================================

/// Google Chat channel integration.
///
/// Supports two modes:
/// - **Webhook mode**: Posts messages to a Google Chat space via an incoming
///   webhook URL. Simple, no OAuth required, outbound-only.
/// - **Service account mode**: Uses a Google service account to call the
///   Google Chat API for full bidirectional messaging.
pub struct GoogleChatChannel {
    /// Incoming webhook URL for the Google Chat space.
    webhook_url: Option<String>,
    /// Service account JSON key (serialized) for API access.
    service_account: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl GoogleChatChannel {
    pub fn new() -> Self {
        Self {
            webhook_url: None,
            service_account: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a webhook-only Google Chat channel.
    pub fn with_webhook(webhook_url: String) -> Self {
        Self {
            webhook_url: Some(webhook_url),
            service_account: None,
            enabled: Some(true),
            client: Client::new(),
        }
    }

    /// Create a Google Chat channel with service account credentials.
    pub fn with_service_account(service_account: String) -> Self {
        Self {
            webhook_url: None,
            service_account: Some(service_account),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for GoogleChatChannel {
    fn id(&self) -> &str {
        "googlechat"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Google Chat".to_string(),
            description: "Google Chat (Workspace) channel via webhook or Chat API".to_string(),
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

        if self.webhook_url.is_some() {
            info!("Google Chat channel starting (webhook mode)");
        } else if self.service_account.is_some() {
            info!("Google Chat channel starting (service account mode)");
            // TODO: Parse the service account JSON and set up OAuth2 token refresh.
        } else {
            warn!("Google Chat channel enabled but no webhook_url or service_account configured");
        }

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Google Chat channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // Prefer webhook mode if configured.
        if let Some(webhook_url) = &self.webhook_url {
            return self.send_via_webhook(webhook_url, message).await;
        }

        // Fall back to Chat API with service account.
        if self.service_account.is_some() {
            return self.send_via_api(to, message).await;
        }

        anyhow::bail!("Google Chat: no webhook_url or service_account configured");
    }
}

impl GoogleChatChannel {
    /// Send a message via Google Chat incoming webhook.
    ///
    /// The webhook URL is space-specific; the `to` parameter is ignored
    /// in webhook mode (messages go to the space the webhook belongs to).
    async fn send_via_webhook(&self, webhook_url: &str, message: &str) -> Result<()> {
        let body = serde_json::json!({
            "text": message,
        });

        info!("Google Chat: sending message via webhook");

        let resp = self
            .client
            .post(webhook_url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Chat webhook send failed ({}): {}", status, text);
        }

        Ok(())
    }

    /// Send a message via the Google Chat API (service account auth).
    ///
    /// `to` is a Chat API space name (e.g. `spaces/AAAA...`).
    async fn send_via_api(&self, to: &str, message: &str) -> Result<()> {
        // TODO: Implement OAuth2 token acquisition from service account credentials.
        // Use `https://chat.googleapis.com/v1/{space}/messages` endpoint.

        let url = format!(
            "https://chat.googleapis.com/v1/{}/messages",
            to
        );

        let body = serde_json::json!({
            "text": message,
        });

        info!(space = %to, "Google Chat: sending message via Chat API");

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Chat API send failed ({}): {}", status, text);
        }

        Ok(())
    }
}

// ============================================================================
// Space-type classifier — new DMs route to 1:1 (not group)
//
// Port of OpenClaw `extensions/googlechat/src/targets.ts`
// (`resolveGoogleChatSpaceChatType`, `isGoogleChatGroupSpace`, v2026.7.1).
// The current `spaceType` field wins over the deprecated `type` /
// `singleUserBotDm` fields; a freshly-created DM space classified as
// `DIRECT_MESSAGE` routes to a 1:1 session instead of a group session.
// Legacy webhook payloads that omit type metadata keep their historical
// group default, while outbound routing requires an exact classification.
// ============================================================================

/// Chat type a Google Chat space resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleChatChatType {
    Direct,
    Group,
}

/// Space metadata subset used for classification (deserialized from event
/// payloads or `spaces.get` responses).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatSpaceInfo {
    pub name: Option<String>,
    /// Current API field: `SPACE`, `GROUP_CHAT`, `DIRECT_MESSAGE`.
    pub space_type: Option<String>,
    /// Deprecated field: `ROOM`, `DM`.
    #[serde(rename = "type")]
    pub legacy_type: Option<String>,
    /// Deprecated 1:1 bot-DM marker.
    pub single_user_bot_dm: Option<bool>,
}

/// Classify a space (upstream `resolveGoogleChatSpaceChatType`). Returns
/// `None` when the payload has no usable type metadata.
pub fn resolve_google_chat_space_chat_type(space: &GoogleChatSpaceInfo) -> Option<GoogleChatChatType> {
    let space_type = space
        .space_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_uppercase();
    // The current field wins when both current and deprecated are present.
    if space_type == "DIRECT_MESSAGE" {
        return Some(GoogleChatChatType::Direct);
    }
    if space_type == "SPACE" || space_type == "GROUP_CHAT" {
        return Some(GoogleChatChatType::Group);
    }
    let legacy = space
        .legacy_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_uppercase();
    if space.single_user_bot_dm == Some(true) || legacy == "DM" {
        return Some(GoogleChatChatType::Direct);
    }
    if legacy == "ROOM" {
        return Some(GoogleChatChatType::Group);
    }
    None
}

/// Group default for legacy payloads without type metadata (upstream
/// `isGoogleChatGroupSpace`).
pub fn is_google_chat_group_space(space: &GoogleChatSpaceInfo) -> bool {
    resolve_google_chat_space_chat_type(space) != Some(GoogleChatChatType::Direct)
}

// ============================================================================
// Native approval card actions + click handling
//
// Port of OpenClaw `extensions/googlechat/src/approval-card-actions.ts` +
// `approval-card-click.ts` (v2026.7.1): approval prompts render as native
// card buttons carrying a single-use random token; a CARD_CLICKED event is
// resolved against the registered binding with account / space / message
// checks and claimed exactly once (missing / in-flight duplicates are
// ignored). Actor authorization against the approvals config is the
// integration point left to the webhook handler.
// ============================================================================

/// Card action method name (upstream `GOOGLECHAT_APPROVAL_ACTION`).
pub const GOOGLECHAT_APPROVAL_ACTION: &str = "openclaw.approval";
const GOOGLECHAT_APPROVAL_ACTION_PARAM: &str = "openclaw_action";
const GOOGLECHAT_APPROVAL_TOKEN_PARAM: &str = "token";
const GOOGLECHAT_APPROVAL_ACTION_VALUE: &str = "approval";

/// Create a single-use approval token (18 random bytes, base64url).
pub fn create_google_chat_approval_token() -> String {
    let bytes: [u8; 18] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Action parameters attached to an approval button (upstream
/// `buildGoogleChatApprovalActionParameters`).
pub fn build_google_chat_approval_action_parameters(token: &str) -> Value {
    json!([
        { "key": GOOGLECHAT_APPROVAL_ACTION_PARAM, "value": GOOGLECHAT_APPROVAL_ACTION_VALUE },
        { "key": GOOGLECHAT_APPROVAL_TOKEN_PARAM, "value": token },
    ])
}

/// Build a native `cardsV2` approval card: prompt text plus one button per
/// `(label, token)` decision.
pub fn build_google_chat_approval_card(title: &str, text: &str, buttons: &[(String, String)]) -> Value {
    let button_widgets: Vec<Value> = buttons
        .iter()
        .map(|(label, token)| {
            json!({
                "text": label,
                "onClick": {
                    "action": {
                        "function": GOOGLECHAT_APPROVAL_ACTION,
                        "parameters": build_google_chat_approval_action_parameters(token),
                    }
                }
            })
        })
        .collect();
    json!({
        "cardsV2": [{
            "cardId": "openclaw-approval",
            "card": {
                "header": { "title": title },
                "sections": [{
                    "widgets": [
                        { "textParagraph": { "text": text } },
                        { "buttonList": { "buttons": button_widgets } },
                    ]
                }]
            }
        }]
    })
}

fn merge_object_params(out: &mut HashMap<String, String>, obj: Option<&Value>) {
    if let Some(Value::Object(map)) = obj {
        for (k, v) in map {
            if let Value::String(s) = v {
                out.insert(k.clone(), s.clone());
            }
        }
    }
}

/// Collect action parameters across the three event surfaces (upstream
/// `collectEventParameters`): `common.parameters`,
/// `commonEventObject.parameters`, and the `action.parameters` array.
fn collect_google_chat_event_parameters(event: &Value) -> HashMap<String, String> {
    let mut params = HashMap::new();
    merge_object_params(&mut params, event.pointer("/common/parameters"));
    merge_object_params(&mut params, event.pointer("/commonEventObject/parameters"));
    if let Some(Value::Array(items)) = event.pointer("/action/parameters") {
        for item in items {
            if let (Some(k), Some(v)) = (
                item.get("key").and_then(Value::as_str),
                item.get("value").and_then(Value::as_str),
            ) {
                params.insert(k.to_string(), v.to_string());
            }
        }
    }
    params
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Read the approval token out of a card-click event, or `None` when the
/// click is not an OpenClaw approval action (upstream
/// `readGoogleChatApprovalActionToken`).
pub fn read_google_chat_approval_action_token(event: &Value) -> Option<String> {
    let params = collect_google_chat_event_parameters(event);
    if params.get(GOOGLECHAT_APPROVAL_ACTION_PARAM).map(String::as_str)
        != Some(GOOGLECHAT_APPROVAL_ACTION_VALUE)
    {
        return None;
    }
    let action_name = non_empty_str(event.pointer("/action/actionMethodName"))
        .or_else(|| non_empty_str(event.pointer("/common/invokedFunction")))
        .or_else(|| non_empty_str(event.pointer("/commonEventObject/invokedFunction")));
    if let Some(name) = action_name {
        if name != GOOGLECHAT_APPROVAL_ACTION && !name.starts_with("https://") {
            return None;
        }
    }
    params
        .get(GOOGLECHAT_APPROVAL_TOKEN_PARAM)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A registered approval card button binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleChatApprovalBinding {
    pub token: String,
    pub account_id: String,
    pub approval_id: String,
    /// Decision this specific button carries (e.g. `allow-once`).
    pub decision: String,
    /// Decisions still permitted for the approval.
    pub allowed_decisions: Vec<String>,
    pub space_name: String,
    pub message_name: String,
    pub thread_name: Option<String>,
    pub expires_at_ms: u64,
}

/// Claim outcome (upstream `GoogleChatApprovalCardClaim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleChatApprovalClaim {
    Claimed(GoogleChatApprovalBinding),
    Missing,
    InFlight,
}

/// Token-keyed binding store with single-claim semantics.
#[derive(Default)]
pub struct GoogleChatApprovalCardBindings {
    bindings: Mutex<HashMap<String, GoogleChatApprovalBinding>>,
    in_flight: Mutex<HashSet<String>>,
}

impl GoogleChatApprovalCardBindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a binding; already-expired bindings are rejected.
    pub fn register(&self, binding: GoogleChatApprovalBinding, now_ms: u64) -> bool {
        if binding.expires_at_ms <= now_ms {
            return false;
        }
        self.bindings.lock().insert(binding.token.clone(), binding);
        true
    }

    /// Look up a live binding (expired entries are dropped on read).
    pub fn get(&self, token: &str, now_ms: u64) -> Option<GoogleChatApprovalBinding> {
        let mut map = self.bindings.lock();
        match map.get(token) {
            Some(b) if b.expires_at_ms > now_ms => Some(b.clone()),
            Some(_) => {
                map.remove(token);
                None
            }
            None => None,
        }
    }

    /// Claim a binding for resolution: a second concurrent claim observes
    /// `InFlight`, a consumed/unknown token observes `Missing`.
    pub fn claim(&self, token: &str) -> GoogleChatApprovalClaim {
        let mut in_flight = self.in_flight.lock();
        if in_flight.contains(token) {
            return GoogleChatApprovalClaim::InFlight;
        }
        match self.bindings.lock().get(token) {
            Some(b) => {
                in_flight.insert(token.to_string());
                GoogleChatApprovalClaim::Claimed(b.clone())
            }
            None => GoogleChatApprovalClaim::Missing,
        }
    }

    /// Release a claim after a failed resolve so a retry can claim again.
    pub fn release(&self, token: &str) {
        self.in_flight.lock().remove(token);
    }

    /// Complete a claim: the token is consumed for good.
    pub fn complete(&self, token: &str) {
        self.bindings.lock().remove(token);
        self.in_flight.lock().remove(token);
    }
}

/// Outcome of routing a webhook event through approval-click handling
/// (upstream `maybeHandleGoogleChatApprovalCardClick`, minus the
/// gateway-resolution side effect which stays in the webhook handler).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleChatApprovalClickOutcome {
    /// Not a CARD_CLICKED approval event — continue normal processing.
    NotApprovalClick,
    /// An approval click that must be swallowed, with the ignore reason.
    Ignored(&'static str),
    /// Claimed: resolve the approval, then `complete` (or `release` on
    /// error) the token.
    Claimed(GoogleChatApprovalBinding),
}

/// Validate and claim an approval card click.
pub fn handle_google_chat_approval_card_click(
    event: &Value,
    account_id: &str,
    bindings: &GoogleChatApprovalCardBindings,
    now_ms: u64,
) -> GoogleChatApprovalClickOutcome {
    let event_type = non_empty_str(event.get("type")).or_else(|| non_empty_str(event.get("eventType")));
    if event_type != Some("CARD_CLICKED") {
        return GoogleChatApprovalClickOutcome::NotApprovalClick;
    }
    let token = match read_google_chat_approval_action_token(event) {
        Some(t) => t,
        None => return GoogleChatApprovalClickOutcome::NotApprovalClick,
    };
    let binding = match bindings.get(&token, now_ms) {
        Some(b) => b,
        None => return GoogleChatApprovalClickOutcome::Ignored("unknown or expired card token"),
    };
    if binding.account_id != account_id {
        return GoogleChatApprovalClickOutcome::Ignored("card token account mismatch");
    }
    if non_empty_str(event.pointer("/space/name")) != Some(binding.space_name.as_str()) {
        return GoogleChatApprovalClickOutcome::Ignored("card token space mismatch");
    }
    if let Some(message_name) = non_empty_str(event.pointer("/message/name")) {
        if message_name != binding.message_name {
            return GoogleChatApprovalClickOutcome::Ignored("card token message mismatch");
        }
    }
    if !binding.allowed_decisions.contains(&binding.decision) {
        return GoogleChatApprovalClickOutcome::Ignored("card token decision is no longer allowed");
    }
    match bindings.claim(&token) {
        GoogleChatApprovalClaim::Claimed(b) => GoogleChatApprovalClickOutcome::Claimed(b),
        GoogleChatApprovalClaim::Missing => {
            GoogleChatApprovalClickOutcome::Ignored("card token already consumed")
        }
        GoogleChatApprovalClaim::InFlight => {
            GoogleChatApprovalClickOutcome::Ignored("card token resolve already in flight")
        }
    }
}

// ============================================================================
// Thread metadata from send responses + message-tool thread replies
//
// Port of OpenClaw `extensions/googlechat/src/api.ts` (`sendGoogleChatMessage`,
// v2026.7.1): send responses return `{ name, thread.name }`; the thread name
// is retained per space so message-tool replies land in the same thread, and
// threaded sends carry `messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD`.
// ============================================================================

/// Parsed send-response metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoogleChatSendReceipt {
    pub message_name: Option<String>,
    pub thread_name: Option<String>,
}

/// Extract message + thread names from a Chat API send response.
pub fn parse_google_chat_send_response(response: &Value) -> GoogleChatSendReceipt {
    GoogleChatSendReceipt {
        message_name: non_empty_str(response.get("name")).map(str::to_string),
        thread_name: non_empty_str(response.pointer("/thread/name")).map(str::to_string),
    }
}

/// Build the send body plus query parameters. A thread reply sets
/// `thread.name` and asks the API to fall back to a new thread when the
/// referenced thread is gone.
pub fn build_google_chat_send_body(
    text: Option<&str>,
    thread: Option<&str>,
) -> (Value, Vec<(&'static str, String)>) {
    let mut body = serde_json::Map::new();
    if let Some(t) = text.filter(|t| !t.is_empty()) {
        body.insert("text".to_string(), Value::String(t.to_string()));
    }
    let mut query = Vec::new();
    if let Some(thread_name) = thread.map(str::trim).filter(|t| !t.is_empty()) {
        body.insert("thread".to_string(), json!({ "name": thread_name }));
        query.push((
            "messageReplyOption",
            "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD".to_string(),
        ));
    }
    (Value::Object(body), query)
}

/// Per-space registry of the latest known thread, fed from send receipts,
/// so message-tool replies stay in-thread.
#[derive(Default)]
pub struct GoogleChatThreadRegistry {
    threads: Mutex<HashMap<String, String>>,
}

impl GoogleChatThreadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain thread metadata from a send receipt.
    pub fn record(&self, space: &str, receipt: &GoogleChatSendReceipt) {
        if let Some(thread) = receipt.thread_name.as_deref().filter(|t| !t.is_empty()) {
            self.threads.lock().insert(space.to_string(), thread.to_string());
        }
    }

    /// Thread to target for a reply in `space`, if one is known.
    pub fn reply_thread_for(&self, space: &str) -> Option<String> {
        self.threads.lock().get(space).cloned()
    }
}

// ============================================================================
// Auth-transport isolation + header normalization + webhook rate-limit
//
// Ports of `extensions/googlechat/src/auth.ts` (per-account token cache,
// `MAX_AUTH_CACHE_SIZE`) and `monitor-routing.ts` /
// `plugin-sdk/webhook-memory-guards.ts` (`WEBHOOK_RATE_LIMIT_DEFAULTS`,
// fixed-window limiter keyed by `path:clientIp`) at v2026.7.1.
// ============================================================================

/// Size cap preventing unbounded growth in long-running deployments
/// (upstream `MAX_AUTH_CACHE_SIZE`, #4948).
pub const GOOGLECHAT_MAX_AUTH_CACHE_SIZE: usize = 32;

/// Per-account access-token cache keyed by a credential fingerprint, so an
/// account whose credentials rotate never reuses another account's (or its
/// own stale) transport. FIFO-evicts the oldest account past the cap.
#[derive(Default)]
pub struct GoogleChatAuthTokenCache {
    /// (account_id, credential_key, token) in insertion order.
    entries: Mutex<Vec<(String, String, String)>>,
}

impl GoogleChatAuthTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cached token for the account, only when the credential key still
    /// matches (auth-transport isolation).
    pub fn get(&self, account_id: &str, credential_key: &str) -> Option<String> {
        self.entries
            .lock()
            .iter()
            .find(|(a, k, _)| a == account_id && k == credential_key)
            .map(|(_, _, t)| t.clone())
    }

    /// Insert/replace the account's token; a changed credential key drops
    /// the previous entry.
    pub fn insert(&self, account_id: &str, credential_key: &str, token: String) {
        let mut entries = self.entries.lock();
        entries.retain(|(a, _, _)| a != account_id);
        entries.push((account_id.to_string(), credential_key.to_string(), token));
        while entries.len() > GOOGLECHAT_MAX_AUTH_CACHE_SIZE {
            entries.remove(0);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Normalize outbound Chat API headers: lowercase names, drop any
/// caller-supplied auth headers (exactly one `authorization` is emitted),
/// and default the JSON content type.
pub fn normalize_google_chat_headers(
    token: &str,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    headers.push(("authorization".to_string(), format!("Bearer {}", token)));
    let mut saw_content_type = false;
    for (name, value) in extra {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty()
            || lower == "authorization"
            || lower == "proxy-authorization"
            || lower.starts_with("x-goog-authenticated")
        {
            continue;
        }
        if lower == "content-type" {
            saw_content_type = true;
        }
        headers.push((lower, value.clone()));
    }
    if !saw_content_type {
        headers.push((
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ));
    }
    headers
}

/// Webhook rate-limit defaults (upstream `WEBHOOK_RATE_LIMIT_DEFAULTS`).
pub const GOOGLECHAT_WEBHOOK_RATE_LIMIT_WINDOW_MS: u64 = 60_000;
pub const GOOGLECHAT_WEBHOOK_RATE_LIMIT_MAX_REQUESTS: u32 = 120;
pub const GOOGLECHAT_WEBHOOK_RATE_LIMIT_MAX_TRACKED_KEYS: usize = 4_096;

/// Fixed-window rate limiter keyed by `path:clientIp` (upstream
/// `createFixedWindowRateLimiter`).
pub struct FixedWindowRateLimiter {
    window_ms: u64,
    max_requests: u32,
    max_tracked_keys: usize,
    windows: Mutex<HashMap<String, (u64, u32)>>,
}

impl FixedWindowRateLimiter {
    pub fn new(window_ms: u64, max_requests: u32, max_tracked_keys: usize) -> Self {
        Self {
            window_ms: window_ms.max(1),
            max_requests,
            max_tracked_keys: max_tracked_keys.max(1),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Limiter with the Google Chat webhook defaults.
    pub fn with_googlechat_defaults() -> Self {
        Self::new(
            GOOGLECHAT_WEBHOOK_RATE_LIMIT_WINDOW_MS,
            GOOGLECHAT_WEBHOOK_RATE_LIMIT_MAX_REQUESTS,
            GOOGLECHAT_WEBHOOK_RATE_LIMIT_MAX_TRACKED_KEYS,
        )
    }

    /// Register a request; `true` when allowed. Untracked keys past the
    /// cardinality cap (after expired-window pruning) fail closed.
    pub fn check(&self, key: &str, now_ms: u64) -> bool {
        let mut windows = self.windows.lock();
        if let Some((start, count)) = windows.get_mut(key) {
            if now_ms.saturating_sub(*start) >= self.window_ms {
                *start = now_ms;
                *count = 1;
                return true;
            }
            if *count >= self.max_requests {
                return false;
            }
            *count += 1;
            return true;
        }
        if windows.len() >= self.max_tracked_keys {
            let window_ms = self.window_ms;
            windows.retain(|_, (start, _)| now_ms.saturating_sub(*start) < window_ms);
            if windows.len() >= self.max_tracked_keys {
                return false;
            }
        }
        windows.insert(key.to_string(), (now_ms, 1));
        true
    }
}

// ============================================================================
// Hidden internal failure banners
//
// Port of the assistant-visible-text failure-banner stripping applied to
// Google Chat outbound delivery (upstream #95084 / #90684, regexes from
// `src/shared/text/assistant-visible-text.ts` at v2026.7.1): internal
// tool-trace scaffolding and misleading `⚠️ 🛠️ … (agent) failed` banners
// are dropped line-by-line before delivery while ordinary assistant prose
// passes through unchanged.
// ============================================================================

static INTERNAL_TRACE_LINE_QUICK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:📊|🛠️|📖|📝|🔍|🔎|⚙️|tool[-_ ]?call|tool[-_ ]?result|function[-_ ]?call)")
        .expect("valid regex")
});
static INTERNAL_TRACE_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:>\s*)?(?:⚠️\s*)?(?:📊|🛠️|📖|📝|🔍|🔎|⚙️)\s*(?:Session Status|Exec|Read|Edit|Write|Patch|Search|Open|Click|Find|Screenshot|Update Plan|Tool Call|Tool Result|Function Call|Shell|Command)\s*:")
        .expect("valid regex")
});
static INTERNAL_COMPACT_FAILURE_TRACE_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:>\s*)?⚠️\s*🛠️\s+\S[\s\S]*\s+\(agent\)`{0,2}\s+failed(?:\s*:.*)?\s*$")
        .expect("valid regex")
});
static INTERNAL_COMPACT_COMMAND_TRACE_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:>\s*)?🛠️\s*(?:(?:(?:elevated|pty)\b\s*(?:·|,)\s*)+)?(?:`{1,2}\s*\S|(?:run|check|fetch|pull|push|view|show|list|switch|create|merge|rebase|stage|restore|reset|stash|search|find|print|copy|move|remove|install|start|cd|git|pnpm|npm|yarn|bun|node|python|python3|bash|sh)\b)")
        .expect("valid regex")
});
static INTERNAL_CHANNEL_TRACE_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:>\s*)?(?:tool[-_ ]?call|tool[-_ ]?result|function[-_ ]?call)\s*[:=]")
        .expect("valid regex")
});

fn is_internal_trace_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !INTERNAL_TRACE_LINE_QUICK_RE.is_match(trimmed) {
        return false;
    }
    INTERNAL_TRACE_LINE_RE.is_match(trimmed)
        || INTERNAL_COMPACT_FAILURE_TRACE_LINE_RE.is_match(trimmed)
        || INTERNAL_COMPACT_COMMAND_TRACE_LINE_RE.is_match(trimmed)
        || INTERNAL_CHANNEL_TRACE_LINE_RE.is_match(trimmed)
}

/// Strip internal failure banners / tool-trace lines from outbound text so
/// internal errors never leak into the chat. Keeps prose, collapses the
/// blank runs left behind, trims the result.
pub fn sanitize_google_chat_outbound_text(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut last_blank = false;
    for line in text.lines() {
        if is_internal_trace_line(line) {
            continue;
        }
        let blank = line.trim().is_empty();
        if blank && (last_blank || out.is_empty()) {
            continue;
        }
        out.push(line);
        last_blank = blank;
    }
    while matches!(out.last(), Some(l) if l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(space_type: Option<&str>, legacy: Option<&str>, dm: Option<bool>) -> GoogleChatSpaceInfo {
        GoogleChatSpaceInfo {
            name: Some("spaces/AAA".to_string()),
            space_type: space_type.map(str::to_string),
            legacy_type: legacy.map(str::to_string),
            single_user_bot_dm: dm,
        }
    }

    // ---- space classifier ----

    #[test]
    fn new_dms_classify_direct_not_group() {
        let dm = space(Some("DIRECT_MESSAGE"), None, None);
        assert_eq!(
            resolve_google_chat_space_chat_type(&dm),
            Some(GoogleChatChatType::Direct)
        );
        assert!(!is_google_chat_group_space(&dm));
    }

    #[test]
    fn current_field_wins_over_deprecated_fields() {
        // spaceType says group even though legacy fields say DM.
        let s = space(Some("SPACE"), Some("DM"), Some(true));
        assert_eq!(
            resolve_google_chat_space_chat_type(&s),
            Some(GoogleChatChatType::Group)
        );
        assert_eq!(
            resolve_google_chat_space_chat_type(&space(Some("GROUP_CHAT"), None, None)),
            Some(GoogleChatChatType::Group)
        );
    }

    #[test]
    fn deprecated_fields_classify_when_current_absent() {
        assert_eq!(
            resolve_google_chat_space_chat_type(&space(None, Some("DM"), None)),
            Some(GoogleChatChatType::Direct)
        );
        assert_eq!(
            resolve_google_chat_space_chat_type(&space(None, None, Some(true))),
            Some(GoogleChatChatType::Direct)
        );
        assert_eq!(
            resolve_google_chat_space_chat_type(&space(None, Some("ROOM"), None)),
            Some(GoogleChatChatType::Group)
        );
    }

    #[test]
    fn missing_metadata_keeps_legacy_group_default() {
        let s = space(None, None, None);
        assert_eq!(resolve_google_chat_space_chat_type(&s), None);
        assert!(is_google_chat_group_space(&s));
    }

    // ---- approval cards ----

    fn binding(token: &str) -> GoogleChatApprovalBinding {
        GoogleChatApprovalBinding {
            token: token.to_string(),
            account_id: "default".to_string(),
            approval_id: "appr-1".to_string(),
            decision: "allow-once".to_string(),
            allowed_decisions: vec!["allow-once".to_string(), "deny".to_string()],
            space_name: "spaces/AAA".to_string(),
            message_name: "spaces/AAA/messages/M1".to_string(),
            thread_name: None,
            expires_at_ms: 10_000,
        }
    }

    fn click_event(token: &str) -> Value {
        json!({
            "type": "CARD_CLICKED",
            "space": { "name": "spaces/AAA" },
            "message": { "name": "spaces/AAA/messages/M1" },
            "action": {
                "actionMethodName": GOOGLECHAT_APPROVAL_ACTION,
                "parameters": [
                    { "key": "openclaw_action", "value": "approval" },
                    { "key": "token", "value": token },
                ]
            },
            "user": { "name": "users/123" },
        })
    }

    #[test]
    fn approval_token_is_urlsafe_and_unique() {
        let a = create_google_chat_approval_token();
        let b = create_google_chat_approval_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 24); // 18 bytes → 24 base64url chars, no padding
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn card_payload_carries_action_and_tokens() {
        let card = build_google_chat_approval_card(
            "Approval required",
            "Run `rm -rf /tmp/x`?",
            &[("Allow once".to_string(), "tok1".to_string()),
              ("Deny".to_string(), "tok2".to_string())],
        );
        let buttons = card
            .pointer("/cardsV2/0/card/sections/0/widgets/1/buttonList/buttons")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(
            buttons[0].pointer("/onClick/action/function").unwrap(),
            GOOGLECHAT_APPROVAL_ACTION
        );
        let event = json!({
            "type": "CARD_CLICKED",
            "action": {
                "actionMethodName": GOOGLECHAT_APPROVAL_ACTION,
                "parameters": buttons[1].pointer("/onClick/action/parameters").unwrap(),
            }
        });
        assert_eq!(read_google_chat_approval_action_token(&event).unwrap(), "tok2");
    }

    #[test]
    fn token_reads_from_common_event_object_and_rejects_foreign_actions() {
        let event = json!({
            "commonEventObject": {
                "invokedFunction": GOOGLECHAT_APPROVAL_ACTION,
                "parameters": { "openclaw_action": "approval", "token": "t1" },
            }
        });
        assert_eq!(read_google_chat_approval_action_token(&event).unwrap(), "t1");

        let foreign = json!({
            "action": {
                "actionMethodName": "some.other.action",
                "parameters": [
                    { "key": "openclaw_action", "value": "approval" },
                    { "key": "token", "value": "t2" },
                ]
            }
        });
        assert!(read_google_chat_approval_action_token(&foreign).is_none());
        assert!(read_google_chat_approval_action_token(&json!({})).is_none());
    }

    #[test]
    fn click_claims_binding_exactly_once() {
        let store = GoogleChatApprovalCardBindings::new();
        assert!(store.register(binding("tok"), 0));
        let ev = click_event("tok");
        match handle_google_chat_approval_card_click(&ev, "default", &store, 1) {
            GoogleChatApprovalClickOutcome::Claimed(b) => assert_eq!(b.approval_id, "appr-1"),
            other => panic!("expected Claimed, got {:?}", other),
        }
        // Second click while resolving: in-flight.
        assert_eq!(
            handle_google_chat_approval_card_click(&ev, "default", &store, 1),
            GoogleChatApprovalClickOutcome::Ignored("card token resolve already in flight")
        );
        store.complete("tok");
        assert_eq!(
            handle_google_chat_approval_card_click(&ev, "default", &store, 1),
            GoogleChatApprovalClickOutcome::Ignored("unknown or expired card token")
        );
    }

    #[test]
    fn click_validates_account_space_message_and_expiry() {
        let store = GoogleChatApprovalCardBindings::new();
        store.register(binding("tok"), 0);
        let ev = click_event("tok");
        assert_eq!(
            handle_google_chat_approval_card_click(&ev, "other-account", &store, 1),
            GoogleChatApprovalClickOutcome::Ignored("card token account mismatch")
        );
        let mut wrong_space = click_event("tok");
        wrong_space["space"]["name"] = json!("spaces/BBB");
        assert_eq!(
            handle_google_chat_approval_card_click(&wrong_space, "default", &store, 1),
            GoogleChatApprovalClickOutcome::Ignored("card token space mismatch")
        );
        let mut wrong_msg = click_event("tok");
        wrong_msg["message"]["name"] = json!("spaces/AAA/messages/M2");
        assert_eq!(
            handle_google_chat_approval_card_click(&wrong_msg, "default", &store, 1),
            GoogleChatApprovalClickOutcome::Ignored("card token message mismatch")
        );
        // Expired binding.
        assert_eq!(
            handle_google_chat_approval_card_click(&click_event("tok"), "default", &store, 20_000),
            GoogleChatApprovalClickOutcome::Ignored("unknown or expired card token")
        );
        // Non-card events pass through.
        assert_eq!(
            handle_google_chat_approval_card_click(&json!({"type": "MESSAGE"}), "default", &store, 1),
            GoogleChatApprovalClickOutcome::NotApprovalClick
        );
    }

    #[test]
    fn release_allows_reclaim_after_failed_resolve() {
        let store = GoogleChatApprovalCardBindings::new();
        store.register(binding("tok"), 0);
        assert!(matches!(store.claim("tok"), GoogleChatApprovalClaim::Claimed(_)));
        store.release("tok");
        assert!(matches!(store.claim("tok"), GoogleChatApprovalClaim::Claimed(_)));
    }

    #[test]
    fn expired_binding_rejected_at_registration() {
        let store = GoogleChatApprovalCardBindings::new();
        assert!(!store.register(binding("tok"), 10_000));
    }

    // ---- thread metadata ----

    #[test]
    fn send_response_thread_metadata_is_retained() {
        let resp = json!({
            "name": "spaces/AAA/messages/M9",
            "thread": { "name": "spaces/AAA/threads/T3" },
        });
        let receipt = parse_google_chat_send_response(&resp);
        assert_eq!(receipt.message_name.as_deref(), Some("spaces/AAA/messages/M9"));
        assert_eq!(receipt.thread_name.as_deref(), Some("spaces/AAA/threads/T3"));

        let registry = GoogleChatThreadRegistry::new();
        registry.record("spaces/AAA", &receipt);
        assert_eq!(
            registry.reply_thread_for("spaces/AAA").as_deref(),
            Some("spaces/AAA/threads/T3")
        );
        assert!(registry.reply_thread_for("spaces/BBB").is_none());
    }

    #[test]
    fn threaded_send_body_sets_thread_and_fallback_reply_option() {
        let (body, query) = build_google_chat_send_body(Some("hi"), Some("spaces/AAA/threads/T3"));
        assert_eq!(body["text"], "hi");
        assert_eq!(body["thread"]["name"], "spaces/AAA/threads/T3");
        assert_eq!(
            query,
            vec![("messageReplyOption", "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD".to_string())]
        );
        let (body, query) = build_google_chat_send_body(Some("hi"), None);
        assert!(body.get("thread").is_none());
        assert!(query.is_empty());
    }

    // ---- auth transport isolation + headers ----

    #[test]
    fn auth_cache_isolates_accounts_and_credential_rotations() {
        let cache = GoogleChatAuthTokenCache::new();
        cache.insert("a", "cred-1", "tok-a".to_string());
        cache.insert("b", "cred-2", "tok-b".to_string());
        assert_eq!(cache.get("a", "cred-1").as_deref(), Some("tok-a"));
        assert_eq!(cache.get("b", "cred-2").as_deref(), Some("tok-b"));
        // A rotated credential key must miss (no cross-credential reuse).
        assert!(cache.get("a", "cred-9").is_none());
        cache.insert("a", "cred-9", "tok-a2".to_string());
        assert_eq!(cache.get("a", "cred-9").as_deref(), Some("tok-a2"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn auth_cache_evicts_oldest_past_cap() {
        let cache = GoogleChatAuthTokenCache::new();
        for i in 0..(GOOGLECHAT_MAX_AUTH_CACHE_SIZE + 4) {
            cache.insert(&format!("acct-{}", i), "k", format!("t{}", i));
        }
        assert_eq!(cache.len(), GOOGLECHAT_MAX_AUTH_CACHE_SIZE);
        assert!(cache.get("acct-0", "k").is_none());
        assert!(cache
            .get(&format!("acct-{}", GOOGLECHAT_MAX_AUTH_CACHE_SIZE + 3), "k")
            .is_some());
    }

    #[test]
    fn header_normalization_dedupes_auth_and_lowercases() {
        let extra = vec![
            ("Authorization".to_string(), "Bearer stale".to_string()),
            ("X-Custom".to_string(), "1".to_string()),
            ("Proxy-Authorization".to_string(), "nope".to_string()),
        ];
        let headers = normalize_google_chat_headers("fresh", &extra);
        let auth: Vec<_> = headers.iter().filter(|(n, _)| n == "authorization").collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1, "Bearer fresh");
        assert!(headers.iter().any(|(n, v)| n == "x-custom" && v == "1"));
        assert!(!headers.iter().any(|(n, _)| n == "proxy-authorization"));
        assert!(headers
            .iter()
            .any(|(n, v)| n == "content-type" && v.starts_with("application/json")));
    }

    // ---- webhook rate limit ----

    #[test]
    fn rate_limiter_enforces_fixed_window() {
        let limiter = FixedWindowRateLimiter::new(1_000, 3, 16);
        assert!(limiter.check("k", 0));
        assert!(limiter.check("k", 100));
        assert!(limiter.check("k", 200));
        assert!(!limiter.check("k", 300)); // over budget inside the window
        assert!(limiter.check("k", 1_000)); // new window
        assert!(limiter.check("other", 300)); // independent key
    }

    #[test]
    fn rate_limiter_defaults_match_upstream() {
        let limiter = FixedWindowRateLimiter::with_googlechat_defaults();
        assert_eq!(limiter.window_ms, 60_000);
        assert_eq!(limiter.max_requests, 120);
        assert_eq!(limiter.max_tracked_keys, 4_096);
    }

    #[test]
    fn rate_limiter_prunes_expired_keys_at_cardinality_cap() {
        let limiter = FixedWindowRateLimiter::new(1_000, 5, 2);
        assert!(limiter.check("a", 0));
        assert!(limiter.check("b", 0));
        // Cap reached and both windows still live → new key fails closed.
        assert!(!limiter.check("c", 500));
        // After the windows expire, pruning frees room.
        assert!(limiter.check("c", 2_000));
    }

    // ---- failure banner sanitization ----

    #[test]
    fn strips_internal_failure_banner_keeps_answer() {
        let text = "Done.\n⚠️ 🛠️ `search repos (agent)` failed";
        assert_eq!(sanitize_google_chat_outbound_text(text), "Done.");
    }

    #[test]
    fn strips_tool_trace_and_command_lines() {
        let text = "🛠️ Exec: ls -la\nHere are your files.\ntool_call: {\"name\":\"x\"}\n🛠️ `git status`";
        assert_eq!(sanitize_google_chat_outbound_text(text), "Here are your files.");
    }

    #[test]
    fn preserves_ordinary_prose_and_collapses_blanks() {
        let prose = "The pipeline has 3 open deals.";
        assert_eq!(sanitize_google_chat_outbound_text(prose), prose);
        let mixed = "Line one.\n\n⚠️ 🛠️ `web fetch (agent)` failed: boom\n\nLine two.";
        assert_eq!(sanitize_google_chat_outbound_text(mixed), "Line one.\n\nLine two.");
        // A wrench emoji inside prose is not a trace line.
        let emoji_prose = "I fixed it with a 🛠️ yesterday.";
        assert_eq!(sanitize_google_chat_outbound_text(emoji_prose), emoji_prose);
    }
}
