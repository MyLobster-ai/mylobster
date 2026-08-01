use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// IRC Channel Implementation
// ============================================================================

/// IRC channel integration.
///
/// Connects to an IRC server via raw TCP (optionally TLS) and joins the
/// configured channels. Uses a simple line-based IRC protocol implementation.
///
/// This is a non-REST channel — it requires a persistent TCP connection.
/// `send_message` will return an error if the connection is not active.
pub struct IrcChannel {
    /// IRC server hostname (e.g. `irc.libera.chat`).
    server: Option<String>,
    /// IRC server port (default: 6667, or 6697 for TLS).
    port: Option<u16>,
    /// Bot nickname.
    nick: Option<String>,
    /// List of IRC channels to join (e.g. `["#mylobster", "#general"]`).
    channels: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether the IRC connection is currently active.
    connected: Arc<AtomicBool>,
}

impl IrcChannel {
    pub fn new() -> Self {
        Self {
            server: None,
            port: None,
            nick: None,
            channels: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured IRC channel.
    pub fn with_config(
        server: String,
        port: u16,
        nick: String,
        channels: Vec<String>,
    ) -> Self {
        Self {
            server: Some(server),
            port: Some(port),
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
impl ChannelPlugin for IrcChannel {
    fn id(&self) -> &str {
        "irc"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "IRC".to_string(),
            description: "Internet Relay Chat channel via raw TCP connection".to_string(),
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

        let server = match &self.server {
            Some(s) => s,
            None => {
                warn!("IRC channel enabled but no server configured");
                return Ok(());
            }
        };

        let port = self.port.unwrap_or(6667);
        let nick = self.nick.as_deref().unwrap_or("mylobster");
        let channels = self.channels.as_deref().unwrap_or(&[]);

        info!(
            server = %server,
            port = %port,
            nick = %nick,
            channels = ?channels,
            "IRC channel starting — would connect to server"
        );

        // TODO: Establish a TCP (or TLS) connection to the IRC server.
        // 1. Send NICK and USER commands.
        // 2. Join configured channels.
        // 3. Start a read loop to parse incoming IRC messages.
        //
        // The connection lifecycle would be managed in a spawned task,
        // setting `self.connected` to true once registered.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("IRC channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send QUIT command and close the TCP connection.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("IRC: not connected — cannot send message to {}", to);
        }

        info!(target_channel = %to, "IRC: sending PRIVMSG");

        // `to` is an IRC channel name (e.g. "#mylobster") or a nick for DMs.
        // Wire integration point: each line from `chunk_privmsg_lines(to,
        // message, IRC_DEFAULT_CHUNK_MAX_CHARS)` is written to the active TCP
        // stream once the connection loop lands.
        let _ = chunk_privmsg_lines(to, message, IRC_DEFAULT_CHUNK_MAX_CHARS)?;

        Ok(())
    }
}

// ============================================================================
// UTF-16-safe PRIVMSG chunking + 512-byte wire truncation
//
// Port of OpenClaw `extensions/irc/src/client.ts` (`takeIrcPrivmsgChunk`,
// `sendPrivmsg`, v2026.7.1). Chunks are capped both by a character budget
// counted in UTF-16 code units (mirroring JS `String.length`, so surrogate
// pairs count as 2) and by the RFC 1459 512-byte line limit after
// subtracting the `PRIVMSG <target> :\r\n` overhead. Splits never land
// inside a code point, and prefer a word boundary when one exists in the
// back half of the fitted chunk.
// ============================================================================

/// RFC 1459 maximum IRC line length in bytes, including `\r\n`.
pub const IRC_MAX_LINE_BYTES: usize = 512;

/// Default per-chunk character budget in UTF-16 code units
/// (upstream `messageChunkMaxChars ?? 350`).
pub const IRC_DEFAULT_CHUNK_MAX_CHARS: usize = 350;

/// Strip IRC control characters that would break framing (CR/LF/NUL and
/// other C0 controls except tab). Mirrors upstream `sanitizeIrcOutboundText`
/// flattening newlines to spaces before chunking.
pub fn sanitize_irc_outbound_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // A run of line breaks becomes exactly ONE space. Mapping each character
    // independently turned a CRLF pair into two spaces, so `one\r\ntwo`
    // reached the wire as `one  two`.
    let mut pending_break = false;
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            pending_break = true;
            continue;
        }
        if c != '\t' && c.is_control() {
            continue;
        }
        if pending_break {
            if !out.is_empty() {
                out.push(' ');
            }
            pending_break = false;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn has_irc_control_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// Take the next PRIVMSG chunk from `text`, honoring both the UTF-16
/// character cap and the byte cap (upstream `takeIrcPrivmsgChunk`).
///
/// Errors when the byte budget cannot fit even one code point (target
/// overhead leaves no room within the 512-byte line limit).
pub fn take_irc_privmsg_chunk(text: &str, max_chars_utf16: usize, max_bytes: usize) -> Result<&str> {
    let mut end_bytes = 0usize; // byte offset of the split
    let mut utf16_units = 0usize; // chunk length in UTF-16 code units
    for c in text.chars() {
        let c_utf16 = c.len_utf16();
        let c_bytes = c.len_utf8();
        let exceeds_char_cap = utf16_units > 0 && utf16_units + c_utf16 > max_chars_utf16;
        if exceeds_char_cap || end_bytes + c_bytes > max_bytes {
            break;
        }
        end_bytes += c_bytes;
        utf16_units += c_utf16;
    }
    if end_bytes == 0 {
        anyhow::bail!("IRC target leaves no room for message text within the 512-byte line limit");
    }
    if end_bytes == text.len() {
        return Ok(text);
    }
    let fitted = &text[..end_bytes];
    // A delimiter just beyond the cap already gives this chunk a clean word
    // boundary.
    if text[end_bytes..].starts_with(' ') {
        return Ok(fitted);
    }
    if let Some(split_at) = fitted.rfind(' ') {
        // Upstream compares UTF-16 indices: keep the word split only when it
        // lies in the back half of the fitted chunk.
        let split_utf16 = fitted[..split_at].chars().map(char::len_utf16).sum::<usize>();
        if split_utf16 >= utf16_units / 2 {
            return Ok(&fitted[..split_at]);
        }
    }
    Ok(fitted)
}

/// Split `text` into complete `PRIVMSG <target> :<chunk>` wire lines, each
/// guaranteed to fit in 512 bytes once `\r\n` is appended (upstream
/// `sendPrivmsg`).
pub fn chunk_privmsg_lines(target: &str, text: &str, max_chars_utf16: usize) -> Result<Vec<String>> {
    let cleaned = sanitize_irc_outbound_text(text);
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    let overhead = format!("PRIVMSG {} :\r\n", target).len();
    let max_chunk_bytes = IRC_MAX_LINE_BYTES.saturating_sub(overhead);
    let max_chars = max_chars_utf16.max(1);
    let mut lines = Vec::new();
    let mut remaining = cleaned.as_str();
    while !remaining.is_empty() {
        let chunk = take_irc_privmsg_chunk(remaining, max_chars, max_chunk_bytes)?;
        let trimmed = chunk.trim_end();
        if !trimmed.is_empty() {
            lines.push(format!("PRIVMSG {} :{}", target, trimmed));
        }
        remaining = remaining[chunk.len()..].trim_start();
        if trimmed.is_empty() && chunk.trim().is_empty() && remaining.is_empty() {
            break;
        }
    }
    Ok(lines)
}

// ============================================================================
// Monitor reconnect state machine
//
// Port of OpenClaw `extensions/irc/src/monitor.ts` (v2026.7.1): on
// disconnect or a failed reconnect attempt the monitor schedules a single
// reconnect timer (1 s); stop/abort wins over any pending timer and further
// events are ignored once stopped.
// ============================================================================

/// Reconnect delay between attempts (upstream `IRC_MONITOR_RECONNECT_DELAY_MS`).
pub const IRC_MONITOR_RECONNECT_DELAY_MS: u64 = 1000;

/// Observable lifecycle states of the IRC monitor connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrcMonitorState {
    Idle,
    Connecting,
    Connected,
    WaitingReconnect,
    Stopped,
}

/// Events fed into the reconnect state machine by the connection driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrcMonitorEvent {
    /// A connection attempt is starting.
    ConnectStarted,
    /// Registration completed (welcome received).
    Connected,
    /// The active connection dropped.
    Disconnected,
    /// The in-flight connection attempt failed.
    ConnectFailed,
    /// The scheduled reconnect timer fired.
    ReconnectTimerFired,
    /// The monitor is being stopped (explicit stop or abort signal).
    StopRequested,
}

/// Action the connection driver must take after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrcMonitorAction {
    None,
    /// Begin a connection attempt now.
    Connect,
    /// Arm a reconnect timer for `delay_ms`.
    ScheduleReconnect { delay_ms: u64 },
    /// Send QUIT and tear down (stop path).
    Shutdown,
}

/// Pure reconnect state machine; the async driver owns sockets and timers.
#[derive(Debug)]
pub struct IrcReconnectMonitor {
    state: IrcMonitorState,
    timer_pending: bool,
}

impl Default for IrcReconnectMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl IrcReconnectMonitor {
    pub fn new() -> Self {
        Self {
            state: IrcMonitorState::Idle,
            timer_pending: false,
        }
    }

