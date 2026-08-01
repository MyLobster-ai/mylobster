//! Synology Chat channel: incoming/outgoing webhook transport plus the
//! authorization pipeline and async delayed-reply behavior of the OpenClaw
//! `synology-chat` plugin.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `extensions/synology-chat/src/webhook-handler.ts`, `client.ts`, and
//! `security.ts`:
//!
//! - **Replies longer than 120 s** (v2026.7.1 row 88): Synology Chat's
//!   outgoing-webhook request has a hard response window (~120 s); a reply
//!   returned synchronously after that is dropped and the chat shows
//!   "Processing…" forever. The port therefore ACKs the webhook immediately
//!   (`204 No Content`) after token/authz checks and delivers the agent's
//!   final reply **asynchronously** through the *incoming* webhook URL
//!   (`deliver_delayed_reply`), which has no request-window coupling.
//! - Constant-time webhook token validation, per-IP invalid-token lockout,
//!   and a per-user rate limit (`rate_limit_per_minute`).
//! - Trigger-word stripping and input sanitization before agent delivery.
//!
//! The webhook HTTP server itself is an integration point (see
//! `start_account`); the request pipeline is implemented as the pure
//! [`SynologyChatChannel::evaluate_webhook`] decision plus testable rate
//! limiter state machines in house style.

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

// ============================================================================
// Extension extras (config.channels.extensions["synologyChat"])
// ============================================================================

/// Extras not covered by the typed `SynologyChatConfig` in
/// `config/types.rs`, read from the flattened extensions map.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SynologyChatExtras {
    /// Opt-in legacy behavior: resolve the mutable webhook `username` to a
    /// Chat API user id for reply delivery (upstream
    /// `dangerouslyAllowNameMatching`).
    pub dangerously_allow_name_matching: Option<bool>,
    /// Webhook body-read timeout override in milliseconds.
    pub body_timeout_ms: Option<u64>,
}

/// Resolves the extras from the extensions map (`synologyChat` /
/// `synology_chat` keys). The typed `SynologyChatConfig` stays the source of
/// truth for the core fields.
pub fn resolve_synology_chat_extras(config: &Config) -> Option<SynologyChatExtras> {
    for key in ["synologyChat", "synology_chat", "synology-chat"] {
        if let Some(raw) = config.channels.extensions.get(key) {
            if let Ok(parsed) = serde_json::from_value(raw.clone()) {
                return Some(parsed);
            }
        }
    }
    None
}

// ============================================================================
// Webhook payload parsing (webhook-handler.ts parsePayload)
// ============================================================================

/// Synology's outgoing-webhook response window. Synchronous replies slower
/// than this are dropped by the NAS; anything potentially slower must go
/// through the async delayed-reply path.
pub const SYNOLOGY_SYNC_REPLY_WINDOW_MS: u64 = 120_000;

/// Parsed outgoing-webhook payload (form-urlencoded or JSON).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SynologyWebhookPayload {
    pub token: String,
    pub user_id: String,
    pub username: String,
    pub text: String,
    pub trigger_word: Option<String>,
}

/// Parses a webhook body. Requires `token`, `user_id`, and `text`; returns
/// `None` when any is missing.
pub fn parse_synology_webhook_payload(
    body: &str,
    content_type: Option<&str>,
) -> Option<SynologyWebhookPayload> {
    if body.is_empty() {
        return None;
    }
    let mut fields: HashMap<String, String> = HashMap::new();
    if content_type.is_some_and(|ct| ct.contains("application/json")) {
        let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
        let object = parsed.as_object()?;
        for (key, value) in object {
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => continue,
            };
            fields.insert(key.clone(), text);
        }
    } else {
        for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
            fields.insert(key.into_owned(), value.into_owned());
        }
    }
    let required = |key: &str| fields.get(key).map(String::as_str).filter(|v| !v.is_empty());
    Some(SynologyWebhookPayload {
        token: required("token")?.to_string(),
        user_id: required("user_id")?.to_string(),
        username: fields.get("username").cloned().unwrap_or_default(),
        text: required("text")?.to_string(),
        trigger_word: fields.get("trigger_word").cloned().filter(|v| !v.is_empty()),
    })
}

