//! Discord streaming/status behavior (v2026.7.1).
//!
//! Ports of OpenClaw `src/channels/status-reactions.ts` (StatusReactionController
//! lifecycle queued→thinking→tool→done/error with `statusReactions.timing`),
//! `extensions/discord/src/monitor/reply-safety.ts` (tool-progress sanitizing),
//! `extensions/discord/src/preview-streaming.ts` (progress-draft default), and
//! the `trackToolCalls` reaction-binding gate from
//! `monitor/message-handler.process.ts`.
//!
//! Bundled-native port; upstream ships these inside the Discord npm plugin.

use crate::config::{StatusReactionsEmojiConfig, StatusReactionsTimingConfig};

// ============================================================================
// Status reaction defaults
// ============================================================================

/// Resolved emoji set for status reactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReactionEmojis {
    pub queued: String,
    pub thinking: String,
    pub tool: String,
    pub coding: String,
    pub web: String,
    pub deploy: String,
    pub build: String,
    pub concierge: String,
    pub done: String,
    pub error: String,
    pub stall_soft: String,
    pub stall_hard: String,
    pub compacting: String,
}

impl Default for StatusReactionEmojis {
    fn default() -> Self {
        Self {
            queued: "👀".to_string(),
            thinking: "🧠".to_string(),
            tool: "🛠️".to_string(),
            coding: "💻".to_string(),
            web: "🌐".to_string(),
            deploy: "🛫".to_string(),
            build: "🏗️".to_string(),
            concierge: "💁".to_string(),
            done: "✅".to_string(),
            error: "❌".to_string(),
            stall_soft: "⏳".to_string(),
            stall_hard: "⚠️".to_string(),
            compacting: "🗜️".to_string(),
        }
    }
}

impl StatusReactionEmojis {
    pub fn resolve(config: Option<&StatusReactionsEmojiConfig>) -> Self {
        let mut emojis = Self::default();
        let Some(config) = config else {
            return emojis;
        };
        macro_rules! apply {
            ($($field:ident),*) => {
                $(if let Some(value) = config.$field.as_deref() {
                    if !value.trim().is_empty() {
                        emojis.$field = value.trim().to_string();
                    }
                })*
            };
        }
        apply!(
            queued, thinking, tool, coding, web, deploy, build, concierge, done, error,
            stall_soft, stall_hard, compacting
        );
        emojis
    }
}

/// Resolved status reaction timing (`messages.statusReactions.timing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReactionTiming {
    pub debounce_ms: u64,
    pub stall_soft_ms: u64,
    pub stall_hard_ms: u64,
    pub done_hold_ms: u64,
    pub error_hold_ms: u64,
}

impl Default for StatusReactionTiming {
    fn default() -> Self {
        Self {
            debounce_ms: 700,
            stall_soft_ms: 10_000,
            stall_hard_ms: 30_000,
            done_hold_ms: 1_500,
            error_hold_ms: 2_500,
        }
    }
}

impl StatusReactionTiming {
    pub fn resolve(config: Option<&StatusReactionsTimingConfig>) -> Self {
        let defaults = Self::default();
        let Some(config) = config else {
            return defaults;
        };
        Self {
            debounce_ms: config.debounce_ms.unwrap_or(defaults.debounce_ms),
            stall_soft_ms: config.stall_soft_ms.unwrap_or(defaults.stall_soft_ms),
            stall_hard_ms: config.stall_hard_ms.unwrap_or(defaults.stall_hard_ms),
            done_hold_ms: config.done_hold_ms.unwrap_or(defaults.done_hold_ms),
            error_hold_ms: config.error_hold_ms.unwrap_or(defaults.error_hold_ms),
        }
    }
}

// ============================================================================
// Tool emoji categories
// ============================================================================

const CODING_TOOL_TOKENS: &[&str] = &[
    "exec", "process", "read", "write", "edit", "session_status", "bash",
];
const WEB_TOOL_TOKENS: &[&str] = &["web_search", "web-search", "web_fetch", "web-fetch", "browser"];
const DEPLOY_TOOL_TOKENS: &[&str] = &[
    "fastlane", "deploy", "upload", "testflight", "ship", "release", "publish", "distribute",
];
const BUILD_TOOL_TOKENS: &[&str] = &[
    "build", "compile", "xcode", "swift", "gradle", "cargo", "make", "cmake", "webpack", "vite",
    "tsc", "lint",
];
const CONCIERGE_TOOL_TOKENS: &[&str] = &[
    "navigate", "click", "fill", "screenshot", "scroll", "page", "form", "puppeteer",
    "playwright", "selenium", "chromedp",
];

