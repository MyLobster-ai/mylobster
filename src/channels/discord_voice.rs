//! Discord voice overhaul behavior (v2026.7.1).
//!
//! Ports of the policy/config surface from OpenClaw
//! `extensions/discord/src/voice/` (manager.ts, realtime.ts, command.ts):
//! `/vc` subcommands, agent-proxy default with forced consult,
//! `voice.allowedChannels`, barge-in policy (`minBargeInAudioEndMs`,
//! `captureSilenceGraceMs`), follow-users, wake-name gating, and DAVE/connect
//! timing knobs.
//!
//! Bundled-native port; upstream ships this inside the Discord npm plugin.
//! The live audio stack (DAVE encryption recovery, ffmpeg stderr handling,
//! opus codecs, multi-user audio mixing) needs voice-transport dependencies
//! the Rust port does not take; those sub-parts are policy-only here.

use crate::config::{DiscordVoiceChannelRef, DiscordVoiceConfig, DiscordVoiceRealtimeConfig};

// ============================================================================
// Defaults
// ============================================================================

/// Minimum assistant playback duration before a barge-in truncates audio.
pub const DEFAULT_MIN_BARGE_IN_AUDIO_END_MS: u64 = 250;
/// Silence grace after a speaker ends before finalizing STT capture.
pub const DEFAULT_CAPTURE_SILENCE_GRACE_MS: u64 = 2_000;
/// Initial voice Ready wait.
pub const DEFAULT_VOICE_CONNECT_TIMEOUT_MS: u64 = 30_000;
/// Grace period for voice reconnect signalling after a disconnect.
pub const DEFAULT_VOICE_RECONNECT_GRACE_MS: u64 = 15_000;
/// Consecutive decrypt failures before DAVE session reinitialization.
pub const DEFAULT_DAVE_DECRYPTION_FAILURE_TOLERANCE: u64 = 24;

pub fn resolve_min_barge_in_audio_end_ms(realtime: Option<&DiscordVoiceRealtimeConfig>) -> u64 {
    realtime
        .and_then(|r| r.min_barge_in_audio_end_ms)
        .unwrap_or(DEFAULT_MIN_BARGE_IN_AUDIO_END_MS)
}

pub fn resolve_capture_silence_grace_ms(voice: Option<&DiscordVoiceConfig>) -> u64 {
    voice
        .and_then(|v| v.capture_silence_grace_ms)
        .unwrap_or(DEFAULT_CAPTURE_SILENCE_GRACE_MS)
}

pub fn resolve_voice_connect_timeout_ms(voice: Option<&DiscordVoiceConfig>) -> u64 {
    voice
        .and_then(|v| v.connect_timeout_ms)
        .unwrap_or(DEFAULT_VOICE_CONNECT_TIMEOUT_MS)
}

pub fn resolve_voice_reconnect_grace_ms(voice: Option<&DiscordVoiceConfig>) -> u64 {
    voice
        .and_then(|v| v.reconnect_grace_ms)
        .unwrap_or(DEFAULT_VOICE_RECONNECT_GRACE_MS)
}

pub fn resolve_dave_encryption_enabled(voice: Option<&DiscordVoiceConfig>) -> bool {
    voice.and_then(|v| v.dave_encryption).unwrap_or(true)
}

pub fn resolve_decryption_failure_tolerance(voice: Option<&DiscordVoiceConfig>) -> u64 {
    voice
        .and_then(|v| v.decryption_failure_tolerance)
        .unwrap_or(DEFAULT_DAVE_DECRYPTION_FAILURE_TOLERANCE)
}

// ============================================================================
// Allowed channels + follow users
// ============================================================================

/// Whether the bot may join or remain in a voice channel. Unset allowlist
/// means any channel is allowed.
pub fn is_voice_channel_allowed(
    allowed_channels: Option<&[DiscordVoiceChannelRef]>,
    guild_id: &str,
    channel_id: &str,
) -> bool {
    match allowed_channels {
        None => true,
        Some(entries) => entries
            .iter()
            .any(|entry| entry.guild_id == guild_id && entry.channel_id == channel_id),
    }
}

