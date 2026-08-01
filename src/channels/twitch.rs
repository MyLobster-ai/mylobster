use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// Twitch Channel Implementation
// ============================================================================

/// Twitch chat channel integration via IRC (TMI).
///
/// Twitch chat uses an IRC-compatible protocol at `irc.chat.twitch.tv:6697`
/// (TLS). Authentication is via OAuth token. Messages are standard
/// PRIVMSG commands to Twitch channels (prefixed with `#`).
///
/// This is a non-REST channel — it requires a persistent IRC/TMI connection.
/// `send_message` will return an error if the connection is not active.
pub struct TwitchChannel {
    /// OAuth token for Twitch IRC (format: `oauth:xxxxx`).
    oauth_token: Option<String>,
    /// Bot nickname (Twitch username, lowercase).
    nick: Option<String>,
    /// List of Twitch channels to join (without `#` prefix).
    channels: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether the Twitch IRC connection is currently active.
    connected: Arc<AtomicBool>,
}

/// Twitch IRC (TMI) server address.
const TWITCH_IRC_HOST: &str = "irc.chat.twitch.tv";
/// Twitch IRC TLS port.
const TWITCH_IRC_PORT: u16 = 6697;

impl TwitchChannel {
    pub fn new() -> Self {
        Self {
            oauth_token: None,
            nick: None,
            channels: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured Twitch channel.
    pub fn with_config(
        oauth_token: String,
        nick: String,
        channels: Vec<String>,
    ) -> Self {
        Self {
            oauth_token: Some(oauth_token),
            nick: Some(nick),
            channels: Some(channels),
            enabled: Some(true),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for TwitchChannel {
    fn id(&self) -> &str {
        "twitch"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Twitch".to_string(),
            description: "Twitch chat channel via IRC/TMI protocol".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::Groups,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let oauth_token = match &self.oauth_token {
            Some(t) => t,
            None => {
                warn!("Twitch channel enabled but no oauth_token configured");
                return Ok(());
            }
        };

        let nick = self.nick.as_deref().unwrap_or("mylobster");
        let channels = self.channels.as_deref().unwrap_or(&[]);

        info!(
            host = %TWITCH_IRC_HOST,
            port = %TWITCH_IRC_PORT,
            nick = %nick,
            channels = ?channels,
            token_suffix = %&oauth_token[oauth_token.len().saturating_sub(4)..],
            "Twitch channel starting — would connect to TMI"
        );

        // TODO: Establish a TLS connection to irc.chat.twitch.tv:6697.
        // 1. Send: PASS oauth:<token>
        // 2. Send: NICK <nick>
        // 3. Send: CAP REQ :twitch.tv/membership twitch.tv/tags twitch.tv/commands
        // 4. JOIN each configured channel (prefixed with #).
        // 5. Start a read loop to parse incoming IRC messages and PING/PONG.
        //
        // The connection lifecycle would be managed in a spawned task,
        // setting `self.connected` to true once the welcome message (001) is received.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Twitch channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send QUIT and close the TLS connection.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!(
                "Twitch: not connected — cannot send message to #{}",
                to
            );
        }

        info!(channel = %to, "Twitch: sending PRIVMSG");

        // `to` is a Twitch channel name (without `#` prefix).
        // The message would be sent as: PRIVMSG #<to> :<message>
        //
        // TODO: Write the PRIVMSG line to the active TLS stream.
        // Twitch IRC has a 500-char message limit; split if needed.
        let _ = message;

        Ok(())
    }
}

// ============================================================================
// Chat-intent auth / token refresh
//
// Port of OpenClaw `extensions/twitch/src/token.ts` +
// `twitch-client.ts` (v2026.7.1). Upstream delegates the chat-intent
// validate/refresh loop to Twurple's `RefreshingAuthProvider`; here the
// equivalent behavior is an explicit state machine the connection driver
// polls: tokens are normalized to the `oauth:` form, validated against
// `https://id.twitch.tv/oauth2/validate`, refreshed via the refresh-token
// grant when a client secret is available, and treated as expiring early by
// a safety margin so a chat reconnect never races token expiry.
// ============================================================================

/// Refresh this long before the reported expiry (expiry margin).
pub const TWITCH_TOKEN_EXPIRY_MARGIN_MS: u64 = 60_000;

/// Twitch developer policy: apps must validate their token on startup and
/// then at least hourly for the lifetime of the account connection.
pub const TWITCH_TOKEN_VALIDATE_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// Where a resolved token came from (upstream `TwitchTokenSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwitchTokenSource {
    Config,
    Env,
    None,
}

/// Normalize a Twitch OAuth token: trim and ensure the `oauth:` prefix
/// (upstream `normalizeTwitchToken`).
pub fn normalize_twitch_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("oauth:") {
        Some(trimmed.to_string())
    } else {
        Some(format!("oauth:{}", trimmed))
    }
}

/// Strip the `oauth:` prefix for HTTP API use (validate/refresh endpoints
/// take the bare token; only the IRC PASS line wants the prefix).
pub fn bare_twitch_token(token: &str) -> &str {
    token.strip_prefix("oauth:").unwrap_or(token)
}

/// Resolve the access token for an account (upstream `resolveTwitchToken`):
/// config beats env, and the env fallback applies to the default account
/// only.
pub fn resolve_twitch_token(
    config_token: Option<&str>,
    env_token: Option<&str>,
    is_default_account: bool,
) -> (String, TwitchTokenSource) {
    if let Some(tok) = config_token.and_then(normalize_twitch_token) {
        return (tok, TwitchTokenSource::Config);
    }
    if is_default_account {
        if let Some(tok) = env_token.and_then(normalize_twitch_token) {
            return (tok, TwitchTokenSource::Env);
        }
    }
    (String::new(), TwitchTokenSource::None)
}

/// Auth lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwitchAuthPhase {
    /// Token has never been validated (or the hourly window elapsed).
    NeedsValidation,
    /// Token validated and inside its expiry window.
    Valid,
    /// Validation was rejected or expiry is imminent; refresh required.
    NeedsRefresh,
    /// Terminal: no path to a usable token (surface to the operator).
    Failed,
}