/// Strips the trigger word prefix from sanitized text (upstream
/// `sanitizeSynologyWebhookText`).
pub fn strip_trigger_word(text: &str, trigger_word: Option<&str>) -> String {
    match trigger_word {
        Some(word) if !word.is_empty() && text.starts_with(word) => {
            text[word.len()..].trim().to_string()
        }
        _ => text.trim().to_string(),
    }
}

// ============================================================================
// Rate limiters (security.ts)
// ============================================================================

const INVALID_TOKEN_WINDOW_MS: u64 = 60_000;
const INVALID_TOKEN_MAX_FAILURES: u32 = 5;

/// Per-source lockout for invalid webhook tokens: once a remote IP exhausts
/// its invalid-token budget, all its requests in the window are rejected
/// with `429` (before token comparison, so brute force cannot probe).
#[derive(Debug, Default)]
pub struct InvalidTokenRateLimiter {
    entries: HashMap<String, (u32, u64)>,
}

impl InvalidTokenRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_locked(&mut self, key: &str, now_ms: u64) -> bool {
        self.sweep(now_ms);
        self.entries
            .get(key)
            .is_some_and(|(failures, _)| *failures >= INVALID_TOKEN_MAX_FAILURES)
    }

    /// Records a failed token check. Returns `true` when the source is now
    /// locked out.
    pub fn record_failure(&mut self, key: &str, now_ms: u64) -> bool {
        self.sweep(now_ms);
        let entry = self.entries.entry(key.to_string()).or_insert((0, now_ms));
        entry.0 += 1;
        entry.1 = now_ms;
        entry.0 >= INVALID_TOKEN_MAX_FAILURES
    }

    fn sweep(&mut self, now_ms: u64) {
        self.entries
            .retain(|_, (_, at)| now_ms.saturating_sub(*at) < INVALID_TOKEN_WINDOW_MS);
    }
}

/// Post-auth per-user rate limit (`rate_limit_per_minute` messages / 60 s
/// sliding window) so authenticated users are still throttled per sender.
#[derive(Debug, Default)]
pub struct UserRateLimiter {
    entries: HashMap<String, Vec<u64>>,
}

impl UserRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&mut self, user_id: &str, limit_per_minute: u32, now_ms: u64) -> bool {
        let entry = self.entries.entry(user_id.to_string()).or_default();
        // Measure each timestamp's age directly. Comparing against a
        // `now_ms.saturating_sub(60_000)` floor breaks near the clock origin:
        // the floor clamps to 0 and a strict `> 0` test then evicts every
        // request recorded at t=0, so the limiter never accumulated a full
        // window and let unlimited traffic through.
        entry.retain(|at| now_ms.saturating_sub(*at) < 60_000);
        if entry.len() >= limit_per_minute as usize {
            return false;
        }
        entry.push(now_ms);
        true
    }
}

// ============================================================================
// Webhook pipeline decision (webhook-handler.ts)
// ============================================================================

/// Authorized inbound message headed for async agent delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynologyInboundMessage {
    pub body: String,
    pub from_user_id: String,
    pub sender_name: String,
}

/// Decision for one inbound webhook request. `Accept` means: ACK the HTTP
/// request immediately with `204 No Content` (so Synology Chat never sits in
/// "Processing…" or hits its ~120 s response window), then run the agent and
/// deliver the reply via [`SynologyChatChannel::deliver_delayed_reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynologyWebhookDecision {
    Reject { status: u16, error: String },
    /// Valid request whose text became empty after sanitization — ACK with
    /// no processing.
    AckOnly,
    Accept { message: SynologyInboundMessage },
}