    pub fn state(&self) -> IrcMonitorState {
        self.state
    }

    /// Guarded reconnect scheduling (upstream `scheduleReconnect`): no-op
    /// when stopped or a timer is already pending.
    fn schedule(&mut self) -> IrcMonitorAction {
        if self.state == IrcMonitorState::Stopped || self.timer_pending {
            return IrcMonitorAction::None;
        }
        self.timer_pending = true;
        self.state = IrcMonitorState::WaitingReconnect;
        IrcMonitorAction::ScheduleReconnect {
            delay_ms: IRC_MONITOR_RECONNECT_DELAY_MS,
        }
    }

    pub fn handle(&mut self, event: IrcMonitorEvent) -> IrcMonitorAction {
        if self.state == IrcMonitorState::Stopped {
            return IrcMonitorAction::None;
        }
        match event {
            IrcMonitorEvent::ConnectStarted => {
                self.state = IrcMonitorState::Connecting;
                IrcMonitorAction::None
            }
            IrcMonitorEvent::Connected => {
                self.state = IrcMonitorState::Connected;
                IrcMonitorAction::None
            }
            IrcMonitorEvent::Disconnected | IrcMonitorEvent::ConnectFailed => self.schedule(),
            IrcMonitorEvent::ReconnectTimerFired => {
                self.timer_pending = false;
                self.state = IrcMonitorState::Connecting;
                IrcMonitorAction::Connect
            }
            IrcMonitorEvent::StopRequested => {
                self.state = IrcMonitorState::Stopped;
                self.timer_pending = false;
                IrcMonitorAction::Shutdown
            }
        }
    }
}

// ============================================================================
// Canonical channel routes + target normalization
//
// Port of OpenClaw `extensions/irc/src/normalize.ts` (v2026.7.1): IRC
// channel names are case-insensitive on the wire, so session route keys use
// a canonical lowercase spelling; targets accept `irc:`, `channel:` and
// `user:` prefixes.
// ============================================================================

/// True for channel targets (`#chan` / `&chan`), upstream `isChannelTarget`.
pub fn is_irc_channel_target(target: &str) -> bool {
    target.starts_with('#') || target.starts_with('&')
}

fn looks_like_irc_target_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || has_irc_control_chars(trimmed) {
        return false;
    }
    // Upstream `IRC_TARGET_PATTERN`: /^[^\s:]+$/u
    !trimmed.chars().any(|c| c.is_whitespace() || c == ':')
}

