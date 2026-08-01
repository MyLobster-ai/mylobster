//! Nextcloud Talk channel: OCS API transport plus the webhook-event
//! classification and bot-capability preflight behavior of the OpenClaw
//! `nextcloud-talk` plugin.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `extensions/nextcloud-talk/src/monitor.ts` (webhook envelope tolerance)
//! and `bot-preflight.ts` (`response` feature probe):
//!
//! - File-share and room-lifecycle system events are **tolerated**: the
//!   webhook body is first parsed against a loose envelope (`type`,
//!   `object.type`); anything that is not a `Create` of a `Note` is ignored
//!   with a success response instead of being rejected as malformed. Only
//!   actual chat messages go through the strict payload schema.
//! - The bot must carry the Talk `response` feature (bitmask bit `2`) or
//!   outbound replies fail; the preflight probes
//!   `GET /ocs/v2.php/apps/spreed/api/v1/bot/admin`, matches the bot by
//!   webhook URL, and reports an actionable `occ talk:bot:state` hint when
//!   the feature is missing.
//!
//! The live webhook HTTP server is an integration point (see
//! `start_account`); classification and probe evaluation are pure functions
//! with unit tests in house style.

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ============================================================================
// Extension configuration (config.channels.extensions["nextcloud"])
// ============================================================================

/// Nextcloud Talk channel configuration read from the flattened
/// `channels.extensions` map (keys `nextcloud`, `nextcloudTalk`, or
/// `nextcloud-talk` are accepted; there is no typed `ChannelsConfig` entry).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NextcloudExtensionConfig {
    pub enabled: Option<bool>,
    /// Nextcloud base URL (e.g. `https://cloud.example.com`).
    pub base_url: Option<String>,
    /// Talk bot shared secret used for webhook signatures.
    pub bot_secret: Option<String>,
    /// Public URL the Talk bot posts webhooks to (used to match the bot in
    /// the admin listing during the `response`-feature preflight).
    pub webhook_public_url: Option<String>,
    /// Admin API credentials for the bot preflight (basic auth).
    pub api_user: Option<String>,
    pub api_password: Option<String>,
}

/// Resolves the Nextcloud Talk extension config from the extensions map.
pub fn resolve_nextcloud_extension_config(config: &Config) -> Option<NextcloudExtensionConfig> {
    for key in ["nextcloud", "nextcloudTalk", "nextcloud-talk"] {
        if let Some(raw) = config.channels.extensions.get(key) {
            if let Ok(parsed) = serde_json::from_value(raw.clone()) {
                return Some(parsed);
            }
        }
    }
    None
}

// ============================================================================
// Webhook event classification (monitor.ts)
// ============================================================================

/// A chat message extracted from a Talk `Create`/`Note` webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkInboundMessage {
    pub message_id: String,
    pub room_token: String,
    pub room_name: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub media_type: String,
    /// The payload doesn't indicate DM vs room; marked as group and refined
    /// downstream (upstream `payloadToInboundMessage`).
    pub is_group_chat: bool,
}

/// Classification of a Talk webhook body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TalkWebhookDecision {
    /// A user chat message to process.
    Message(TalkInboundMessage),
    /// A tolerated non-message event (file share, reaction, room lifecycle,
    /// system activity): acknowledged with success, never treated as
    /// malformed.
    Ignored,
    /// The body is not valid JSON or claims to be a message but fails the
    /// strict payload schema → `400 Invalid payload format`.
    Invalid,
}