/// Resolve the appropriate tool-status emoji for a tool invocation — this is
/// what makes tool-status emojis win over the generic thinking reaction while
/// tools run.
pub fn resolve_tool_emoji(tool_name: Option<&str>, emojis: &StatusReactionEmojis) -> String {
    let normalized = tool_name.unwrap_or("").trim().to_lowercase();
    if normalized.is_empty() {
        return emojis.tool.clone();
    }
    let has = |tokens: &[&str]| tokens.iter().any(|token| normalized.contains(token));
    if has(DEPLOY_TOOL_TOKENS) {
        emojis.deploy.clone()
    } else if has(BUILD_TOOL_TOKENS) {
        emojis.build.clone()
    } else if has(CONCIERGE_TOOL_TOKENS) {
        emojis.concierge.clone()
    } else if has(WEB_TOOL_TOKENS) {
        emojis.web.clone()
    } else if has(CODING_TOOL_TOKENS) {
        emojis.coding.clone()
    } else {
        emojis.tool.clone()
    }
}

// ============================================================================
// Status reaction engine (deterministic, poll-driven)
// ============================================================================

/// A reaction operation the channel adapter must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionOp {
    Set(String),
    Remove(String),
}

/// Deterministic port of the StatusReactionController: intermediate states
/// debounce (`timing.debounceMs`), terminal states apply immediately, stall
/// timers escalate ⏳/⚠️, and terminal states protect against later updates.
/// Reaction removals are deferred until cleanup to avoid visible flicker.
#[derive(Debug)]
pub struct StatusReactionEngine {
    enabled: bool,
    emojis: StatusReactionEmojis,
    timing: StatusReactionTiming,
    current: Option<String>,
    pending: Option<(String, u64)>,
    active: Vec<String>,
    finished: bool,
    last_activity_ms: u64,
    stall_soft_fired: bool,
    stall_hard_fired: bool,
    cleanup_at_ms: Option<u64>,
}

impl StatusReactionEngine {
    pub fn new(
        enabled: bool,
        emojis: StatusReactionEmojis,
        timing: StatusReactionTiming,
        now_ms: u64,
    ) -> Self {
        Self {
            enabled,
            emojis,
            timing,
            current: None,
            pending: None,
            active: Vec::new(),
            finished: false,
            last_activity_ms: now_ms,
            stall_soft_fired: false,
            stall_hard_fired: false,
            cleanup_at_ms: None,
        }
    }

    fn schedule(&mut self, emoji: String, now_ms: u64, immediate: bool) -> Vec<ReactionOp> {
        if !self.enabled || self.finished {
            return Vec::new();
        }
        self.last_activity_ms = now_ms;
        self.stall_soft_fired = false;
        self.stall_hard_fired = false;
        if immediate {
            self.pending = None;
            return self.apply(emoji);
        }
        if self.current.is_none() {
            // First state applies immediately so the user sees feedback fast.
            return self.apply(emoji);
        }
        self.pending = Some((emoji, now_ms + self.timing.debounce_ms));
        Vec::new()
    }

    fn apply(&mut self, emoji: String) -> Vec<ReactionOp> {
        if self.current.as_deref() == Some(emoji.as_str()) {
            return Vec::new();
        }
        self.current = Some(emoji.clone());
        if !self.active.contains(&emoji) {
            self.active.push(emoji.clone());
        }
        vec![ReactionOp::Set(emoji)]
    }

    pub fn set_queued(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = self.emojis.queued.clone();
        self.schedule(emoji, now_ms, false)
    }

    pub fn set_thinking(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = self.emojis.thinking.clone();
        self.schedule(emoji, now_ms, false)
    }

    /// Tool states resolve tool-category emojis (💻/🌐/🛫/🏗️/💁/🛠️) and
    /// replace any pending thinking reaction — tool-status emojis are shown
    /// before (instead of) the thinking reaction while the tool runs.
    pub fn set_tool(&mut self, tool_name: Option<&str>, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = resolve_tool_emoji(tool_name, &self.emojis);
        self.pending = None;
        self.schedule(emoji, now_ms, false)
    }

    pub fn set_compacting(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = self.emojis.compacting.clone();
        self.schedule(emoji, now_ms, false)
    }