// ============================================================================
// Synology Chat channel
// ============================================================================

/// Synology Chat channel integration.
///
/// Communicates with Synology Chat via incoming/outgoing webhooks.
/// Outbound messages are sent as `POST` with `payload={"text":"..."}`.
/// Inbound messages are received as form-urlencoded webhook POSTs.
pub struct SynologyChatChannel {
    enabled: bool,
    token: Option<String>,
    incoming_url: Option<String>,
    bot_name: String,
    dm_policy: crate::config::DmPolicy,
    allowed_user_ids: Vec<String>,
    rate_limit_per_minute: u32,
    client: Client,
    invalid_token_limiter: Mutex<InvalidTokenRateLimiter>,
    user_rate_limiter: Mutex<UserRateLimiter>,
}

impl SynologyChatChannel {
    pub fn new(config: &Config) -> Self {
        let chat_config = config.channels.synology_chat.as_ref();
        let account = chat_config.map(|c| &c.default_account);

        let enabled = account
            .and_then(|a| a.enabled)
            .unwrap_or(false);

        let token = account.and_then(|a| a.token.clone());
        let incoming_url = account.and_then(|a| a.incoming_url.clone());
        let bot_name = account
            .and_then(|a| a.bot_name.clone())
            .unwrap_or_else(|| "MyLobster".to_string());
        let dm_policy = account
            .and_then(|a| a.dm_policy)
            .unwrap_or(crate::config::DmPolicy::Open);
        let allowed_user_ids = account
            .and_then(|a| a.allowed_user_ids.clone())
            .unwrap_or_default();
        let rate_limit_per_minute = account
            .and_then(|a| a.rate_limit_per_minute)
            .unwrap_or(30);

        let allow_insecure = account
            .and_then(|a| a.allow_insecure_ssl)
            .unwrap_or(false);

        let client = Client::builder()
            .danger_accept_invalid_certs(allow_insecure)
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            enabled,
            token,
            incoming_url,
            bot_name,
            dm_policy,
            allowed_user_ids,
            rate_limit_per_minute,
            client,
            invalid_token_limiter: Mutex::new(InvalidTokenRateLimiter::new()),
            user_rate_limiter: Mutex::new(UserRateLimiter::new()),
        }
    }

    /// Validate an inbound webhook token using constant-time comparison.
    fn validate_token(&self, received: &str) -> bool {
        match &self.token {
            Some(expected) => {
                let expected_bytes = expected.as_bytes();
                let received_bytes = received.as_bytes();
                // Constant-time comparison to prevent timing attacks
                expected_bytes.ct_eq(received_bytes).into()
            }
            None => false,
        }
    }

    /// Sanitize inbound message text by stripping dangerous patterns.
    fn sanitize_input(text: &str) -> String {
        // Strip potential injection patterns
        text.replace('\0', "")
            .replace('\r', "")
            // Limit length to prevent abuse
            .chars()
            .take(4096)
            .collect()
    }

    /// Runs the full webhook authorization pipeline on a parsed payload:
    /// invalid-token lockout → constant-time token check → DM policy /
    /// allowlist → per-user rate limit → sanitize + trigger-word strip.
    ///
    /// Callers must ACK the request immediately on `Accept` and hand the
    /// message to the agent asynchronously — the reply is delivered later
    /// via [`deliver_delayed_reply`](Self::deliver_delayed_reply), so
    /// replies slower than [`SYNOLOGY_SYNC_REPLY_WINDOW_MS`] still arrive.
    pub fn evaluate_webhook(
        &self,
        payload: &SynologyWebhookPayload,
        remote_ip: &str,
        now_ms: u64,
    ) -> SynologyWebhookDecision {
        // Once a source has exhausted its invalid-token budget, reject all
        // requests in the window.
        if self.invalid_token_limiter.lock().is_locked(remote_ip, now_ms) {
            return SynologyWebhookDecision::Reject {
                status: 429,
                error: "Rate limit exceeded".to_string(),
            };
        }

        if !self.validate_token(&payload.token) {
            if self
                .invalid_token_limiter
                .lock()
                .record_failure(remote_ip, now_ms)
            {
                return SynologyWebhookDecision::Reject {
                    status: 429,
                    error: "Rate limit exceeded".to_string(),
                };
            }
            return SynologyWebhookDecision::Reject {
                status: 401,
                error: "Invalid token".to_string(),
            };
        }

        // DM policy + allowlist.
        match self.dm_policy {
            crate::config::DmPolicy::Disabled => {
                return SynologyWebhookDecision::Reject {
                    status: 403,
                    error: "DMs are disabled".to_string(),
                };
            }
            crate::config::DmPolicy::Allowlist => {
                if self.allowed_user_ids.is_empty() {
                    return SynologyWebhookDecision::Reject {
                        status: 403,
                        error: "Allowlist is empty. Configure allowedUserIds or use \
                                dmPolicy=open with allowedUserIds=[\"*\"]."
                            .to_string(),
                    };
                }
                if !self.is_user_allowed(&payload.user_id) {
                    return SynologyWebhookDecision::Reject {
                        status: 403,
                        error: "User not authorized".to_string(),
                    };
                }
            }
            crate::config::DmPolicy::Open | crate::config::DmPolicy::Pairing => {
                // Open: everyone; explicit allowlist entries still narrow it
                // down when configured.
                if !self.allowed_user_ids.is_empty() && !self.is_user_allowed(&payload.user_id) {
                    return SynologyWebhookDecision::Reject {
                        status: 403,
                        error: "User not authorized".to_string(),
                    };
                }
            }
        }

        // Keep a separate post-auth budget so authenticated users are still
        // throttled per sender.
        if !self.user_rate_limiter.lock().check(
            &payload.user_id,
            self.rate_limit_per_minute,
            now_ms,
        ) {
            return SynologyWebhookDecision::Reject {
                status: 429,
                error: "Rate limit exceeded".to_string(),
            };
        }

        let clean = Self::sanitize_input(&payload.text);
        let body = strip_trigger_word(&clean, payload.trigger_word.as_deref());
        if body.is_empty() {
            return SynologyWebhookDecision::AckOnly;
        }

        SynologyWebhookDecision::Accept {
            message: SynologyInboundMessage {
                body,
                from_user_id: payload.user_id.clone(),
                sender_name: payload.username.clone(),
            },
        }
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_user_ids
            .iter()
            .any(|id| id == "*" || id == user_id)
    }

    /// Delivers an agent reply through the **incoming** webhook URL, outside
    /// any outgoing-webhook request window. This is the >120 s delayed-reply
    /// path: the original HTTP request was already ACKed with `204`, so the
    /// reply arrives whenever the agent finishes.
    pub async fn deliver_delayed_reply(&self, user_id: &str, reply: &str) -> Result<()> {
        self.send_message(user_id, reply).await
    }
}