/// Normalize a raw messaging target (upstream `normalizeIrcMessagingTarget`):
/// strips `irc:` / `channel:` / `user:` prefixes (case-insensitive), forces a
/// `#` onto bare `channel:` names, and validates the remainder.
pub fn normalize_irc_messaging_target(raw: &str) -> Option<String> {
    let mut target = raw.trim().to_string();
    if target.is_empty() {
        return None;
    }
    if target.to_ascii_lowercase().starts_with("irc:") {
        target = target["irc:".len()..].trim().to_string();
    }
    if target.to_ascii_lowercase().starts_with("channel:") {
        target = target["channel:".len()..].trim().to_string();
        if !target.starts_with('#') && !target.starts_with('&') {
            target = format!("#{}", target);
        }
    }
    if target.to_ascii_lowercase().starts_with("user:") {
        target = target["user:".len()..].trim().to_string();
    }
    if target.is_empty() || !looks_like_irc_target_id(&target) {
        return None;
    }
    Some(target)
}

/// Canonical route key for an IRC target: normalized spelling, lowercased.
/// Channel names are case-insensitive per RFC 1459 casemapping, so
/// `#OpenClaw` and `#openclaw` collapse to one session route.
pub fn canonical_irc_route_key(raw: &str) -> Option<String> {
    normalize_irc_messaging_target(raw).map(|t| t.to_ascii_lowercase())
}