/// Whether configured follow-users are active (`followUsersEnabled !== false`).
pub fn follow_users_enabled(voice: Option<&DiscordVoiceConfig>) -> bool {
    voice.and_then(|v| v.follow_users_enabled) != Some(false)
}

/// Whether the bot should follow this user's voice channel moves.
pub fn should_follow_user(voice: Option<&DiscordVoiceConfig>, user_id: &str) -> bool {
    if !follow_users_enabled(voice) {
        return false;
    }
    voice
        .and_then(|v| v.follow_users.as_ref())
        .map(|users| users.iter().any(|u| u.trim() == user_id))
        .unwrap_or(false)
}

// ============================================================================
// Consult policy + wake names + barge-in
// ============================================================================

/// Consult policy for realtime voice: agent-proxy mode forces the agent brain
/// for every substantive turn by default ("always"); other modes default to
/// "auto".
pub fn resolve_voice_consult_policy(
    realtime: Option<&DiscordVoiceRealtimeConfig>,
    is_agent_proxy: bool,
) -> &'static str {
    match realtime.and_then(|r| r.consult_policy.as_deref()) {
        Some("always") => "always",
        Some("auto") => "auto",
        _ => {
            if is_agent_proxy {
                "always"
            } else {
                "auto"
            }
        }
    }
}

fn normalize_wake_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

/// Resolve wake names: configured names win; otherwise the routed agent name,
/// then the agent id.
pub fn resolve_voice_wake_names(
    realtime: Option<&DiscordVoiceRealtimeConfig>,
    agent_name: Option<&str>,
    agent_id: &str,
) -> Vec<String> {
    let configured: Vec<String> = realtime
        .and_then(|r| r.wake_names.as_ref())
        .map(|names| names.iter().filter_map(|n| normalize_wake_name(n)).collect())
        .unwrap_or_default();
    let mut names: Vec<String> = if !configured.is_empty() {
        configured
    } else {
        let mut defaults = Vec::new();
        if let Some(name) = agent_name.and_then(normalize_wake_name) {
            defaults.push(name);
        }
        if defaults.is_empty() {
            if let Some(id) = normalize_wake_name(agent_id) {
                defaults.push(id);
            }
        }
        defaults
    };
    names.sort();
    names.dedup();
    names
}

/// Whether wake-name gating applies (agent-proxy + `requireWakeName: true`).
pub fn require_wake_name(
    realtime: Option<&DiscordVoiceRealtimeConfig>,
    is_agent_proxy: bool,
) -> bool {
    is_agent_proxy && realtime.and_then(|r| r.require_wake_name) == Some(true)
}

/// Whether a transcript addresses one of the wake names (case-insensitive
/// whole-word match).
pub fn transcript_matches_wake_name(transcript: &str, wake_names: &[String]) -> bool {
    if wake_names.is_empty() {
        return false;
    }
    let lower = transcript.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    wake_names.iter().any(|name| {
        let name_words: Vec<&str> = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        if name_words.is_empty() {
            return false;
        }
        words
            .windows(name_words.len())
            .any(|window| window == name_words.as_slice())
    })
}

/// Whether speaker-start events may interrupt active realtime playback.
/// Wake-name gating disables barge-in; otherwise `bargeIn` config wins,
/// defaulting to interrupt-on-input.
pub fn resolve_barge_in_enabled(
    realtime: Option<&DiscordVoiceRealtimeConfig>,
    wake_name_required: bool,
) -> bool {
    if wake_name_required {
        return false;
    }
    realtime.and_then(|r| r.barge_in).unwrap_or(true)
}

/// Whether an active barge-in should truncate assistant playback: only after
/// the minimum playback duration (`minBargeInAudioEndMs`, 0 = immediate).
pub fn should_truncate_on_barge_in(
    playback_elapsed_ms: u64,
    realtime: Option<&DiscordVoiceRealtimeConfig>,
) -> bool {
    playback_elapsed_ms >= resolve_min_barge_in_audio_end_ms(realtime)
}

