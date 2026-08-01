use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::agents::tools::web_fetch::{
    hostname_resolves_to_private_ip_with_policy, is_private_ip, SsrfPolicy,
};
use crate::gateway::GatewayState;

use anyhow::{bail, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::Client;
use rusqlite::Connection;
use std::net::IpAddr;
use tracing::{info, warn};
use url::Url;

// ============================================================================
// Microsoft Teams Channel Implementation
// ============================================================================

/// Microsoft Teams channel integration using the Bot Framework REST API.
///
/// Communicates with Teams via the Azure Bot Service / Bot Framework v3 API.
/// Messages are sent using the Bot Connector REST API at
/// `https://smba.trafficmanager.net/` (or the `serviceUrl` from incoming
/// activities).
///
/// Configuration requires an Azure Bot registration with app ID and password.
pub struct TeamsChannel {
    /// Azure Bot app ID (client ID from Azure AD app registration).
    app_id: Option<String>,
    /// Azure Bot app password (client secret).
    app_password: Option<String>,
    /// Bot Framework service URL (set from incoming activity `serviceUrl`).
    service_url: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for Bot Framework API calls.
    client: Client,
}

impl TeamsChannel {
    pub fn new() -> Self {
        Self {
            app_id: None,
            app_password: None,
            service_url: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Teams channel.
    pub fn with_config(app_id: String, app_password: String) -> Self {
        Self {
            app_id: Some(app_id),
            app_password: Some(app_password),
            service_url: None,
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Acquire an OAuth2 token from Azure AD for the Bot Framework.
    ///
    /// Calls `https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token`
    /// with client credentials grant.
    async fn acquire_token(&self) -> Result<String> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Teams app_id not configured"))?;
        let app_password = self
            .app_password
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Teams app_password not configured"))?;

        let token_url =
            "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";

        let resp = self
            .client
            .post(token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", app_id),
                ("client_secret", app_password),
                (
                    "scope",
                    "https://api.botframework.com/.default",
                ),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Teams OAuth2 token request failed ({}): {}", status, text);
        }

        let body: serde_json::Value = resp.json().await?;
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Teams: no access_token in OAuth2 response"))?
            .to_string();

        Ok(token)
    }
}

#[async_trait]
impl ChannelPlugin for TeamsChannel {
    fn id(&self) -> &str {
        "teams"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Microsoft Teams".to_string(),
            description: "Microsoft Teams channel via Bot Framework REST API".to_string(),
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
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.app_id.is_none() || self.app_password.is_none() {
            warn!("Teams channel enabled but app_id or app_password not configured");
            return Ok(());
        }

        info!("Microsoft Teams channel starting");

        // Validate credentials by acquiring an initial token.
        match self.acquire_token().await {
            Ok(_) => info!("Teams: OAuth2 token acquired successfully"),
            Err(e) => warn!("Teams: failed to acquire initial OAuth2 token: {}", e),
        }

        // TODO: Set up an HTTP endpoint to receive incoming Bot Framework activities.
        // The Bot Framework sends POST requests to the bot's messaging endpoint.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Microsoft Teams channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // Only trusted Bot Framework hosts may receive service tokens
        // (service-URL trust validation, see below).
        let service_url = normalize_bot_framework_service_url(
            self.service_url
                .as_deref()
                .unwrap_or("https://smba.trafficmanager.net/amer/"),
        )?;

        let token = self.acquire_token().await?;

        // `to` is a conversation ID from the Bot Framework activity.
        // Format: `POST {serviceUrl}/v3/conversations/{conversationId}/activities`
        let url = format!("{}/v3/conversations/{}/activities", service_url, to);

        let body = serde_json::json!({
            "type": "message",
            "text": message,
        });

        info!(conversation_id = %to, "Teams: sending message");

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
            anyhow::bail!("Teams send failed ({}): {}", status, text);
        }

        Ok(())
    }
}

// ============================================================================
// Threaded reply routing for proactive sends
//
// Port of OpenClaw `extensions/msteams/src/sdk-proactive.ts`
// (`resolveThreadedConversationId`) and `conversation-store-helpers.ts`
// (`normalizeStoredConversationId`) at v2026.7.1. Proactive channel sends
// that should land inside a thread address the Bot Connector conversation
// as `<baseConversationId>;messageid=<threadActivityId>`; a send without a
// thread activity strips any stale `;messageid=` suffix so it posts
// top-level instead of into whichever thread the id was captured in.
// ============================================================================

/// Strip any `;messageid=...` (or other `;`-suffixed) qualifier from a
/// stored conversation id (upstream `normalizeStoredConversationId`).
pub fn normalize_stored_conversation_id(raw: &str) -> &str {
    raw.split(';').next().unwrap_or(raw)
}

/// Resolve the conversation id for a proactive send. With a thread activity
/// id the send routes into that thread; without one it routes top-level
/// (upstream `resolveThreadedConversationId`).
pub fn resolve_threaded_conversation_id(
    conversation_id: &str,
    thread_activity_id: Option<&str>,
) -> String {
    let base = normalize_stored_conversation_id(conversation_id);
    match thread_activity_id.map(str::trim).filter(|t| !t.is_empty()) {
        Some(thread) => format!("{};messageid={}", base, thread),
        None => base.to_string(),
    }
}

// ============================================================================
// Persisted sent-message markers (TTL) — SQLite store
//
// Port of OpenClaw `extensions/msteams/src/sent-message-cache.ts` +
// `sqlite-state.ts` (v2026.7.1): messages the bot itself sent are recorded
// as `<conversationId>:<messageId>` markers so inbound webhook deliveries
// of our own activities can be recognized and skipped. Markers carry a
// 24 h TTL, survive restarts (upstream: persistent keyed store slice), and
// a sweep prunes expired rows plus the oldest rows beyond the entry cap.
// ============================================================================

/// Marker TTL (upstream `TTL_MS`).
pub const TEAMS_SENT_MESSAGE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Maximum retained markers (upstream in-memory `MAX_ENTRIES`).
pub const TEAMS_SENT_STORE_MAX_ENTRIES: i64 = 20_000;

/// SQLite-backed sent-message marker store.
pub struct TeamsSentMessageStore {
    conn: Mutex<Connection>,
}

impl TeamsSentMessageStore {
    /// Open (and migrate) a store at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// In-memory store (tests / ephemeral runs).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS msteams_sent_messages (
                 key TEXT PRIMARY KEY,
                 sent_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_msteams_sent_messages_sent_at
                 ON msteams_sent_messages(sent_at_ms);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn make_key(conversation_id: &str, message_id: &str) -> String {
        format!("{}:{}", conversation_id, message_id)
    }

    /// Record a sent message marker. Empty ids are ignored (upstream guard).
    /// Re-recording keeps the original send time so restored entries keep
    /// their TTL window (upstream `readTimestamp` re-priming).
    pub fn record(&self, conversation_id: &str, message_id: &str, now_ms: i64) -> Result<()> {
        if conversation_id.is_empty() || message_id.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO msteams_sent_messages (key, sent_at_ms) VALUES (?1, ?2)
             ON CONFLICT(key) DO NOTHING",
            rusqlite::params![Self::make_key(conversation_id, message_id), now_ms],
        )?;
        Ok(())
    }

    /// Whether the marker exists and is inside its TTL window.
    pub fn was_sent(&self, conversation_id: &str, message_id: &str, now_ms: i64) -> Result<bool> {
        if conversation_id.is_empty() || message_id.is_empty() {
            return Ok(false);
        }
        let conn = self.conn.lock();
        let sent_at: Option<i64> = conn
            .query_row(
                "SELECT sent_at_ms FROM msteams_sent_messages WHERE key = ?1",
                rusqlite::params![Self::make_key(conversation_id, message_id)],
                |row| row.get(0),
            )
            .ok();
        Ok(matches!(sent_at, Some(at) if now_ms - at < TEAMS_SENT_MESSAGE_TTL_MS))
    }

    /// Sweep: delete expired markers, then trim the oldest rows beyond the
    /// entry cap. Returns the number of rows removed.
    pub fn sweep(&self, now_ms: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let expired = conn.execute(
            "DELETE FROM msteams_sent_messages WHERE sent_at_ms <= ?1",
            rusqlite::params![now_ms - TEAMS_SENT_MESSAGE_TTL_MS],
        )?;
        let trimmed = conn.execute(
            "DELETE FROM msteams_sent_messages WHERE key IN (
                 SELECT key FROM msteams_sent_messages
                 ORDER BY sent_at_ms DESC
                 LIMIT -1 OFFSET ?1
             )",
            rusqlite::params![TEAMS_SENT_STORE_MAX_ENTRIES],
        )?;
        Ok(expired + trimmed)
    }

    /// Number of stored markers (sweeping is the caller's business).
    pub fn len(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM msteams_sent_messages", [], |r| r.get(0))?)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

// ============================================================================
// Service-URL trust validation + attachment-fetch DNS validation
//
// Port of OpenClaw `extensions/msteams/src/bot-framework-service-url.ts`
// (v2026.7.1). Only documented Bot Framework serviceUrl hosts may receive
// Bot Framework service tokens; attachment content fetches are further
// gated behind the same trusted-host suffix allowlist plus a private-range
// block (IP-literal check + DNS re-resolution, mirroring the SSRF logic in
// `src/agents/tools/web_fetch.rs`).
// ============================================================================

/// Documented Bot Framework serviceUrl hosts for commercial, GCC, GCC High,
/// DOD, and Azure China clouds (suffix-matched).
pub const BOT_FRAMEWORK_SERVICE_URL_HOST_ALLOWLIST: &[&str] = &[
    "smba.trafficmanager.net",
    "smba.infra.gcc.teams.microsoft.com",
    "smba.infra.gov.teams.microsoft.us",
    "smba.infra.dod.teams.microsoft.us",
    "botframework.azure.cn",
];

fn host_matches_suffix(host: &str, suffix: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let suffix = suffix.trim_end_matches('.').to_ascii_lowercase();
    if suffix.is_empty() {
        return false;
    }
    host == suffix || host.ends_with(&format!(".{}", suffix))
}

fn host_in_allowlist(host: &str, extra_trusted_hosts: &[String]) -> bool {
    BOT_FRAMEWORK_SERVICE_URL_HOST_ALLOWLIST
        .iter()
        .any(|s| host_matches_suffix(host, s))
        || extra_trusted_hosts.iter().any(|s| host_matches_suffix(host, s))
}

/// Hostname of a serviceUrl for diagnostics, or `"invalid-url"` (upstream
/// `describeBotFrameworkServiceUrlHost`).
pub fn describe_bot_framework_service_url_host(service_url: &str) -> String {
    Url::parse(service_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "invalid-url".to_string())
}

/// Whether a serviceUrl is HTTPS and under a trusted Bot Framework host
/// (upstream `isAllowedBotFrameworkServiceUrl`).
pub fn is_allowed_bot_framework_service_url(service_url: &str) -> bool {
    let trimmed = service_url.trim();
    if trimmed.is_empty() {
        return false;
    }
    match Url::parse(trimmed) {
        Ok(url) => {
            url.scheme() == "https"
                && url
                    .host_str()
                    .map(|h| host_in_allowlist(h, &[]))
                    .unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Normalize a trusted serviceUrl (trim + strip trailing slashes), or `None`
/// when untrusted (upstream `tryNormalizeBotFrameworkServiceUrl`).
pub fn try_normalize_bot_framework_service_url(service_url: &str) -> Option<String> {
    if !is_allowed_bot_framework_service_url(service_url) {
        return None;
    }
    Some(service_url.trim().trim_end_matches('/').to_string())
}

/// Normalizing variant that fails closed with the upstream error wording.
pub fn normalize_bot_framework_service_url(service_url: &str) -> Result<String> {
    try_normalize_bot_framework_service_url(service_url).ok_or_else(|| {
        anyhow::anyhow!(
            "Blocked Microsoft Teams serviceUrl host: {}",
            describe_bot_framework_service_url_host(service_url)
        )
    })
}

/// Static (pre-DNS) validation of a Teams attachment content URL: HTTPS
/// only, host under the trusted serviceUrl allowlist (plus configured
/// extras), and IP-literal hosts must not target private ranges.
pub fn validate_teams_attachment_url(url_str: &str, extra_trusted_hosts: &[String]) -> Result<Url> {
    let url = Url::parse(url_str.trim())
        .map_err(|_| anyhow::anyhow!("Teams attachment URL is not a valid URL"))?;
    if url.scheme() != "https" {
        bail!("Teams attachment URL must be https");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Teams attachment URL has no host"))?;
    if let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        if is_private_ip(ip) {
            bail!("Teams attachment URL targets a private/internal address (SSRF protection)");
        }
        // Trusted serviceUrl hosts are DNS names; raw public IPs are still
        // outside the trust boundary for attachment tokens.
        bail!("Teams attachment URL host is not a trusted serviceUrl host: {}", host);
    }
    if !host_in_allowlist(host, extra_trusted_hosts) {
        bail!("Teams attachment URL host is not a trusted serviceUrl host: {}", host);
    }
    Ok(url)
}

/// Full validation including the DNS re-resolution check (defends against
/// DNS rebinding of a nominally-trusted name into private space).
pub async fn validate_teams_attachment_url_with_dns(
    url_str: &str,
    extra_trusted_hosts: &[String],
) -> Result<Url> {
    let url = validate_teams_attachment_url(url_str, extra_trusted_hosts)?;
    if let Some(host) = url.host_str() {
        if hostname_resolves_to_private_ip_with_policy(host, &SsrfPolicy::default()).await {
            bail!("Teams attachment URL hostname resolves to a private/internal address (SSRF protection)");
        }
    }
    Ok(url)
}

// ============================================================================
// JWKS / Bot Connector egress diagnostics
//
// Port of OpenClaw `extensions/msteams/src/errors.ts` (v2026.7.1) egress
// classification: failures reaching Azure AD (token), the Bot Framework
// JWKS endpoint, or the Bot Connector are classified so the operator sees
// what is blocked instead of an opaque fetch error.
// ============================================================================

/// Which Teams egress endpoint the failure occurred against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamsEgressEndpoint {
    /// `login.microsoftonline.com` OAuth2 token endpoint.
    OauthToken,
    /// Bot Framework OpenID/JWKS metadata endpoint.
    Jwks,
    /// `smba.trafficmanager.net` (or regional) Bot Connector.
    BotConnector,
}

impl TeamsEgressEndpoint {
    fn label(self) -> &'static str {
        match self {
            TeamsEgressEndpoint::OauthToken => "Azure AD token endpoint",
            TeamsEgressEndpoint::Jwks => "Bot Framework JWKS endpoint",
            TeamsEgressEndpoint::BotConnector => "Bot Connector",
        }
    }
}

/// Classified egress failure cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamsEgressErrorClass {
    Dns,
    Timeout,
    Tls,
    ConnectionRefused,
    ProxyBlocked,
    HttpStatus(u16),
    Other,
}

/// Classify an egress error from its transport detail string and/or HTTP
/// status (mirrors upstream status/code extraction in `errors.ts`, adapted
/// to reqwest error text).
pub fn classify_teams_egress_error(detail: &str, status: Option<u16>) -> TeamsEgressErrorClass {
    if let Some(code) = status {
        if !(200..300).contains(&code) {
            return TeamsEgressErrorClass::HttpStatus(code);
        }
    }
    let lower = detail.to_ascii_lowercase();
    if lower.contains("dns") || lower.contains("name or service not known") || lower.contains("failed to lookup") {
        TeamsEgressErrorClass::Dns
    } else if lower.contains("timed out") || lower.contains("timeout") {
        TeamsEgressErrorClass::Timeout
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        TeamsEgressErrorClass::Tls
    } else if lower.contains("connection refused") || lower.contains("econnrefused") {
        TeamsEgressErrorClass::ConnectionRefused
    } else if lower.contains("proxy") || lower.contains("407") {
        TeamsEgressErrorClass::ProxyBlocked
    } else {
        TeamsEgressErrorClass::Other
    }
}

/// Operator-facing description of a classified egress failure, with the
/// remediation hint attached.
pub fn describe_teams_egress_failure(
    endpoint: TeamsEgressEndpoint,
    class: TeamsEgressErrorClass,
    detail: &str,
) -> String {
    let cause = match class {
        TeamsEgressErrorClass::Dns => {
            "DNS resolution failed — check outbound DNS / network egress".to_string()
        }
        TeamsEgressErrorClass::Timeout => {
            "request timed out — check firewall/egress rules for *.microsoftonline.com, *.botframework.com and smba.trafficmanager.net".to_string()
        }
        TeamsEgressErrorClass::Tls => {
            "TLS handshake failed — check for intercepting proxies / custom CA requirements".to_string()
        }
        TeamsEgressErrorClass::ConnectionRefused => {
            "connection refused — outbound HTTPS appears blocked".to_string()
        }
        TeamsEgressErrorClass::ProxyBlocked => {
            "blocked by HTTP proxy — check proxy allowlist for Microsoft Bot Framework hosts".to_string()
        }
        TeamsEgressErrorClass::HttpStatus(401) => {
            "rejected with 401 — check the Azure Bot app ID / password (client secret may be expired)".to_string()
        }
        TeamsEgressErrorClass::HttpStatus(403) => {
            "rejected with 403 — the bot registration may lack the Teams channel or tenant consent".to_string()
        }
        TeamsEgressErrorClass::HttpStatus(code) => format!("HTTP {} from the service", code),
        TeamsEgressErrorClass::Other => "unclassified egress failure".to_string(),
    };
    format!("Teams egress failure ({}): {} [{}]", endpoint.label(), cause, detail)
}

// ============================================================================
// Admin-only group actions gate
//
// Group-management actions (member add/remove, channel create/delete,
// group rename) invoked from a Teams group are gated to configured admin
// users; with no admins configured the gate fails closed (v2026.7.1 admin
// gating on Graph group management).
// ============================================================================

/// Group actions requiring the admin gate.
pub const TEAMS_ADMIN_GATED_GROUP_ACTIONS: &[&str] = &[
    "add-member",
    "remove-member",
    "create-channel",
    "delete-channel",
    "rename-group",
    "archive-team",
];

/// Whether the action name is subject to the admin gate.
pub fn is_admin_gated_group_action(action: &str) -> bool {
    let a = action.trim().to_ascii_lowercase();
    TEAMS_ADMIN_GATED_GROUP_ACTIONS.contains(&a.as_str())
}

/// Result of the admin gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamsGroupActionDecision {
    Allowed,
    DeniedNotAdmin,
    DeniedNoAdminsConfigured,
}

/// Gate a group action: the sender (by AAD object id or UPN,
/// case-insensitive) must be listed in `admin_users`. Fails closed when the
/// list is empty. Non-gated actions are always allowed.
pub fn resolve_teams_group_action_gate(
    action: &str,
    sender_id: &str,
    sender_upn: Option<&str>,
    admin_users: &[String],
) -> TeamsGroupActionDecision {
    if !is_admin_gated_group_action(action) {
        return TeamsGroupActionDecision::Allowed;
    }
    if admin_users.is_empty() {
        return TeamsGroupActionDecision::DeniedNoAdminsConfigured;
    }
    let matches_admin = |candidate: &str| {
        let c = candidate.trim().to_ascii_lowercase();
        !c.is_empty() && admin_users.iter().any(|a| a.trim().to_ascii_lowercase() == c)
    };
    if matches_admin(sender_id) || sender_upn.map(matches_admin).unwrap_or(false) {
        TeamsGroupActionDecision::Allowed
    } else {
        TeamsGroupActionDecision::DeniedNotAdmin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- threaded reply routing ----

    #[test]
    fn proactive_send_targets_thread_when_activity_id_present() {
        assert_eq!(
            resolve_threaded_conversation_id("19:abc@thread.tacv2", Some("174"))
                .as_str(),
            "19:abc@thread.tacv2;messageid=174"
        );
        // A stale ;messageid= on the stored id is replaced, not doubled.
        assert_eq!(
            resolve_threaded_conversation_id("19:abc@thread.tacv2;messageid=1", Some("174")),
            "19:abc@thread.tacv2;messageid=174"
        );
    }

    #[test]
    fn proactive_send_without_thread_posts_top_level() {
        assert_eq!(
            resolve_threaded_conversation_id("19:abc@thread.tacv2;messageid=17", None),
            "19:abc@thread.tacv2"
        );
        assert_eq!(
            resolve_threaded_conversation_id("19:abc@thread.tacv2", Some("  ")),
            "19:abc@thread.tacv2"
        );
        assert_eq!(normalize_stored_conversation_id("a;b;c"), "a");
    }

    // ---- sent-message store ----

    #[test]
    fn sent_markers_round_trip_and_expire() {
        let store = TeamsSentMessageStore::open_in_memory().unwrap();
        store.record("conv", "msg1", 1_000).unwrap();
        assert!(store.was_sent("conv", "msg1", 2_000).unwrap());
        assert!(!store.was_sent("conv", "other", 2_000).unwrap());
        assert!(!store.was_sent("", "msg1", 2_000).unwrap());
        // Past the TTL the marker no longer matches.
        assert!(!store
            .was_sent("conv", "msg1", 1_000 + TEAMS_SENT_MESSAGE_TTL_MS)
            .unwrap());
    }

    #[test]
    fn record_keeps_original_send_time() {
        let store = TeamsSentMessageStore::open_in_memory().unwrap();
        store.record("conv", "msg", 1_000).unwrap();
        // Re-recording (e.g. restore-and-register) must not extend the TTL.
        store.record("conv", "msg", 5_000_000).unwrap();
        assert!(!store
            .was_sent("conv", "msg", 1_000 + TEAMS_SENT_MESSAGE_TTL_MS)
            .unwrap());
    }

    #[test]
    fn sweep_prunes_expired_and_caps_entries() {
        let store = TeamsSentMessageStore::open_in_memory().unwrap();
        store.record("conv", "old", 0).unwrap();
        store.record("conv", "fresh", TEAMS_SENT_MESSAGE_TTL_MS).unwrap();
        let removed = store.sweep(TEAMS_SENT_MESSAGE_TTL_MS + 1).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.len().unwrap(), 1);
        assert!(store
            .was_sent("conv", "fresh", TEAMS_SENT_MESSAGE_TTL_MS + 2)
            .unwrap());
    }

    #[test]
    fn sweep_trims_oldest_beyond_cap() {
        let store = TeamsSentMessageStore::open_in_memory().unwrap();
        // Shrink-scale check of the trim query: insert 5, cap is global, so
        // exercise the OFFSET clause directly with a small synthetic cap.
        for i in 0..5 {
            store.record("conv", &format!("m{}", i), i).unwrap();
        }
        {
            let conn = store.conn.lock();
            conn.execute(
                "DELETE FROM msteams_sent_messages WHERE key IN (
                     SELECT key FROM msteams_sent_messages
                     ORDER BY sent_at_ms DESC LIMIT -1 OFFSET 3)",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.len().unwrap(), 3);
        // Oldest two (m0, m1) were trimmed; newest three remain.
        assert!(!store.was_sent("conv", "m0", 10).unwrap());
        assert!(store.was_sent("conv", "m4", 10).unwrap());
    }

    // ---- service-URL trust ----

    #[test]
    fn documented_service_url_hosts_are_trusted() {
        assert!(is_allowed_bot_framework_service_url(
            "https://smba.trafficmanager.net/amer/"
        ));
        assert!(is_allowed_bot_framework_service_url(
            "https://region.smba.infra.gcc.teams.microsoft.com/"
        ));
        assert!(is_allowed_bot_framework_service_url(
            "https://asia.botframework.azure.cn/"
        ));
    }

    #[test]
    fn untrusted_or_non_https_service_urls_are_blocked() {
        assert!(!is_allowed_bot_framework_service_url("https://evil.example.com/"));
        // Suffix must match on a label boundary.
        assert!(!is_allowed_bot_framework_service_url(
            "https://evilsmba.trafficmanager.net.example.com/"
        ));
        assert!(!is_allowed_bot_framework_service_url(
            "http://smba.trafficmanager.net/amer/"
        ));
        assert!(!is_allowed_bot_framework_service_url(""));
        assert!(!is_allowed_bot_framework_service_url("not a url"));
    }

    #[test]
    fn normalize_strips_trailing_slashes_and_fails_closed() {
        assert_eq!(
            try_normalize_bot_framework_service_url("https://smba.trafficmanager.net/amer/ ")
                .unwrap(),
            "https://smba.trafficmanager.net/amer"
        );
        let err = normalize_bot_framework_service_url("https://evil.example.com/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Blocked Microsoft Teams serviceUrl host: evil.example.com"));
        assert!(normalize_bot_framework_service_url("::bogus::")
            .unwrap_err()
            .to_string()
            .contains("invalid-url"));
    }

    // ---- attachment URL validation ----

    #[test]
    fn attachment_urls_require_trusted_https_hosts() {
        assert!(validate_teams_attachment_url(
            "https://smba.trafficmanager.net/amer/v3/attachments/1",
            &[]
        )
        .is_ok());
        assert!(validate_teams_attachment_url("http://smba.trafficmanager.net/x", &[]).is_err());
        assert!(validate_teams_attachment_url("https://attacker.example.com/x", &[]).is_err());
        // Extra trusted hosts extend the allowlist.
        assert!(validate_teams_attachment_url(
            "https://files.contoso.example/x",
            &["files.contoso.example".to_string()]
        )
        .is_ok());
    }

    #[test]
    fn attachment_urls_block_private_ip_literals() {
        for url in [
            "https://127.0.0.1/x",
            "https://10.0.0.8/x",
            "https://192.168.1.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest/meta-data",
        ] {
            assert!(validate_teams_attachment_url(url, &[]).is_err(), "{}", url);
        }
        // Public IP literals are also outside the trust boundary.
        assert!(validate_teams_attachment_url("https://8.8.8.8/x", &[]).is_err());
    }

    // ---- egress diagnostics ----

    #[test]
    fn egress_errors_classify_by_transport_detail() {
        assert_eq!(
            classify_teams_egress_error("failed to lookup address information", None),
            TeamsEgressErrorClass::Dns
        );
        assert_eq!(
            classify_teams_egress_error("operation timed out", None),
            TeamsEgressErrorClass::Timeout
        );
        assert_eq!(
            classify_teams_egress_error("invalid peer certificate", None),
            TeamsEgressErrorClass::Tls
        );
        assert_eq!(
            classify_teams_egress_error("Connection refused (os error 61)", None),
            TeamsEgressErrorClass::ConnectionRefused
        );
        assert_eq!(
            classify_teams_egress_error("anything", Some(401)),
            TeamsEgressErrorClass::HttpStatus(401)
        );
        assert_eq!(
            classify_teams_egress_error("mystery", None),
            TeamsEgressErrorClass::Other
        );
    }

    #[test]
    fn egress_descriptions_carry_endpoint_and_guidance() {
        let msg = describe_teams_egress_failure(
            TeamsEgressEndpoint::Jwks,
            TeamsEgressErrorClass::Dns,
            "lookup failed",
        );
        assert!(msg.contains("JWKS"));
        assert!(msg.contains("DNS"));
        let msg = describe_teams_egress_failure(
            TeamsEgressEndpoint::OauthToken,
            TeamsEgressErrorClass::HttpStatus(401),
            "401",
        );
        assert!(msg.contains("Azure AD token endpoint"));
        assert!(msg.contains("client secret"));
        let msg = describe_teams_egress_failure(
            TeamsEgressEndpoint::BotConnector,
            TeamsEgressErrorClass::Timeout,
            "t/o",
        );
        assert!(msg.contains("Bot Connector"));
        assert!(msg.contains("smba.trafficmanager.net"));
    }

    // ---- admin gate ----

    #[test]
    fn group_actions_gate_to_admins_and_fail_closed() {
        let admins = vec!["AdminUser@contoso.com".to_string(), "aad-guid-1".to_string()];
        assert_eq!(
            resolve_teams_group_action_gate("add-member", "aad-guid-1", None, &admins),
            TeamsGroupActionDecision::Allowed
        );
        assert_eq!(
            resolve_teams_group_action_gate(
                "Remove-Member",
                "other-id",
                Some("adminuser@CONTOSO.com"),
                &admins
            ),
            TeamsGroupActionDecision::Allowed
        );
        assert_eq!(
            resolve_teams_group_action_gate("add-member", "mallory", None, &admins),
            TeamsGroupActionDecision::DeniedNotAdmin
        );
        assert_eq!(
            resolve_teams_group_action_gate("add-member", "anyone", None, &[]),
            TeamsGroupActionDecision::DeniedNoAdminsConfigured
        );
        // Non-gated actions pass through.
        assert_eq!(
            resolve_teams_group_action_gate("send-message", "anyone", None, &[]),
            TeamsGroupActionDecision::Allowed
        );
    }
}