/// Next step the connection driver must perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwitchAuthAction {
    /// Call `GET https://id.twitch.tv/oauth2/validate` with the token.
    Validate,
    /// Call the refresh-token grant on `https://id.twitch.tv/oauth2/token`.
    Refresh,
    /// Token is good — use it for the chat connection.
    UseToken(String),
    /// Terminal failure with an operator-facing reason.
    Fail(String),
}

/// Validate/refresh state machine with expiry-margin logic.
#[derive(Debug, Clone)]
pub struct TwitchAuthState {
    access_token: String,
    refresh_token: Option<String>,
    has_client_secret: bool,
    phase: TwitchAuthPhase,
    /// Absolute expiry (ms epoch) reported by validate/refresh, if known.
    expires_at_ms: Option<u64>,
    /// Last successful validation (ms epoch).
    last_validated_ms: Option<u64>,
}

impl TwitchAuthState {
    pub fn new(access_token: String, refresh_token: Option<String>, has_client_secret: bool) -> Self {
        Self {
            access_token,
            refresh_token,
            has_client_secret,
            phase: TwitchAuthPhase::NeedsValidation,
            expires_at_ms: None,
            last_validated_ms: None,
        }
    }

    pub fn phase(&self) -> TwitchAuthPhase {
        self.phase
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Refresh is possible only with both a refresh token and a client
    /// secret (upstream: `RefreshingAuthProvider` requires clientSecret;
    /// without it a static token is used until it dies).
    pub fn can_refresh(&self) -> bool {
        self.has_client_secret && self.refresh_token.as_deref().is_some_and(|t| !t.is_empty())
    }

    /// True when the known expiry is inside the safety margin.
    fn expiring_soon(&self, now_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(at) => now_ms.saturating_add(TWITCH_TOKEN_EXPIRY_MARGIN_MS) >= at,
            None => false,
        }
    }