// ============================================================================
// /vc command surface
// ============================================================================

/// A parsed `/vc` voice subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcCommand {
    /// Join a voice channel (optional explicit channel id).
    Join { channel_id: Option<String> },
    /// Leave the current voice channel.
    Leave,
    /// Report voice session status.
    Status,
    /// Switch voice conversation mode ("stt-tts" | "agent-proxy" | "bidi").
    Mode { mode: String },
}

/// Valid `/vc mode` values (STT/TTS, agent-proxy talk buffer, bidi realtime).
pub const VC_MODES: &[&str] = &["stt-tts", "agent-proxy", "bidi"];

/// Parse a `/vc` text command (`/vc join [channel]`, `/vc leave`,
/// `/vc status`, `/vc mode <mode>`). `/vc` alone reports status.
pub fn parse_vc_command(text: &str) -> Option<VcCommand> {
    let parsed = super::discord::parse_discord_text_command(text)?;
    if parsed.name != "vc" && parsed.name != "voice" {
        return None;
    }
    let args = parsed.args_raw.unwrap_or_default();
    let mut parts = args.split_whitespace();
    match parts.next() {
        None => Some(VcCommand::Status),
        Some("join") => {
            let channel_id = parts
                .next()
                .map(|raw| {
                    raw.trim_start_matches("<#")
                        .trim_end_matches('>')
                        .to_string()
                })
                .filter(|id| !id.is_empty());
            Some(VcCommand::Join { channel_id })
        }
        Some("leave") => Some(VcCommand::Leave),
        Some("status") => Some(VcCommand::Status),
        Some("mode") => {
            let mode = parts.next()?.to_lowercase();
            if VC_MODES.contains(&mode.as_str()) {
                Some(VcCommand::Mode { mode })
            } else {
                None
            }
        }
        Some(_) => None,
    }
}