/// Normalize an allowlist entry (upstream `normalizeIrcAllowEntry`):
/// lowercase, strip `irc:` and `user:` prefixes.
pub fn normalize_irc_allow_entry(raw: &str) -> String {
    let mut value = raw.trim().to_ascii_lowercase();
    if let Some(rest) = value.strip_prefix("irc:") {
        value = rest.to_string();
    }
    if let Some(rest) = value.strip_prefix("user:") {
        value = rest.to_string();
    }
    value.trim().to_string()
}

/// Resolve where an inbound PRIVMSG routes (upstream
/// `resolveIrcInboundTarget`): channel messages keep the channel target;
/// direct messages route back to the sender nick.
pub fn resolve_irc_inbound_target(target: &str, sender_nick: &str) -> (bool, String) {
    if is_irc_channel_target(target) {
        return (true, target.to_string());
    }
    let nick = sender_nick.trim();
    if nick.is_empty() {
        (false, target.to_string())
    } else {
        (false, nick.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- chunking ----

    #[test]
    fn short_message_is_single_line() {
        let lines = chunk_privmsg_lines("#chan", "hello world", 350).unwrap();
        assert_eq!(lines, vec!["PRIVMSG #chan :hello world"]);
    }

    #[test]
    fn char_cap_counts_utf16_units() {
        // Each 😀 is 2 UTF-16 units / 4 UTF-8 bytes. A 5-unit budget fits
        // two emoji (4 units) but not three.
        let text = "😀😀😀";
        let chunk = take_irc_privmsg_chunk(text, 5, 1000).unwrap();
        assert_eq!(chunk, "😀😀");
    }

    #[test]
    fn byte_cap_never_splits_a_code_point() {
        let text = "😀😀"; // 8 bytes
        let chunk = take_irc_privmsg_chunk(text, 100, 5).unwrap();
        assert_eq!(chunk, "😀"); // 4 bytes fit; splitting mid-emoji is impossible
    }

    #[test]
    fn wire_lines_respect_512_byte_limit() {
        let text = "a".repeat(2000);
        let lines = chunk_privmsg_lines("#somechannel", &text, 5000).unwrap();
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.len() + 2 <= IRC_MAX_LINE_BYTES, "line too long: {}", line.len());
        }
        let total: usize = lines
            .iter()
            .map(|l| l.trim_start_matches("PRIVMSG #somechannel :").len())
            .sum();
        assert_eq!(total, 2000);
    }

    #[test]
    fn chunk_prefers_word_boundary_in_back_half() {
        let text = "hello brave new world";
        // Budget of 12 UTF-16 units cuts inside "new"; last space is at
        // index 11 (>= 12/2) so the chunk ends at the word boundary.
        let chunk = take_irc_privmsg_chunk(text, 12, 1000).unwrap();
        assert_eq!(chunk, "hello brave");
    }

    #[test]
    fn chunk_keeps_hard_cut_when_boundary_in_front_half() {
        let text = "hi aaaaaaaaaaaaaaaaaaaa";
        let chunk = take_irc_privmsg_chunk(text, 14, 1000).unwrap();
        // Last space (index 2) is before the midpoint, so hard-cut wins.
        assert_eq!(chunk.len(), 14);
    }

    #[test]
    fn delimiter_just_beyond_cap_is_clean_boundary() {
        let text = "abcdef ghij";
        let chunk = take_irc_privmsg_chunk(text, 6, 1000).unwrap();
        assert_eq!(chunk, "abcdef");
    }

    #[test]
    fn zero_room_for_text_errors() {
        assert!(take_irc_privmsg_chunk("hello", 10, 0).is_err());
    }

    #[test]
    fn newlines_flatten_to_spaces_before_chunking() {
        let lines = chunk_privmsg_lines("#c", "one\ntwo\r\nthree", 350).unwrap();
        assert_eq!(lines, vec!["PRIVMSG #c :one two three"]);
    }

    // ---- reconnect state machine ----

    #[test]
    fn disconnect_schedules_single_reconnect() {
        let mut m = IrcReconnectMonitor::new();
        assert_eq!(m.handle(IrcMonitorEvent::ConnectStarted), IrcMonitorAction::None);
        assert_eq!(m.handle(IrcMonitorEvent::Connected), IrcMonitorAction::None);
        assert_eq!(
            m.handle(IrcMonitorEvent::Disconnected),
            IrcMonitorAction::ScheduleReconnect {
                delay_ms: IRC_MONITOR_RECONNECT_DELAY_MS
            }
        );
        // Second disconnect while a timer is pending must not double-arm.
        assert_eq!(m.handle(IrcMonitorEvent::Disconnected), IrcMonitorAction::None);
        assert_eq!(m.state(), IrcMonitorState::WaitingReconnect);
    }

    #[test]
    fn timer_fire_drives_connect_and_failure_reschedules() {
        let mut m = IrcReconnectMonitor::new();
        m.handle(IrcMonitorEvent::ConnectStarted);
        m.handle(IrcMonitorEvent::Connected);
        m.handle(IrcMonitorEvent::Disconnected);
        assert_eq!(
            m.handle(IrcMonitorEvent::ReconnectTimerFired),
            IrcMonitorAction::Connect
        );
        assert_eq!(m.state(), IrcMonitorState::Connecting);
        assert_eq!(
            m.handle(IrcMonitorEvent::ConnectFailed),
            IrcMonitorAction::ScheduleReconnect {
                delay_ms: IRC_MONITOR_RECONNECT_DELAY_MS
            }
        );
    }

    #[test]
    fn stop_wins_over_pending_reconnect() {
        let mut m = IrcReconnectMonitor::new();
        m.handle(IrcMonitorEvent::ConnectStarted);
        m.handle(IrcMonitorEvent::Connected);
        m.handle(IrcMonitorEvent::Disconnected);
        assert_eq!(m.handle(IrcMonitorEvent::StopRequested), IrcMonitorAction::Shutdown);
        assert_eq!(m.state(), IrcMonitorState::Stopped);
        // Everything after stop is inert.
        assert_eq!(m.handle(IrcMonitorEvent::ReconnectTimerFired), IrcMonitorAction::None);
        assert_eq!(m.handle(IrcMonitorEvent::Disconnected), IrcMonitorAction::None);
    }

    // ---- canonical routes ----

    #[test]
    fn channel_names_are_case_insensitive_route_keys() {
        assert_eq!(
            canonical_irc_route_key("#OpenClaw").unwrap(),
            canonical_irc_route_key("#openclaw").unwrap()
        );
        assert_eq!(canonical_irc_route_key("#OpenClaw").unwrap(), "#openclaw");
        assert_eq!(canonical_irc_route_key("SomeNick").unwrap(), "somenick");
    }

    #[test]
    fn messaging_target_prefixes_normalize() {
        assert_eq!(normalize_irc_messaging_target("irc:#chan").unwrap(), "#chan");
        assert_eq!(normalize_irc_messaging_target("channel:general").unwrap(), "#general");
        assert_eq!(normalize_irc_messaging_target("CHANNEL:#ops").unwrap(), "#ops");
        assert_eq!(normalize_irc_messaging_target("user:alice").unwrap(), "alice");
        assert_eq!(normalize_irc_messaging_target("  bob  ").unwrap(), "bob");
    }

    #[test]
    fn invalid_targets_are_rejected() {
        assert!(normalize_irc_messaging_target("").is_none());
        assert!(normalize_irc_messaging_target("has space").is_none());
        assert!(normalize_irc_messaging_target("has:colon").is_none());
        assert!(normalize_irc_messaging_target("ctrl\u{1}char").is_none());
    }

    #[test]
    fn allow_entries_normalize() {
        assert_eq!(normalize_irc_allow_entry("IRC:Alice"), "alice");
        assert_eq!(normalize_irc_allow_entry("user:Bob"), "bob");
        assert_eq!(normalize_irc_allow_entry("  Carol "), "carol");
    }

    #[test]
    fn inbound_dm_routes_to_sender_nick() {
        assert_eq!(
            resolve_irc_inbound_target("#chan", "alice"),
            (true, "#chan".to_string())
        );
        assert_eq!(
            resolve_irc_inbound_target("mybot", "alice"),
            (false, "alice".to_string())
        );
        assert_eq!(
            resolve_irc_inbound_target("mybot", "  "),
            (false, "mybot".to_string())
        );
    }
}