    /// Cancel any pending debounced emoji (used before forcing a transition).
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    pub fn set_done(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = self.emojis.done.clone();
        let hold = self.timing.done_hold_ms;
        self.terminal(emoji, now_ms, hold)
    }

    pub fn set_error(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        let emoji = self.emojis.error.clone();
        let hold = self.timing.error_hold_ms;
        self.terminal(emoji, now_ms, hold)
    }

    fn terminal(&mut self, emoji: String, now_ms: u64, hold_ms: u64) -> Vec<ReactionOp> {
        if !self.enabled || self.finished {
            return Vec::new();
        }
        self.pending = None;
        self.finished = true;
        self.cleanup_at_ms = Some(now_ms + hold_ms);
        self.apply(emoji)
    }

    /// Advance timers: apply due debounced emoji, fire stall warnings, and
    /// clean up terminal reactions after the configured hold.
    pub fn poll(&mut self, now_ms: u64) -> Vec<ReactionOp> {
        if !self.enabled {
            return Vec::new();
        }
        let mut ops = Vec::new();
        if let Some(cleanup_at) = self.cleanup_at_ms {
            if now_ms >= cleanup_at {
                self.cleanup_at_ms = None;
                ops.extend(self.clear());
            }
            return ops;
        }
        if self.finished {
            return ops;
        }
        if let Some((emoji, due)) = self.pending.clone() {
            if now_ms >= due {
                self.pending = None;
                ops.extend(self.apply(emoji));
            }
        }
        let idle = now_ms.saturating_sub(self.last_activity_ms);
        if !self.stall_hard_fired && idle >= self.timing.stall_hard_ms {
            self.stall_hard_fired = true;
            self.stall_soft_fired = true;
            let emoji = self.emojis.stall_hard.clone();
            ops.extend(self.apply(emoji));
        } else if !self.stall_soft_fired && idle >= self.timing.stall_soft_ms {
            self.stall_soft_fired = true;
            let emoji = self.emojis.stall_soft.clone();
            ops.extend(self.apply(emoji));
        }
        ops
    }

    /// Remove all active reactions (deferred removals happen here).
    pub fn clear(&mut self) -> Vec<ReactionOp> {
        let ops = self
            .active
            .drain(..)
            .map(ReactionOp::Remove)
            .collect::<Vec<_>>();
        self.current = None;
        self.pending = None;
        ops
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

// ============================================================================
// trackToolCalls reaction binding gate
// ============================================================================

/// A message-tool `react` call the status-reaction lifecycle should adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedToolReaction {
    pub emoji: String,
    pub message_id: String,
}

/// Gate for binding status reactions to an agent-initiated `message(action=
/// react, trackToolCalls=true)` call: only on tool start, only for the message
/// tool, never for removals, and never when status reactions are disabled or
/// source replies are tool-only.
#[allow(clippy::too_many_arguments)]
pub fn resolve_tracked_tool_reaction(
    status_reactions_enabled: bool,
    source_replies_tool_only: bool,
    phase: &str,
    tool_name: &str,
    action: Option<&str>,
    track_tool_calls: bool,
    emoji: Option<&str>,
    remove: bool,
    message_id: Option<&str>,
    inbound_message_id: &str,
) -> Option<TrackedToolReaction> {
    if source_replies_tool_only || !status_reactions_enabled {
        return None;
    }
    if phase != "start" || tool_name != "message" {
        return None;
    }
    if action.map(|a| a.to_lowercase()).as_deref() != Some("react") {
        return None;
    }
    if !track_tool_calls || remove {
        return None;
    }
    let emoji = emoji.map(str::trim).filter(|e| !e.is_empty())?;
    let message_id = message_id
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(inbound_message_id);
    Some(TrackedToolReaction {
        emoji: emoji.to_string(),
        message_id: message_id.to_string(),
    })
}

// ============================================================================
// Tool progress sanitizing + suppression
// ============================================================================

/// Sanitize Discord-visible tool progress / front-channel text: strip internal
/// channel lines (analysis/commentary/thinking/reasoning prefixes) outside
/// code fences and collapse excess blank lines.
pub fn sanitize_discord_front_channel_text(text: &str) -> String {
    let mut in_fence = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            kept.push(line);
            continue;
        }
        if !in_fence && is_internal_channel_line(line.trim()) {
            continue;
        }
        kept.push(line);
    }
    collapse_excess_blank_lines(&kept.join("\n"))
}