    /// Decide the next action (pure; drives the async side).
    pub fn next_action(&self, now_ms: u64) -> TwitchAuthAction {
        match self.phase {
            TwitchAuthPhase::Failed => TwitchAuthAction::Fail(
                "Twitch auth failed: token rejected and no refresh path (need refreshToken + clientSecret)"
                    .to_string(),
            ),
            TwitchAuthPhase::NeedsRefresh => {
                if self.can_refresh() {
                    TwitchAuthAction::Refresh
                } else {
                    TwitchAuthAction::Fail(
                        "Twitch token expired or rejected; token refresh disabled (no refresh token)"
                            .to_string(),
                    )
                }
            }
            TwitchAuthPhase::NeedsValidation => TwitchAuthAction::Validate,
            TwitchAuthPhase::Valid => {
                if self.expiring_soon(now_ms) {
                    if self.can_refresh() {
                        TwitchAuthAction::Refresh
                    } else {
                        // A static token stays in use until Twitch rejects it.
                        TwitchAuthAction::UseToken(self.access_token.clone())
                    }
                } else if self
                    .last_validated_ms
                    .map_or(true, |last| {
                        now_ms.saturating_sub(last) >= TWITCH_TOKEN_VALIDATE_INTERVAL_MS
                    })
                {
                    TwitchAuthAction::Validate
                } else {
                    TwitchAuthAction::UseToken(self.access_token.clone())
                }
            }
        }
    }

    /// `/validate` returned 200 with `expires_in` seconds.
    pub fn on_validated(&mut self, now_ms: u64, expires_in_s: Option<u64>) {
        self.last_validated_ms = Some(now_ms);
        self.expires_at_ms = expires_in_s.map(|s| now_ms.saturating_add(s.saturating_mul(1000)));
        self.phase = TwitchAuthPhase::Valid;
    }

    /// `/validate` returned 401 — the token is dead.
    pub fn on_validation_rejected(&mut self) {
        self.phase = if self.can_refresh() {
            TwitchAuthPhase::NeedsRefresh
        } else {
            TwitchAuthPhase::Failed
        };
    }

    /// Refresh grant succeeded with a new token pair.
    pub fn on_refreshed(
        &mut self,
        now_ms: u64,
        access_token: String,
        refresh_token: Option<String>,
        expires_in_s: Option<u64>,
    ) {
        self.access_token = access_token;
        if refresh_token.is_some() {
            self.refresh_token = refresh_token;
        }
        self.last_validated_ms = Some(now_ms);
        self.expires_at_ms = expires_in_s.map(|s| now_ms.saturating_add(s.saturating_mul(1000)));
        self.phase = TwitchAuthPhase::Valid;
    }

    /// Refresh grant failed (upstream `onRefreshFailure`).
    pub fn on_refresh_failed(&mut self) {
        self.phase = TwitchAuthPhase::Failed;
    }
}

// ============================================================================
// Account-lifetime keepalive
//
// Twitch requires connected apps to keep validating their token hourly for
// the lifetime of the session; a validation that stops implies a revoked
// token the app never notices. The policy below is the periodic
// validation-ping schedule the connection driver runs alongside IRC
// PING/PONG.
// ============================================================================

/// Periodic validation-ping policy for the lifetime of an account session.
#[derive(Debug, Clone, Copy)]
pub struct TwitchKeepalivePolicy {
    /// Interval between validation pings (defaults to the hourly mandate).
    pub interval_ms: u64,
}

impl Default for TwitchKeepalivePolicy {
    fn default() -> Self {
        Self {
            interval_ms: TWITCH_TOKEN_VALIDATE_INTERVAL_MS,
        }
    }
}

impl TwitchKeepalivePolicy {
    /// Absolute time the next validation ping is due.
    pub fn next_due_ms(&self, last_validated_ms: u64) -> u64 {
        last_validated_ms.saturating_add(self.interval_ms)
    }