#[async_trait]
impl ChannelPlugin for SynologyChatChannel {
    fn id(&self) -> &str {
        "synology_chat"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Synology Chat".to_string(),
            description: "Synology Chat NAS messaging integration via webhooks".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if self.token.is_none() {
            warn!("Synology Chat: no token configured, webhook validation will reject all messages");
        }
        if self.incoming_url.is_none() {
            warn!("Synology Chat: no incoming_url configured, outbound messages disabled");
        }

        // Integration point: the outgoing-webhook HTTP endpoint parses each
        // POST with `parse_synology_webhook_payload`, runs
        // `evaluate_webhook`, ACKs Accept/AckOnly with 204 immediately, and
        // spawns the agent turn whose reply goes out via
        // `deliver_delayed_reply` (async, immune to the ~120 s window).

        info!(
            bot_name = %self.bot_name,
            dm_policy = ?self.dm_policy,
            "Synology Chat channel started"
        );
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        info!("Synology Chat channel stopped");
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let incoming_url = self.incoming_url.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Synology Chat: no incoming_url configured for outbound messages")
        })?;

        // Build payload per Synology Chat incoming webhook format
        let user_ids: Vec<serde_json::Value> = if to.is_empty() {
            vec![]
        } else {
            to.split(',')
                .map(|id| serde_json::Value::Number(id.trim().parse::<i64>().unwrap_or(0).into()))
                .collect()
        };

        let payload = serde_json::json!({
            "text": message,
            "user_ids": user_ids,
        });

        let payload_str = serde_json::to_string(&payload)?;

        debug!(url = %incoming_url, "Sending Synology Chat message");

        let resp = self
            .client
            .post(incoming_url)
            .form(&[("payload", &payload_str)])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Synology Chat: send failed with status {}: {}",
                status,
                body
            );
        }

        Ok(())
    }
}