/// Per-account voice connection group id (per-account voice isolation).
pub fn voice_connection_group(account_id: &str) -> String {
    format!("mylobster:{}", account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_ref(guild: &str, channel: &str) -> DiscordVoiceChannelRef {
        DiscordVoiceChannelRef {
            guild_id: guild.to_string(),
            channel_id: channel.to_string(),
        }
    }

    #[test]
    fn allowed_channels_unset_allows_all() {
        assert!(is_voice_channel_allowed(None, "g1", "c1"));
        let allowed = vec![channel_ref("g1", "c1")];
        assert!(is_voice_channel_allowed(Some(&allowed), "g1", "c1"));
        assert!(!is_voice_channel_allowed(Some(&allowed), "g1", "c2"));
        assert!(!is_voice_channel_allowed(Some(&allowed), "g2", "c1"));
        let empty: Vec<DiscordVoiceChannelRef> = Vec::new();
        assert!(!is_voice_channel_allowed(Some(&empty), "g1", "c1"));
    }

    #[test]
    fn follow_users_policy() {
        let voice = DiscordVoiceConfig {
            follow_users: Some(vec!["42".to_string()]),
            ..Default::default()
        };
        assert!(should_follow_user(Some(&voice), "42"));
        assert!(!should_follow_user(Some(&voice), "43"));
        let disabled = DiscordVoiceConfig {
            follow_users_enabled: Some(false),
            follow_users: Some(vec!["42".to_string()]),
            ..Default::default()
        };
        assert!(!should_follow_user(Some(&disabled), "42"));
        assert!(!should_follow_user(None, "42"));
    }

    #[test]
    fn consult_policy_defaults() {
        // agent-proxy forces the agent brain by default.
        assert_eq!(resolve_voice_consult_policy(None, true), "always");
        assert_eq!(resolve_voice_consult_policy(None, false), "auto");
        let auto = DiscordVoiceRealtimeConfig {
            consult_policy: Some("auto".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_voice_consult_policy(Some(&auto), true), "auto");
    }

    #[test]
    fn wake_name_resolution_and_matching() {
        let names = resolve_voice_wake_names(None, Some("Fany"), "83a45c9e");
        assert_eq!(names, vec!["fany".to_string()]);
        let fallback = resolve_voice_wake_names(None, None, "agent-1");
        assert_eq!(fallback, vec!["agent-1".to_string()]);
        let configured = DiscordVoiceRealtimeConfig {
            wake_names: Some(vec!["Lobster Bot".to_string(), " ".to_string()]),
            ..Default::default()
        };
        let names = resolve_voice_wake_names(Some(&configured), Some("Fany"), "x");
        assert_eq!(names, vec!["lobster bot".to_string()]);
        assert!(transcript_matches_wake_name("Hey Lobster Bot, what's up?", &names));
        assert!(!transcript_matches_wake_name("hey lobster", &names));
        assert!(!transcript_matches_wake_name("anything", &[]));
    }

    #[test]
    fn wake_name_gating_disables_barge_in() {
        let realtime = DiscordVoiceRealtimeConfig {
            require_wake_name: Some(true),
            barge_in: Some(true),
            ..Default::default()
        };
        assert!(require_wake_name(Some(&realtime), true));
        assert!(!require_wake_name(Some(&realtime), false));
        assert!(!resolve_barge_in_enabled(Some(&realtime), true));
        assert!(resolve_barge_in_enabled(Some(&realtime), false));
        // Default barge-in is on.
        assert!(resolve_barge_in_enabled(None, false));
        let off = DiscordVoiceRealtimeConfig {
            barge_in: Some(false),
            ..Default::default()
        };
        assert!(!resolve_barge_in_enabled(Some(&off), false));
    }

    #[test]
    fn barge_in_min_playback() {
        assert!(!should_truncate_on_barge_in(100, None));
        assert!(should_truncate_on_barge_in(250, None));
        let immediate = DiscordVoiceRealtimeConfig {
            min_barge_in_audio_end_ms: Some(0),
            ..Default::default()
        };
        assert!(should_truncate_on_barge_in(0, Some(&immediate)));
    }

    #[test]
    fn timing_defaults() {
        assert_eq!(resolve_min_barge_in_audio_end_ms(None), 250);
        assert_eq!(resolve_capture_silence_grace_ms(None), 2_000);
        assert_eq!(resolve_voice_connect_timeout_ms(None), 30_000);
        assert_eq!(resolve_voice_reconnect_grace_ms(None), 15_000);
        assert!(resolve_dave_encryption_enabled(None));
        assert_eq!(resolve_decryption_failure_tolerance(None), 24);
        let voice = DiscordVoiceConfig {
            capture_silence_grace_ms: Some(500),
            dave_encryption: Some(false),
            ..Default::default()
        };
        assert_eq!(resolve_capture_silence_grace_ms(Some(&voice)), 500);
        assert!(!resolve_dave_encryption_enabled(Some(&voice)));
    }

    #[test]
    fn parses_vc_commands() {
        assert_eq!(parse_vc_command("/vc"), Some(VcCommand::Status));
        assert_eq!(parse_vc_command("/vc status"), Some(VcCommand::Status));
        assert_eq!(parse_vc_command("/vc leave"), Some(VcCommand::Leave));
        assert_eq!(
            parse_vc_command("/vc join"),
            Some(VcCommand::Join { channel_id: None })
        );
        assert_eq!(
            parse_vc_command("/vc join <#12345>"),
            Some(VcCommand::Join {
                channel_id: Some("12345".to_string())
            })
        );
        assert_eq!(
            parse_vc_command("/vc mode bidi"),
            Some(VcCommand::Mode {
                mode: "bidi".to_string()
            })
        );
        assert_eq!(parse_vc_command("/vc mode nonsense"), None);
        assert_eq!(parse_vc_command("/vc dance"), None);
        assert_eq!(parse_vc_command("/help"), None);
    }

    #[test]
    fn per_account_voice_isolation_group() {
        assert_eq!(voice_connection_group("default"), "mylobster:default");
        assert_ne!(voice_connection_group("a"), voice_connection_group("b"));
    }
}