    /// Whether a validation ping is due now. A session that has never
    /// validated is always due (validate-on-startup mandate).
    pub fn is_due(&self, last_validated_ms: Option<u64>, now_ms: u64) -> bool {
        match last_validated_ms {
            None => true,
            Some(last) => now_ms >= self.next_due_ms(last),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: u64 = 60 * 60 * 1000;

    // ---- token resolution / normalization ----

    #[test]
    fn tokens_are_normalized_to_oauth_prefix() {
        assert_eq!(normalize_twitch_token("abc").unwrap(), "oauth:abc");
        assert_eq!(normalize_twitch_token("oauth:abc").unwrap(), "oauth:abc");
        assert_eq!(normalize_twitch_token("  abc  ").unwrap(), "oauth:abc");
        assert!(normalize_twitch_token("   ").is_none());
        assert_eq!(bare_twitch_token("oauth:abc"), "abc");
        assert_eq!(bare_twitch_token("abc"), "abc");
    }

    #[test]
    fn config_token_beats_env_and_env_is_default_account_only() {
        let (tok, src) = resolve_twitch_token(Some("cfg"), Some("env"), true);
        assert_eq!((tok.as_str(), src), ("oauth:cfg", TwitchTokenSource::Config));

        let (tok, src) = resolve_twitch_token(None, Some("env"), true);
        assert_eq!((tok.as_str(), src), ("oauth:env", TwitchTokenSource::Env));

        let (tok, src) = resolve_twitch_token(None, Some("env"), false);
        assert_eq!((tok.as_str(), src), ("", TwitchTokenSource::None));
    }

    // ---- validate/refresh state machine ----

    fn refreshable() -> TwitchAuthState {
        TwitchAuthState::new("oauth:tok".into(), Some("refresh".into()), true)
    }

    fn static_only() -> TwitchAuthState {
        TwitchAuthState::new("oauth:tok".into(), None, false)
    }

    #[test]
    fn fresh_state_validates_first() {
        assert_eq!(refreshable().next_action(0), TwitchAuthAction::Validate);
    }

    #[test]
    fn validated_token_is_used_until_hourly_window() {
        let mut s = refreshable();
        s.on_validated(1000, Some(4 * 3600));
        assert_eq!(
            s.next_action(1000),
            TwitchAuthAction::UseToken("oauth:tok".into())
        );
        // One hour later the keepalive validation is due again.
        assert_eq!(s.next_action(1000 + HOUR_MS), TwitchAuthAction::Validate);
    }

    #[test]
    fn expiry_margin_triggers_refresh_before_actual_expiry() {
        let mut s = refreshable();
        // expires in 90 seconds
        s.on_validated(0, Some(90));
        // 40s in: 50s remain < 60s margin → refresh now.
        assert_eq!(s.next_action(40_000), TwitchAuthAction::Refresh);
        // 10s in: 80s remain > margin → still usable.
        assert_eq!(
            s.next_action(10_000),
            TwitchAuthAction::UseToken("oauth:tok".into())
        );
    }

    #[test]
    fn static_token_without_refresh_path_rides_until_rejected() {
        let mut s = static_only();
        s.on_validated(0, Some(90));
        // Inside the margin but no refresh path: keep using it.
        assert_eq!(
            s.next_action(40_000),
            TwitchAuthAction::UseToken("oauth:tok".into())
        );
        s.on_validation_rejected();
        assert_eq!(s.phase(), TwitchAuthPhase::Failed);
        assert!(matches!(s.next_action(50_000), TwitchAuthAction::Fail(_)));
    }

    #[test]
    fn rejected_validation_refreshes_when_possible() {
        let mut s = refreshable();
        s.on_validation_rejected();
        assert_eq!(s.phase(), TwitchAuthPhase::NeedsRefresh);
        assert_eq!(s.next_action(0), TwitchAuthAction::Refresh);
        s.on_refreshed(0, "oauth:new".into(), Some("refresh2".into()), Some(3600));
        assert_eq!(
            s.next_action(1),
            TwitchAuthAction::UseToken("oauth:new".into())
        );
    }

    #[test]
    fn refresh_failure_is_terminal() {
        let mut s = refreshable();
        s.on_validation_rejected();
        s.on_refresh_failed();
        assert_eq!(s.phase(), TwitchAuthPhase::Failed);
        assert!(matches!(s.next_action(0), TwitchAuthAction::Fail(_)));
    }

    #[test]
    fn refresh_without_secret_fails_with_clear_reason() {
        let mut s = TwitchAuthState::new("oauth:tok".into(), Some("refresh".into()), false);
        assert!(!s.can_refresh());
        s.on_validation_rejected();
        match s.next_action(0) {
            TwitchAuthAction::Fail(reason) => assert!(reason.contains("refresh")),
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    // ---- keepalive policy ----

    #[test]
    fn keepalive_is_hourly_and_due_on_startup() {
        let p = TwitchKeepalivePolicy::default();
        assert_eq!(p.interval_ms, HOUR_MS);
        assert!(p.is_due(None, 0));
        assert!(!p.is_due(Some(0), HOUR_MS - 1));
        assert!(p.is_due(Some(0), HOUR_MS));
        assert_eq!(p.next_due_ms(5), 5 + HOUR_MS);
    }
}