/// Send a standalone message via Synology Chat (used by the channel dispatch in mod.rs).
pub async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SynologyChatChannel::new(config);
    channel.send_message(to, message).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DmPolicy, SynologyChatAccountConfig, SynologyChatConfig};

    fn channel(account: SynologyChatAccountConfig) -> SynologyChatChannel {
        let mut config = Config::default();
        config.channels.synology_chat = Some(SynologyChatConfig {
            accounts: None,
            default_account: account,
        });
        SynologyChatChannel::new(&config)
    }

    fn base_account() -> SynologyChatAccountConfig {
        SynologyChatAccountConfig {
            enabled: Some(true),
            token: Some("secret-token".to_string()),
            incoming_url: Some("https://nas.example/webhook".to_string()),
            dm_policy: Some(DmPolicy::Open),
            rate_limit_per_minute: Some(3),
            ..Default::default()
        }
    }

    fn payload(token: &str, user_id: &str, text: &str) -> SynologyWebhookPayload {
        SynologyWebhookPayload {
            token: token.to_string(),
            user_id: user_id.to_string(),
            username: "alice".to_string(),
            text: text.to_string(),
            trigger_word: None,
        }
    }

    #[test]
    fn synology_parses_form_and_json_payloads() {
        let form = "token=t&user_id=42&username=alice&text=hello%20bot&trigger_word=";
        let parsed = parse_synology_webhook_payload(form, None).unwrap();
        assert_eq!(parsed.user_id, "42");
        assert_eq!(parsed.text, "hello bot");
        assert_eq!(parsed.trigger_word, None);

        let json = r#"{"token":"t","user_id":42,"username":"a","text":"hi","trigger_word":"!bot"}"#;
        let parsed = parse_synology_webhook_payload(json, Some("application/json")).unwrap();
        assert_eq!(parsed.user_id, "42");
        assert_eq!(parsed.trigger_word.as_deref(), Some("!bot"));

        // Missing required fields → None.
        assert!(parse_synology_webhook_payload("token=t&user_id=1", None).is_none());
        assert!(parse_synology_webhook_payload("", None).is_none());
    }

    #[test]
    fn synology_strips_trigger_word() {
        assert_eq!(strip_trigger_word("!bot do things", Some("!bot")), "do things");
        assert_eq!(strip_trigger_word("do things", Some("!bot")), "do things");
        assert_eq!(strip_trigger_word("  padded  ", None), "padded");
    }

    #[test]
    fn synology_accepts_valid_webhook_and_acks_empty() {
        let ch = channel(base_account());
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "hello"), "1.2.3.4", 0);
        let SynologyWebhookDecision::Accept { message } = decision else {
            panic!("expected accept");
        };
        assert_eq!(message.body, "hello");
        assert_eq!(message.from_user_id, "42");
        // Empty-after-sanitize → AckOnly (204, no processing).
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "   "), "1.2.3.4", 0);
        assert_eq!(decision, SynologyWebhookDecision::AckOnly);
    }

    #[test]
    fn synology_rejects_invalid_token_then_locks_out_source() {
        let ch = channel(base_account());
        for i in 0..4 {
            let decision = ch.evaluate_webhook(&payload("wrong", "42", "hi"), "9.9.9.9", i);
            assert_eq!(
                decision,
                SynologyWebhookDecision::Reject { status: 401, error: "Invalid token".to_string() }
            );
        }
        // 5th failure trips the lockout; further requests (even with the
        // right token) are rejected in the window.
        let decision = ch.evaluate_webhook(&payload("wrong", "42", "hi"), "9.9.9.9", 5);
        assert!(matches!(decision, SynologyWebhookDecision::Reject { status: 429, .. }));
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "9.9.9.9", 6);
        assert!(matches!(decision, SynologyWebhookDecision::Reject { status: 429, .. }));
        // Other sources are unaffected.
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "8.8.8.8", 6);
        assert!(matches!(decision, SynologyWebhookDecision::Accept { .. }));
    }

    #[test]
    fn synology_dm_policy_disabled_and_allowlist() {
        let mut account = base_account();
        account.dm_policy = Some(DmPolicy::Disabled);
        let ch = channel(account);
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", 0);
        assert_eq!(
            decision,
            SynologyWebhookDecision::Reject { status: 403, error: "DMs are disabled".to_string() }
        );

        // Allowlist with empty list → actionable 403.
        let mut account = base_account();
        account.dm_policy = Some(DmPolicy::Allowlist);
        let ch = channel(account);
        let decision = ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", 0);
        let SynologyWebhookDecision::Reject { status: 403, error } = decision else {
            panic!("expected 403");
        };
        assert!(error.contains("Allowlist is empty"));

        // Allowlist honored; wildcard allowed.
        let mut account = base_account();
        account.dm_policy = Some(DmPolicy::Allowlist);
        account.allowed_user_ids = Some(vec!["7".to_string()]);
        let ch = channel(account);
        assert!(matches!(
            ch.evaluate_webhook(&payload("secret-token", "7", "hi"), "1.1.1.1", 0),
            SynologyWebhookDecision::Accept { .. }
        ));
        assert!(matches!(
            ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", 0),
            SynologyWebhookDecision::Reject { status: 403, .. }
        ));
    }

    #[test]
    fn synology_per_user_rate_limit() {
        let ch = channel(base_account()); // limit 3/min
        for i in 0..3 {
            assert!(matches!(
                ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", i),
                SynologyWebhookDecision::Accept { .. }
            ));
        }
        assert!(matches!(
            ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", 10),
            SynologyWebhookDecision::Reject { status: 429, .. }
        ));
        // Other users unaffected; window slides after 60 s.
        assert!(matches!(
            ch.evaluate_webhook(&payload("secret-token", "43", "hi"), "1.1.1.1", 10),
            SynologyWebhookDecision::Accept { .. }
        ));
        assert!(matches!(
            ch.evaluate_webhook(&payload("secret-token", "42", "hi"), "1.1.1.1", 61_000),
            SynologyWebhookDecision::Accept { .. }
        ));
    }

    #[test]
    fn synology_extras_parse_from_extensions_map() {
        let mut config = Config::default();
        config.channels.extensions.insert(
            "synologyChat".to_string(),
            serde_json::json!({ "dangerouslyAllowNameMatching": true, "bodyTimeoutMs": 2500 }),
        );
        let extras = resolve_synology_chat_extras(&config).unwrap();
        assert_eq!(extras.dangerously_allow_name_matching, Some(true));
        assert_eq!(extras.body_timeout_ms, Some(2500));
        assert!(resolve_synology_chat_extras(&Config::default()).is_none());
    }

    #[test]
    fn synology_sync_reply_window_documented() {
        // The delayed-reply path exists because Synology drops synchronous
        // replies after ~120 s.
        assert_eq!(SYNOLOGY_SYNC_REPLY_WINDOW_MS, 120_000);
    }
}