fn is_internal_channel_line(line: &str) -> bool {
    let body = line.strip_prefix('>').map(str::trim_start).unwrap_or(line);
    for prefix in ["analysis", "commentary", "thinking", "reasoning"] {
        if body.len() >= prefix.len() && body[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let rest = body[prefix.len()..].trim_start();
            if rest.starts_with(':') || rest.starts_with('=') {
                return true;
            }
        }
    }
    false
}

fn collapse_excess_blank_lines(text: &str) -> String {
    // Trim trailing spaces/tabs, then collapse 3+ newlines to 2.
    let no_trailing = text
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = String::with_capacity(no_trailing.len());
    let mut newline_run = 0usize;
    for ch in no_trailing.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push(ch);
            }
        } else {
            newline_run = 0;
            out.push(ch);
        }
    }
    out
}

/// Suppress streamed tool progress when source replies are delivered only via
/// the message tool (no visible default response path).
pub fn should_suppress_tool_progress(source_replies_tool_only: bool) -> bool {
    source_replies_tool_only
}

// ============================================================================
// Streaming default + final-message safety
// ============================================================================

/// Discord streaming default: progress-draft streaming when nothing is
/// configured (`resolveDiscordPreviewStreamMode`).
pub fn resolve_discord_preview_stream_mode(configured: Option<&str>) -> String {
    match configured.map(str::trim).filter(|mode| !mode.is_empty()) {
        Some(mode) => mode.to_string(),
        None => "progress".to_string(),
    }
}

/// Allowed-mentions payload for finals delivered as fresh messages: fresh
/// sends mark the channel unread for the user, but must not escalate into
/// `@everyone`/`@here` broadcast pings.
pub fn final_message_allowed_mentions() -> serde_json::Value {
    serde_json::json!({ "parse": ["users", "roles"] })
}