fn non_empty_str(value: Option<&serde_json::Value>) -> Option<&str> {
    value.and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Classifies a Talk webhook body.
///
/// The loose envelope check runs first so file-share/lifecycle system events
/// (e.g. `Activity` types, `File` objects) are ignored instead of rejected;
/// only `Create` + `Note` bodies must satisfy the strict schema
/// (`actor: Person`, `object: Note`, `target: Collection`).
pub fn classify_talk_webhook(body: &str) -> TalkWebhookDecision {
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(body) else {
        return TalkWebhookDecision::Invalid;
    };
    let Some(event_type) = non_empty_str(envelope.get("type")) else {
        return TalkWebhookDecision::Invalid;
    };
    if event_type != "Create" {
        return TalkWebhookDecision::Ignored;
    }
    if let Some(object_type) = non_empty_str(envelope.get("object").and_then(|o| o.get("type"))) {
        if object_type != "Note" {
            return TalkWebhookDecision::Ignored;
        }
    }

    // Strict payload schema for actual chat messages.
    let actor = envelope.get("actor");
    let object = envelope.get("object");
    let target = envelope.get("target");
    let actor_ok = non_empty_str(actor.and_then(|a| a.get("type"))) == Some("Person");
    let target_ok = non_empty_str(target.and_then(|t| t.get("type"))) == Some("Collection");
    let (Some(actor), Some(object), Some(target)) = (actor, object, target) else {
        return TalkWebhookDecision::Invalid;
    };
    let (Some(actor_id), Some(object_id), Some(target_id)) = (
        non_empty_str(actor.get("id")),
        non_empty_str(object.get("id")),
        non_empty_str(target.get("id")),
    ) else {
        return TalkWebhookDecision::Invalid;
    };
    if !actor_ok || !target_ok {
        return TalkWebhookDecision::Invalid;
    }
    let content = object.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let name = object.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let text = if content.is_empty() { name } else { content };
    let media_type = object
        .get("mediaType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("text/plain");

    TalkWebhookDecision::Message(TalkInboundMessage {
        message_id: object_id.to_string(),
        room_token: target_id.to_string(),
        room_name: target
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sender_id: actor_id.to_string(),
        sender_name: actor
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text: text.to_string(),
        media_type: media_type.to_string(),
        is_group_chat: true,
    })
}

// ============================================================================
// Bot `response` feature preflight (bot-preflight.ts)
// ============================================================================

/// Talk bot feature bit required for the bot to post replies.
pub const BOT_FEATURE_RESPONSE: u64 = 2;

/// Outcome of the `response`-feature preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotResponseFeatureProbe {
    /// Bot found and it carries the `response` feature.
    Ok {
        bot_id: String,
        bot_name: String,
        features: u64,
    },
    /// Probe skipped because required config (base URL, webhook URL, or API
    /// credentials) is missing — not an error.
    Skipped { reason: &'static str },
    /// No bot in the admin listing matches the configured webhook URL.
    BotNotFound { webhook_url: String },
    /// The bot exists but lacks the `response` feature; `message` carries the
    /// actionable `occ talk:bot:state` remediation hint.
    MissingResponseFeature {
        bot_id: String,
        bot_name: String,
        features: Option<u64>,
        message: String,
    },
    /// The admin API call failed.
    ApiError { status: Option<u16>, message: String },
}

/// Normalizes a URL for bot matching: drops the fragment and any trailing
/// slash so `https://x/hook/` matches `https://x/hook`.
pub fn normalize_url_for_match(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match url::Url::parse(trimmed) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            parsed.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => trimmed.trim_end_matches('/').to_string(),
    }
}

fn coerce_feature_mask(value: Option<&serde_json::Value>) -> Option<u64> {
    match value? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn format_missing_response_feature_message(
    bot_id: &str,
    bot_name: &str,
    features: Option<u64>,
) -> String {
    let feature_text = features
        .map(|f| format!(" (features={})", f))
        .unwrap_or_default();
    let name = if bot_name.trim().is_empty() {
        "matching bot"
    } else {
        bot_name
    };
    format!(
        "Nextcloud Talk bot \"{}\" ({}) is missing the response feature{}; outbound replies will fail. \
         Run ./occ talk:bot:state --feature webhook --feature response --feature reaction {} 1 \
         or reinstall the bot with --feature response.",
        name, bot_id, feature_text, bot_id
    )
}

/// Evaluates the admin bot listing (`ocs.data`) against the configured
/// webhook URL and requires the `response` feature bit.
pub fn evaluate_bot_response_feature(
    bots: &serde_json::Value,
    webhook_public_url: &str,
) -> BotResponseFeatureProbe {
    let webhook_url = normalize_url_for_match(webhook_public_url);
    if webhook_url.is_empty() {
        return BotResponseFeatureProbe::Skipped {
            reason: "webhookPublicUrl is not configured",
        };
    }
    let entries = bots.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let bot = entries.iter().find(|entry| {
        entry
            .get("url")
            .and_then(|v| v.as_str())
            .map(normalize_url_for_match)
            .as_deref()
            == Some(webhook_url.as_str())
    });
    let Some(bot) = bot else {
        return BotResponseFeatureProbe::BotNotFound { webhook_url };
    };
    let bot_id = bot
        .get("id")
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());
    let bot_name = bot
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let features = coerce_feature_mask(bot.get("features"));
    match features {
        Some(mask) if mask & BOT_FEATURE_RESPONSE == BOT_FEATURE_RESPONSE => {
            BotResponseFeatureProbe::Ok {
                bot_id,
                bot_name,
                features: mask,
            }
        }
        _ => {
            let message = format_missing_response_feature_message(&bot_id, &bot_name, features);
            BotResponseFeatureProbe::MissingResponseFeature {
                bot_id,
                bot_name,
                features,
                message,
            }
        }
    }
}

// ============================================================================
// Nextcloud Talk Channel Implementation
// ============================================================================

/// Nextcloud Talk channel integration via the OCS API.
///
/// Communicates with a Nextcloud instance using the Talk API (OCS format).
/// Messages are sent via
/// `POST /ocs/v2.php/apps/spreed/api/v1/chat/{token}`.
///
/// Authentication uses either a Nextcloud app password or a bot-specific
/// token. All API calls require the `OCS-APIRequest: true` header.
///
/// API docs: <https://nextcloud-talk.readthedocs.io/en/latest/>
pub struct NextcloudChannel {
    /// Nextcloud server URL (e.g. `https://cloud.example.com`).
    server_url: Option<String>,
    /// Authentication token (app password or bot token).
    token: Option<String>,
    /// Nextcloud username for basic auth.
    username: Option<String>,
    /// Public webhook URL for the `response`-feature preflight.
    webhook_public_url: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl NextcloudChannel {
    pub fn new() -> Self {
        Self {
            server_url: None,
            token: None,
            username: None,
            webhook_public_url: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Nextcloud Talk channel.
    pub fn with_config(server_url: String, username: String, token: String) -> Self {
        Self {
            server_url: Some(server_url),
            token: Some(token),
            username: Some(username),
            webhook_public_url: None,
            enabled: Some(true),
            client: Client::new(),
        }
    }

    /// Create a channel from the flattened extensions config
    /// (`channels.extensions["nextcloud"]`).
    pub fn from_config(config: &Config) -> Self {
        match resolve_nextcloud_extension_config(config) {
            Some(ext) => Self {
                enabled: ext.enabled,
                server_url: ext.base_url,
                token: ext.api_password.or(ext.bot_secret),
                username: ext.api_user,
                webhook_public_url: ext.webhook_public_url,
                client: Client::new(),
            },
            None => Self::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Probes the Talk admin bot listing and requires the `response`
    /// feature capability for the configured webhook URL
    /// (v2026.7.1 row 88, upstream `probeNextcloudTalkBotResponseFeature`).
    pub async fn probe_bot_response_feature(&self) -> BotResponseFeatureProbe {
        let Some(server_url) = self.server_url.as_deref() else {
            return BotResponseFeatureProbe::Skipped {
                reason: "baseUrl is not configured",
            };
        };
        let Some(webhook_url) = self.webhook_public_url.as_deref() else {
            return BotResponseFeatureProbe::Skipped {
                reason: "webhookPublicUrl is not configured",
            };
        };
        let (Some(username), Some(token)) = (self.username.as_deref(), self.token.as_deref())
        else {
            return BotResponseFeatureProbe::Skipped {
                reason: "apiUser/apiPassword are not configured",
            };
        };
        let url = format!(
            "{}/ocs/v2.php/apps/spreed/api/v1/bot/admin",
            server_url.trim_end_matches('/'),
        );
        let response = self
            .client
            .get(&url)
            .basic_auth(username, Some(token))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return BotResponseFeatureProbe::ApiError {
                    status: None,
                    message: format!("Nextcloud Talk bot response feature probe failed: {}", e),
                };
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return BotResponseFeatureProbe::ApiError {
                status: Some(status),
                message: format!(
                    "Nextcloud Talk bot response feature probe failed ({}): {}",
                    status,
                    body.chars().take(1024).collect::<String>()
                ),
            };
        }
        let payload: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return BotResponseFeatureProbe::ApiError {
                    status: None,
                    message: format!("Nextcloud Talk bot response feature probe failed: {}", e),
                };
            }
        };
        let bots = payload
            .get("ocs")
            .and_then(|o| o.get("data"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        evaluate_bot_response_feature(&bots, webhook_url)
    }
}

#[async_trait]
impl ChannelPlugin for NextcloudChannel {
    fn id(&self) -> &str {
        "nextcloud"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Nextcloud Talk".to_string(),
            description: "Nextcloud Talk channel via OCS API".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Groups,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let server_url = match &self.server_url {
            Some(url) => url,
            None => {
                warn!("Nextcloud Talk channel enabled but no server_url configured");
                return Ok(());
            }
        };

        if self.token.is_none() {
            warn!("Nextcloud Talk channel enabled but no token configured");
            return Ok(());
        }

        info!(server_url = %server_url, "Nextcloud Talk channel starting");

        // Verify connectivity by calling the capabilities endpoint.
        let caps_url = format!(
            "{}/ocs/v2.php/cloud/capabilities",
            server_url.trim_end_matches('/'),
        );

        let username = self.username.as_deref().unwrap_or("bot");
        let token = self.token.as_deref().unwrap_or_default();

        match self
            .client
            .get(&caps_url)
            .basic_auth(username, Some(token))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!("Nextcloud Talk: server capabilities endpoint reachable");
            }
            Ok(resp) => {
                warn!(
                    "Nextcloud Talk: capabilities returned status {}",
                    resp.status()
                );
            }
            Err(e) => {
                warn!("Nextcloud Talk: failed to reach server: {}", e);
            }
        }

        // Preflight: the Talk bot must carry the `response` feature or
        // outbound replies fail (v2026.7.1 row 88).
        match self.probe_bot_response_feature().await {
            BotResponseFeatureProbe::Ok { bot_name, features, .. } => {
                info!(bot = %bot_name, features, "Nextcloud Talk: bot has the response feature");
            }
            BotResponseFeatureProbe::Skipped { reason } => {
                info!("Nextcloud Talk: bot response feature probe skipped: {}", reason);
            }
            BotResponseFeatureProbe::BotNotFound { webhook_url } => {
                warn!(
                    "Nextcloud Talk: no bot with webhook URL {} found in admin listing",
                    webhook_url
                );
            }
            BotResponseFeatureProbe::MissingResponseFeature { message, .. } => {
                warn!("{}", message);
            }
            BotResponseFeatureProbe::ApiError { message, .. } => {
                warn!("{}", message);
            }
        }

        // Integration point: the Talk webhook HTTP server receives signed
        // bot webhooks; each body flows through `classify_talk_webhook` so
        // file-share/lifecycle system events are acknowledged (Ignored)
        // rather than rejected, and only `Message` decisions reach the agent.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Nextcloud Talk channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let server_url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk server_url not configured"))?;

        let username = self
            .username
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk username not configured"))?;

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk token not configured"))?;

        // `to` is a Nextcloud Talk conversation token (e.g. "abc123xy").
        let url = format!(
            "{}/ocs/v2.php/apps/spreed/api/v1/chat/{}",
            server_url.trim_end_matches('/'),
            to,
        );

        let body = serde_json::json!({
            "message": message,
        });

        info!(conversation = %to, "Nextcloud Talk: sending message");

        let resp = self
            .client
            .post(&url)
            .basic_auth(username, Some(token))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Nextcloud Talk send message failed ({}): {}",
                status,
                text
            );
        }

        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn note_create_body() -> String {
        serde_json::json!({
            "type": "Create",
            "actor": { "type": "Person", "id": "users/alice", "name": "Alice" },
            "object": {
                "type": "Note",
                "id": "123",
                "name": "message",
                "content": "hello world",
                "mediaType": "text/markdown"
            },
            "target": { "type": "Collection", "id": "roomtok", "name": "Team Room" }
        })
        .to_string()
    }

    #[test]
    fn nextcloud_classifies_chat_message() {
        let decision = classify_talk_webhook(&note_create_body());
        let TalkWebhookDecision::Message(msg) = decision else {
            panic!("expected message, got {:?}", decision);
        };
        assert_eq!(msg.message_id, "123");
        assert_eq!(msg.room_token, "roomtok");
        assert_eq!(msg.sender_id, "users/alice");
        assert_eq!(msg.text, "hello world");
        assert_eq!(msg.media_type, "text/markdown");
        assert!(msg.is_group_chat);
    }

    #[test]
    fn nextcloud_tolerates_lifecycle_and_file_share_events() {
        // Room lifecycle / activity events (non-Create) are ignored, not invalid.
        let activity = serde_json::json!({ "type": "Activity" }).to_string();
        assert_eq!(classify_talk_webhook(&activity), TalkWebhookDecision::Ignored);
        let delete = serde_json::json!({ "type": "Delete", "object": { "type": "Note" } }).to_string();
        assert_eq!(classify_talk_webhook(&delete), TalkWebhookDecision::Ignored);
        // File-share events: Create of a non-Note object is ignored.
        let file_share = serde_json::json!({
            "type": "Create",
            "object": { "type": "File", "id": "9" }
        })
        .to_string();
        assert_eq!(classify_talk_webhook(&file_share), TalkWebhookDecision::Ignored);
    }

    #[test]
    fn nextcloud_rejects_malformed_message_payloads() {
        assert_eq!(classify_talk_webhook("not json"), TalkWebhookDecision::Invalid);
        assert_eq!(classify_talk_webhook("{}"), TalkWebhookDecision::Invalid);
        // Claims to be a Note Create but misses required fields.
        let missing_target = serde_json::json!({
            "type": "Create",
            "actor": { "type": "Person", "id": "u", "name": "n" },
            "object": { "type": "Note", "id": "1", "content": "hi" }
        })
        .to_string();
        assert_eq!(classify_talk_webhook(&missing_target), TalkWebhookDecision::Invalid);
        // Wrong actor type fails the strict schema.
        let wrong_actor = serde_json::json!({
            "type": "Create",
            "actor": { "type": "Application", "id": "u", "name": "n" },
            "object": { "type": "Note", "id": "1", "content": "hi" },
            "target": { "type": "Collection", "id": "t", "name": "r" }
        })
        .to_string();
        assert_eq!(classify_talk_webhook(&wrong_actor), TalkWebhookDecision::Invalid);
    }

    #[test]
    fn nextcloud_message_falls_back_to_name_and_plain_text() {
        let body = serde_json::json!({
            "type": "Create",
            "actor": { "type": "Person", "id": "u", "name": "" },
            "object": { "type": "Note", "id": "1", "name": "fallback", "content": "" },
            "target": { "type": "Collection", "id": "t", "name": "" }
        })
        .to_string();
        let TalkWebhookDecision::Message(msg) = classify_talk_webhook(&body) else {
            panic!("expected message");
        };
        assert_eq!(msg.text, "fallback");
        assert_eq!(msg.media_type, "text/plain");
    }

    #[test]
    fn nextcloud_url_normalization_for_bot_match() {
        assert_eq!(
            normalize_url_for_match("https://x.example/hook/#frag"),
            "https://x.example/hook"
        );
        assert_eq!(
            normalize_url_for_match("  https://x.example/hook/ "),
            "https://x.example/hook"
        );
        assert_eq!(normalize_url_for_match("not a url/"), "not a url");
        assert_eq!(normalize_url_for_match(""), "");
    }

    #[test]
    fn nextcloud_bot_response_feature_evaluation() {
        let bots = serde_json::json!([
            { "id": 7, "name": "OpenClaw", "url": "https://gw.example/hook/", "features": 7 },
            { "id": 8, "name": "Other", "url": "https://other.example/hook", "features": 1 }
        ]);
        // Feature present (7 & 2 == 2).
        let probe = evaluate_bot_response_feature(&bots, "https://gw.example/hook");
        assert_eq!(
            probe,
            BotResponseFeatureProbe::Ok {
                bot_id: "7".to_string(),
                bot_name: "OpenClaw".to_string(),
                features: 7
            }
        );
        // Feature missing → actionable occ hint.
        let probe = evaluate_bot_response_feature(&bots, "https://other.example/hook");
        let BotResponseFeatureProbe::MissingResponseFeature { features, message, .. } = probe else {
            panic!("expected missing feature");
        };
        assert_eq!(features, Some(1));
        assert!(message.contains("occ talk:bot:state"));
        assert!(message.contains("--feature response"));
        // No matching bot.
        let probe = evaluate_bot_response_feature(&bots, "https://nowhere.example/hook");
        assert!(matches!(probe, BotResponseFeatureProbe::BotNotFound { .. }));
        // String feature mask coercion.
        let bots = serde_json::json!([
            { "id": "b1", "name": "S", "url": "https://s.example/h", "features": "3" }
        ]);
        assert!(matches!(
            evaluate_bot_response_feature(&bots, "https://s.example/h"),
            BotResponseFeatureProbe::Ok { features: 3, .. }
        ));
    }
}