/// Whether the final reply should be sent as a fresh message (mark unread)
/// rather than editing the last streamed progress draft.
pub fn should_send_final_as_fresh_message(stream_mode: &str) -> bool {
    // Progress drafts are working artifacts; finals always land as new
    // messages so clients mark the channel unread.
    stream_mode == "progress"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(now: u64) -> StatusReactionEngine {
        StatusReactionEngine::new(
            true,
            StatusReactionEmojis::default(),
            StatusReactionTiming::default(),
            now,
        )
    }

    #[test]
    fn lifecycle_queued_thinking_tool_done() {
        let mut e = engine(0);
        // First state applies immediately.
        assert_eq!(e.set_queued(0), vec![ReactionOp::Set("👀".to_string())]);
        // Intermediate states debounce.
        assert!(e.set_thinking(100).is_empty());
        assert!(e.poll(300).is_empty());
        assert_eq!(e.poll(900), vec![ReactionOp::Set("🧠".to_string())]);
        // Tool state replaces pending and resolves category emoji.
        assert!(e.set_tool(Some("bash"), 1000).is_empty());
        assert_eq!(e.poll(1800), vec![ReactionOp::Set("💻".to_string())]);
        // Terminal done is immediate and protects against later updates.
        assert_eq!(e.set_done(2000), vec![ReactionOp::Set("✅".to_string())]);
        assert!(e.set_thinking(2100).is_empty());
        assert!(e.is_finished());
        // Cleanup removes all active reactions after the hold.
        let cleanup = e.poll(2000 + 1500);
        assert!(cleanup.contains(&ReactionOp::Remove("👀".to_string())));
        assert!(cleanup.contains(&ReactionOp::Remove("✅".to_string())));
    }

    #[test]
    fn error_state_uses_error_hold() {
        let mut e = engine(0);
        e.set_queued(0);
        assert_eq!(e.set_error(100), vec![ReactionOp::Set("❌".to_string())]);
        assert!(e.poll(100 + 2_400).is_empty());
        assert!(!e.poll(100 + 2_500).is_empty());
    }

    #[test]
    fn stall_timers_escalate() {
        let mut e = engine(0);
        e.set_queued(0);
        assert_eq!(e.poll(10_000), vec![ReactionOp::Set("⏳".to_string())]);
        assert_eq!(e.poll(30_000), vec![ReactionOp::Set("⚠️".to_string())]);
        // Activity resets stall state.
        assert!(e.set_thinking(31_000).is_empty());
        assert_eq!(e.poll(31_700), vec![ReactionOp::Set("🧠".to_string())]);
        assert!(e.poll(32_000).is_empty());
    }

    #[test]
    fn timing_config_resolution() {
        let cfg = StatusReactionsTimingConfig {
            debounce_ms: Some(100),
            stall_soft_ms: None,
            stall_hard_ms: Some(60_000),
            done_hold_ms: None,
            error_hold_ms: None,
        };
        let timing = StatusReactionTiming::resolve(Some(&cfg));
        assert_eq!(timing.debounce_ms, 100);
        assert_eq!(timing.stall_soft_ms, 10_000);
        assert_eq!(timing.stall_hard_ms, 60_000);
    }

    #[test]
    fn tool_emoji_categories() {
        let emojis = StatusReactionEmojis::default();
        assert_eq!(resolve_tool_emoji(Some("web_search"), &emojis), "🌐");
        assert_eq!(resolve_tool_emoji(Some("cargo-build"), &emojis), "🏗️");
        assert_eq!(resolve_tool_emoji(Some("deploy_prod"), &emojis), "🛫");
        assert_eq!(resolve_tool_emoji(Some("screenshot"), &emojis), "💁");
        assert_eq!(resolve_tool_emoji(Some("bash"), &emojis), "💻");
        assert_eq!(resolve_tool_emoji(Some("mystery"), &emojis), "🛠️");
        assert_eq!(resolve_tool_emoji(None, &emojis), "🛠️");
    }

    #[test]
    fn tracked_tool_reaction_gate() {
        let some = resolve_tracked_tool_reaction(
            true, false, "start", "message", Some("react"), true, Some("🔥"), false, None, "m1",
        );
        assert_eq!(
            some,
            Some(TrackedToolReaction {
                emoji: "🔥".to_string(),
                message_id: "m1".to_string()
            })
        );
        // Disabled status reactions / tool-only replies / removals never bind.
        assert!(resolve_tracked_tool_reaction(
            false, false, "start", "message", Some("react"), true, Some("🔥"), false, None, "m1"
        )
        .is_none());
        assert!(resolve_tracked_tool_reaction(
            true, true, "start", "message", Some("react"), true, Some("🔥"), false, None, "m1"
        )
        .is_none());
        assert!(resolve_tracked_tool_reaction(
            true, false, "start", "message", Some("react"), true, Some("🔥"), true, None, "m1"
        )
        .is_none());
        assert!(resolve_tracked_tool_reaction(
            true, false, "end", "message", Some("react"), true, Some("🔥"), false, None, "m1"
        )
        .is_none());
        assert!(resolve_tracked_tool_reaction(
            true, false, "start", "message", Some("send"), true, Some("🔥"), false, None, "m1"
        )
        .is_none());
        // Explicit message id wins over the inbound message.
        let explicit = resolve_tracked_tool_reaction(
            true, false, "start", "message", Some("REACT"), true, Some("🔥"), false, Some("m9"),
            "m1",
        )
        .unwrap();
        assert_eq!(explicit.message_id, "m9");
    }

    #[test]
    fn sanitizes_internal_channel_lines_outside_fences() {
        let text = "Hello\nanalysis: internal note\n> commentary: quoted internal\nWorld\n```\nthinking: keep inside fence\n```\nReasoning: also internal\n";
        let out = sanitize_discord_front_channel_text(text);
        assert!(out.contains("Hello"));
        assert!(out.contains("World"));
        assert!(out.contains("thinking: keep inside fence"));
        assert!(!out.contains("internal note"));
        assert!(!out.contains("quoted internal"));
        assert!(!out.contains("also internal"));
    }

    #[test]
    fn collapses_blank_lines() {
        let out = sanitize_discord_front_channel_text("a\n\n\n\nb");
        assert_eq!(out, "a\n\nb");
    }

    #[test]
    fn stream_mode_default_is_progress() {
        assert_eq!(resolve_discord_preview_stream_mode(None), "progress");
        assert_eq!(resolve_discord_preview_stream_mode(Some("  ")), "progress");
        assert_eq!(resolve_discord_preview_stream_mode(Some("partial")), "partial");
    }

    #[test]
    fn final_message_mentions_never_escalate() {
        let mentions = final_message_allowed_mentions();
        let parse = mentions["parse"].as_array().unwrap();
        assert!(parse.iter().any(|v| v == "users"));
        assert!(parse.iter().any(|v| v == "roles"));
        assert!(!parse.iter().any(|v| v == "everyone"));
        assert!(should_send_final_as_fresh_message("progress"));
        assert!(!should_send_final_as_fresh_message("partial"));
    }
}
