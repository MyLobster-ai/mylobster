//! Voice Call channel (Twilio/Telnyx phone calls).
//!
//! Port of OpenClaw `extensions/voice-call` at v2026.7.1 (parity rows from
//! PARITY_v2026.5.2.md + PARITY_v2026.7.1.md):
//! - `src/config.ts` — per-number inbound routing (`numbers` map keyed by
//!   E.164), effective-config resolution, `sessionScope: "per-call"` session
//!   key derivation, Telnyx config surface
//! - `src/providers/twilio-region.ts` — IE1/AU1 region-aware API hostnames
//! - `src/providers/twilio.ts` — Twilio webhook payload parsing
//!   (x-www-form-urlencoded → typed events incl. speech + DTMF)
//! - `src/webhook.ts` — transcript auto-respond decision
//! - `src/webhook/stale-call-reaper.ts` — stale stream reaper
//! - `src/webhook-replay.ts` — bounded webhook replay tracking
//! - `src/manager/store.ts` — persisted call store (SID → call metadata);
//!   the upstream plugin-state chunk store maps to a rusqlite table here
//! - `src/deep-merge.ts` — defined-values deep merge for TTS overrides
//!
//! Live wiring (the Twilio webhook HTTP server, media-stream WebSocket, and
//! Telnyx realtime media streaming) is not hosted in this module; the
//! behavior is implemented as testable pure logic plus documented
//! integration points on [`VoiceCallChannel`].

use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

// ---------------------------------------------------------------------------
// Configuration (`config.channels.extensions["voicecall"]` / `"voice-call"`)
// ---------------------------------------------------------------------------

/// Per-dialed-number inbound overrides. Keys of the `numbers` map are E.164
/// numbers. Upstream: `config.ts::VoiceCallNumberRouteConfigSchema`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct VoiceCallNumberRoute {
    /// Greeting message for inbound calls to this number.
    pub inbound_greeting: Option<String>,
    /// TTS override, deep-merged over the channel-level TTS config.
    pub tts: Option<serde_json::Value>,
    /// Agent ID to use for voice response generation for this number.
    pub agent_id: Option<String>,
    /// Optional model override for voice responses for this number.
    pub response_model: Option<String>,
    /// System prompt for voice responses for this number.
    pub response_system_prompt: Option<String>,
    /// Timeout for response generation in ms for this number.
    pub response_timeout_ms: Option<u64>,
}

/// Twilio provider settings. Upstream: `config.ts` twilio section +
/// `providers/twilio-region.ts` (region is validated against the closed
/// `us1`/`ie1`/`au1` set).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VoiceCallTwilioConfig {
    pub account_sid: Option<String>,
    pub auth_token: Option<String>,
    /// Twilio region: "us1" (default), "ie1" (Dublin), "au1" (Sydney).
    pub region: Option<String>,
}

/// Telnyx media-streaming surface. Realtime media streaming over the Telnyx
/// WebSocket is **not implemented** in the Rust port — the config surface is
/// accepted and validated so configs written for the Node gateway load
/// unchanged, and [`telnyx_media_streaming_supported`] documents the gap.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VoiceCallTelnyxConfig {
    pub api_key: Option<String>,
    pub connection_id: Option<String>,
    /// Ed25519 public key used for webhook signature verification.
    pub public_key: Option<String>,
    /// Realtime media streaming options (codec, bidirectional flags, ...).
    /// Accepted but currently inert; see [`telnyx_media_streaming_supported`].
    pub streaming: Option<serde_json::Value>,
}

/// Voice Call extension config. Upstream: `config.ts::VoiceCallConfigSchema`
/// (subset relevant to routing/session/webhook behavior).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VoiceCallExtensionConfig {
    pub enabled: Option<bool>,
    /// Active provider: "twilio", "telnyx", "plivo", or "mock".
    pub provider: Option<String>,
    /// Agent owning voice sessions (defaults to the gateway default agent).
    pub agent_id: Option<String>,
    /// "per-phone" (default) or "per-call".
    pub session_scope: Option<String>,
    /// Inbound policy: "disabled", "open", "allowlist", "pairing".
    pub inbound_policy: Option<String>,
    /// Allowlist of E.164 numbers for inbound calls.
    pub allow_from: Option<Vec<String>>,
    /// Channel-default greeting for inbound calls.
    pub inbound_greeting: Option<String>,
    /// Channel-default TTS config (opaque; deep-merged with number routes).
    pub tts: Option<serde_json::Value>,
    /// Channel-default response model / prompt / timeout.
    pub response_model: Option<String>,
    pub response_system_prompt: Option<String>,
    pub response_timeout_ms: Option<u64>,
    /// Per-dialed-number overrides keyed by E.164 number.
    pub numbers: HashMap<String, VoiceCallNumberRoute>,
    pub twilio: Option<VoiceCallTwilioConfig>,
    pub telnyx: Option<VoiceCallTelnyxConfig>,
    /// Reap unanswered calls older than this many seconds (0/None = off).
    pub stale_call_reaper_seconds: Option<u64>,
}

/// Validate provider-specific required fields, mirroring
/// `config.ts::validateVoiceCallConfig` for the Telnyx branch.
pub fn validate_voice_call_config(config: &VoiceCallExtensionConfig) -> Vec<String> {
    let mut errors = Vec::new();
    if config.provider.as_deref() == Some("telnyx") {
        let telnyx = config.telnyx.clone().unwrap_or_default();
        let has = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
        if !has(&telnyx.api_key) {
            errors.push("voicecall.telnyx.apiKey is required (or set TELNYX_API_KEY env)".into());
        }
        if !has(&telnyx.connection_id) {
            errors.push(
                "voicecall.telnyx.connectionId is required (or set TELNYX_CONNECTION_ID env)".into(),
            );
        }
        if !has(&telnyx.public_key) {
            errors.push(
                "voicecall.telnyx.publicKey is required (or set TELNYX_PUBLIC_KEY env)".into(),
            );
        }
    }
    if let Some(region) = config
        .twilio
        .as_ref()
        .and_then(|t| t.region.as_deref())
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        if TwilioRegion::parse(region).is_none() {
            errors.push(format!(
                "voicecall.twilio.region must be one of us1, ie1, au1 (got \"{region}\")"
            ));
        }
    }
    errors
}

/// Whether Telnyx realtime media streaming is wired in this build.
/// Integration point: a `webhook/realtime-handler` equivalent would consume
/// [`VoiceCallTelnyxConfig::streaming`] and bridge Telnyx media WebSocket
/// frames into the TTS/STT pipeline. Upstream: `src/media-stream.ts`.
pub const fn telnyx_media_streaming_supported() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Twilio regions (`providers/twilio-region.ts`)
// ---------------------------------------------------------------------------

/// Closed set of supported Twilio API regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwilioRegion {
    Us1,
    Ie1,
    Au1,
}

impl TwilioRegion {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "us1" => Some(Self::Us1),
            "ie1" => Some(Self::Ie1),
            "au1" => Some(Self::Au1),
            _ => None,
        }
    }

    /// Region-specific REST API hostname.
    pub fn api_hostname(self) -> &'static str {
        match self {
            Self::Us1 => "api.twilio.com",
            Self::Ie1 => "api.dublin.ie1.twilio.com",
            Self::Au1 => "api.sydney.au1.twilio.com",
        }
    }
}

/// Build the account-scoped Twilio REST base URL for a region (defaults to
/// us1). Upstream: `resolveTwilioApiBaseUrl`.
pub fn resolve_twilio_api_base_url(account_sid: &str, region: Option<TwilioRegion>) -> String {
    format!(
        "https://{}/2010-04-01/Accounts/{account_sid}",
        region.unwrap_or(TwilioRegion::Us1).api_hostname()
    )
}

/// Reject base URLs whose hostname is outside the supported region set.
/// Upstream: `requireSupportedTwilioApiHostname`.
pub fn require_supported_twilio_api_hostname(base_url: &str) -> Result<String> {
    let parsed = url::Url::parse(base_url)?;
    let hostname = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Twilio API base URL has no hostname: {base_url}"))?;
    let supported = [TwilioRegion::Us1, TwilioRegion::Ie1, TwilioRegion::Au1]
        .iter()
        .any(|r| r.api_hostname() == hostname);
    if !supported {
        anyhow::bail!("Unsupported Twilio API hostname: {hostname}");
    }
    Ok(hostname.to_string())
}

// ---------------------------------------------------------------------------
// Per-number inbound routing (`config.ts` numbers map)
// ---------------------------------------------------------------------------

/// Digits-only route key (E.164 lookup ignores formatting).
/// Upstream: `normalizePhoneRouteKey`.
pub fn normalize_phone_route_key(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

/// Resolve the `numbers` route key for a dialed number: exact key match
/// first, then digit-normalized comparison.
/// Upstream: `resolveVoiceCallNumberRouteKey`.
pub fn resolve_number_route_key(
    numbers: &HashMap<String, VoiceCallNumberRoute>,
    phone: Option<&str>,
) -> Option<String> {
    if numbers.is_empty() {
        return None;
    }
    if let Some(phone) = phone {
        if numbers.contains_key(phone) {
            return Some(phone.to_string());
        }
    }
    let normalized = normalize_phone_route_key(phone.unwrap_or_default());
    if normalized.is_empty() {
        return None;
    }
    let mut keys: Vec<&String> = numbers.keys().collect();
    keys.sort(); // deterministic across HashMap iteration order
    keys.into_iter()
        .find(|key| normalize_phone_route_key(key) == normalized)
        .cloned()
}

/// Inbound-only routing from a persisted call record: a stored
/// `numberRouteKey` wins, otherwise fall back to the dialed `to` number.
/// Outbound calls never use number routes.
/// Upstream: `resolveVoiceCallNumberRouteKeyForCall`.
pub fn resolve_number_route_key_for_call(
    direction: CallDirection,
    to: Option<&str>,
    stored_route_key: Option<&str>,
) -> Option<String> {
    if direction != CallDirection::Inbound {
        return None;
    }
    stored_route_key
        .map(str::to_string)
        .or_else(|| to.map(str::to_string))
}

/// The effective per-number profile: channel defaults with route overrides
/// applied (TTS deep-merged). Upstream: `resolveVoiceCallEffectiveConfig`.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveVoiceCallProfile {
    /// Matched key of the `numbers` map, when a route applied.
    pub number_route_key: Option<String>,
    pub inbound_greeting: Option<String>,
    pub agent_id: Option<String>,
    pub response_model: Option<String>,
    pub response_system_prompt: Option<String>,
    pub response_timeout_ms: Option<u64>,
    /// Effective TTS config (channel TTS with route TTS deep-merged on top).
    pub tts: Option<serde_json::Value>,
}

/// Deep-merge JSON objects, overlay winning wherever it defines a non-null
/// value; nested objects merge recursively. Upstream: `deep-merge.ts::
/// deepMergeDefined` (as used for per-number TTS overrides).
pub fn deep_merge_defined(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (key, overlay_value) in overlay_map {
                if overlay_value.is_null() {
                    continue;
                }
                let next = match merged.get(key) {
                    Some(existing) => deep_merge_defined(existing, overlay_value),
                    None => overlay_value.clone(),
                };
                merged.insert(key.clone(), next);
            }
            serde_json::Value::Object(merged)
        }
        (_, serde_json::Value::Null) => base.clone(),
        _ => overlay.clone(),
    }
}

/// Resolve the effective per-number profile for a dialed number (or a
/// pre-resolved route key), falling back to channel defaults.
pub fn resolve_effective_profile(
    config: &VoiceCallExtensionConfig,
    phone_or_route_key: Option<&str>,
) -> EffectiveVoiceCallProfile {
    let defaults = EffectiveVoiceCallProfile {
        number_route_key: None,
        inbound_greeting: config.inbound_greeting.clone(),
        agent_id: config.agent_id.clone(),
        response_model: config.response_model.clone(),
        response_system_prompt: config.response_system_prompt.clone(),
        response_timeout_ms: config.response_timeout_ms,
        tts: config.tts.clone(),
    };
    let Some(route_key) = resolve_number_route_key(&config.numbers, phone_or_route_key) else {
        return defaults;
    };
    let Some(route) = config.numbers.get(&route_key) else {
        return defaults;
    };
    let tts = match (&defaults.tts, &route.tts) {
        (Some(base), Some(overlay)) => Some(deep_merge_defined(base, overlay)),
        (None, Some(overlay)) => Some(overlay.clone()),
        (base, None) => base.clone(),
    };
    EffectiveVoiceCallProfile {
        number_route_key: Some(route_key),
        inbound_greeting: route.inbound_greeting.clone().or(defaults.inbound_greeting),
        agent_id: route.agent_id.clone().or(defaults.agent_id),
        response_model: route.response_model.clone().or(defaults.response_model),
        response_system_prompt: route
            .response_system_prompt
            .clone()
            .or(defaults.response_system_prompt),
        response_timeout_ms: route.response_timeout_ms.or(defaults.response_timeout_ms),
        tts,
    }
}

// ---------------------------------------------------------------------------
// Session scope (`config.ts::resolveVoiceCallSessionKey`)
// ---------------------------------------------------------------------------

/// Voice session scoping. `PerPhone` keeps one stable session per caller
/// number; `PerCall` gives every call fresh agent memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceSessionScope {
    #[default]
    #[serde(rename = "per-phone")]
    PerPhone,
    #[serde(rename = "per-call")]
    PerCall,
}

impl VoiceSessionScope {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("per-call") => Self::PerCall,
            _ => Self::PerPhone,
        }
    }
}

/// Normalize an agent id: trimmed, lowercased, empty → "main".
/// Upstream: `plugin-sdk/routing::normalizeAgentId`.
pub fn normalize_agent_id(agent_id: Option<&str>) -> String {
    let normalized = agent_id.map(str::trim).unwrap_or_default().to_lowercase();
    if normalized.is_empty() {
        "main".to_string()
    } else {
        normalized
    }
}

/// Derive the canonical voice session key.
///
/// - `per-call`: `agent:<agent>:voice:call:<callId>` — includes the call SID
///   so every call gets fresh memory.
/// - `per-phone` (default): `agent:<agent>:voice:<digits>` — stable per
///   caller number, with the call id as fallback when no phone is known.
///
/// Upstream: `resolveVoiceCallSessionKey`.
pub fn resolve_voice_call_session_key(
    agent_id: Option<&str>,
    scope: VoiceSessionScope,
    call_id: &str,
    phone: Option<&str>,
) -> String {
    let prefix = format!("agent:{}:voice", normalize_agent_id(agent_id));
    let key = match scope {
        VoiceSessionScope::PerCall => format!("{prefix}:call:{call_id}"),
        VoiceSessionScope::PerPhone => {
            let digits = normalize_phone_route_key(phone.unwrap_or_default());
            if digits.is_empty() {
                format!("{prefix}:{call_id}")
            } else {
                format!("{prefix}:{digits}")
            }
        }
    };
    key.to_lowercase()
}

// ---------------------------------------------------------------------------
// Call lifecycle types (`types.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallDirection {
    Inbound,
    Outbound,
}

/// Call lifecycle states. Upstream: `types.ts::CallStateSchema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallState {
    #[serde(rename = "initiated")]
    Initiated,
    #[serde(rename = "ringing")]
    Ringing,
    #[serde(rename = "answered")]
    Answered,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "speaking")]
    Speaking,
    #[serde(rename = "listening")]
    Listening,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "hangup-user")]
    HangupUser,
    #[serde(rename = "hangup-bot")]
    HangupBot,
    #[serde(rename = "timeout")]
    Timeout,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "no-answer")]
    NoAnswer,
    #[serde(rename = "busy")]
    Busy,
    #[serde(rename = "voicemail")]
    Voicemail,
}

impl CallState {
    /// Terminal states — the call is over. Upstream: `TerminalStates`.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::HangupUser
                | Self::HangupBot
                | Self::Timeout
                | Self::Error
                | Self::Failed
                | Self::NoAnswer
                | Self::Busy
                | Self::Voicemail
        )
    }

    /// States proving a live conversation (speech/transcription running).
    /// Inbound Twilio calls may never fire `call.answered`, so these guard
    /// the reaper. Upstream: `stale-call-reaper.ts::LiveConversationStates`.
    pub fn is_live_conversation(self) -> bool {
        matches!(self, Self::Speaking | Self::Listening)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Initiated => "initiated",
            Self::Ringing => "ringing",
            Self::Answered => "answered",
            Self::Active => "active",
            Self::Speaking => "speaking",
            Self::Listening => "listening",
            Self::Completed => "completed",
            Self::HangupUser => "hangup-user",
            Self::HangupBot => "hangup-bot",
            Self::Timeout => "timeout",
            Self::Error => "error",
            Self::Failed => "failed",
            Self::NoAnswer => "no-answer",
            Self::Busy => "busy",
            Self::Voicemail => "voicemail",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "initiated" => Self::Initiated,
            "ringing" => Self::Ringing,
            "answered" => Self::Answered,
            "active" => Self::Active,
            "speaking" => Self::Speaking,
            "listening" => Self::Listening,
            "completed" => Self::Completed,
            "hangup-user" => Self::HangupUser,
            "hangup-bot" => Self::HangupBot,
            "timeout" => Self::Timeout,
            "error" => Self::Error,
            "failed" => Self::Failed,
            "no-answer" => Self::NoAnswer,
            "busy" => Self::Busy,
            "voicemail" => Self::Voicemail,
            _ => return None,
        })
    }
}

/// Why a call ended. Upstream: `types.ts::EndReasonSchema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallEndReason {
    Completed,
    HangupUser,
    HangupBot,
    Timeout,
    Error,
    Failed,
    NoAnswer,
    Busy,
    Voicemail,
}

// ---------------------------------------------------------------------------
// Twilio webhook parsing (`providers/twilio.ts::normalizeEvent`)
// ---------------------------------------------------------------------------

/// Payload-specific portion of a webhook event.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceCallEventKind {
    Initiated,
    Ringing,
    Answered,
    /// Final speech transcript from `<Gather>` (`SpeechResult`).
    Speech { transcript: String, confidence: f64 },
    /// DTMF key presses (`Digits`).
    Dtmf { digits: String },
    Ended { reason: CallEndReason },
}

/// One normalized webhook event. Upstream: `types.ts::NormalizedEvent`
/// (base fields) + `providers/twilio.ts::normalizeEvent`.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceCallEvent {
    /// Internal call id (query override when present, else the CallSid).
    pub call_id: String,
    /// Provider call SID.
    pub provider_call_id: String,
    /// Stable provider-derived key for replay dedupe.
    pub dedupe_key: Option<String>,
    /// Per-turn nonce for `<Gather>` replay hardening.
    pub turn_token: Option<String>,
    pub direction: Option<CallDirection>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: VoiceCallEventKind,
}

/// Parse an `application/x-www-form-urlencoded` body into a flat map
/// (first value wins per key).
pub fn parse_voice_webhook_form(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        out.entry(key.into_owned()).or_insert_with(|| value.into_owned());
    }
    out
}

fn parse_twilio_direction(raw: Option<&str>) -> Option<CallDirection> {
    match raw {
        Some("inbound") => Some(CallDirection::Inbound),
        Some("outbound-api") | Some("outbound-dial") => Some(CallDirection::Outbound),
        _ => None,
    }
}

/// Confidence defaults to 0.9 when absent or malformed.
/// Upstream: `TwilioProvider.parseConfidence`.
fn parse_twilio_confidence(raw: Option<&str>) -> f64 {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return 0.9;
    };
    let numeric = trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.')
        && trimmed.chars().filter(|c| *c == '.').count() <= 1
        && !trimmed.starts_with('.')
        && !trimmed.ends_with('.');
    if numeric {
        trimmed.parse().unwrap_or(0.9)
    } else {
        0.9
    }
}

/// Map a Twilio `CallStatus` to a terminal end reason, when terminal.
/// Upstream: `mapProviderStatusToEndReason`.
pub fn map_twilio_status_to_end_reason(status: &str) -> Option<CallEndReason> {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" => Some(CallEndReason::Completed),
        "busy" => Some(CallEndReason::Busy),
        "failed" => Some(CallEndReason::Failed),
        "no-answer" => Some(CallEndReason::NoAnswer),
        "canceled" | "cancelled" => Some(CallEndReason::Completed),
        _ => None,
    }
}

/// Convert Twilio webhook params to a normalized event. Ordering matches
/// upstream: speech, then DTMF, then status transitions; unknown statuses
/// yield `None`. Upstream: `TwilioProvider.normalizeEvent`.
pub fn parse_twilio_voice_webhook_event(
    form: &HashMap<String, String>,
    call_id_override: Option<&str>,
    dedupe_key: Option<&str>,
    turn_token: Option<&str>,
) -> Option<VoiceCallEvent> {
    let call_sid = form.get("CallSid").cloned().unwrap_or_default();
    let call_id = call_id_override
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| call_sid.clone());
    let base = |kind: VoiceCallEventKind| VoiceCallEvent {
        call_id: call_id.clone(),
        provider_call_id: call_sid.clone(),
        dedupe_key: dedupe_key.map(str::to_string),
        turn_token: turn_token.map(str::to_string),
        direction: parse_twilio_direction(form.get("Direction").map(String::as_str)),
        from: form.get("From").cloned().filter(|s| !s.is_empty()),
        to: form.get("To").cloned().filter(|s| !s.is_empty()),
        kind,
    };

    if let Some(speech) = form.get("SpeechResult").filter(|s| !s.is_empty()) {
        return Some(base(VoiceCallEventKind::Speech {
            transcript: speech.clone(),
            confidence: parse_twilio_confidence(form.get("Confidence").map(String::as_str)),
        }));
    }
    if let Some(digits) = form.get("Digits").filter(|d| !d.is_empty()) {
        return Some(base(VoiceCallEventKind::Dtmf { digits: digits.clone() }));
    }
    let status = form
        .get("CallStatus")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match status.as_str() {
        "initiated" | "queued" => Some(base(VoiceCallEventKind::Initiated)),
        "ringing" => Some(base(VoiceCallEventKind::Ringing)),
        "in-progress" => Some(base(VoiceCallEventKind::Answered)),
        _ => map_twilio_status_to_end_reason(&status)
            .map(|reason| base(VoiceCallEventKind::Ended { reason })),
    }
}

// ---------------------------------------------------------------------------
// Transcript auto-respond decision (`webhook.ts::processEventWithAutoResponse`)
// ---------------------------------------------------------------------------

/// Whether a final speech transcript should trigger the agent auto-response.
///
/// Both media-stream and carrier-webhook transcripts share this handoff:
/// respond only when no explicit waiter already consumed the transcript, and
/// the call is inbound or an outbound `conversation`-mode call.
/// Upstream: `webhook.ts::processEventWithAutoResponse`.
pub fn should_auto_respond(
    direction: CallDirection,
    call_mode: Option<&str>,
    waiter_resolved: bool,
) -> bool {
    if waiter_resolved {
        return false;
    }
    direction == CallDirection::Inbound || call_mode == Some("conversation")
}

// ---------------------------------------------------------------------------
// Persisted call store (`manager/store.ts` → rusqlite)
// ---------------------------------------------------------------------------

/// Maximum retained call records (oldest pruned first).
/// Upstream: `store.ts::MAX_CALL_RECORD_EVENTS`.
pub const MAX_CALL_RECORDS: usize = 1000;

/// One persisted call record (SID → call metadata).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_id: String,
    pub provider_call_id: Option<String>,
    pub provider: String,
    pub direction: CallDirection,
    pub from: Option<String>,
    pub to: Option<String>,
    pub state: CallState,
    /// Unix ms when the call started.
    pub started_at: u64,
    /// Unix ms when answered, when it ever was.
    pub answered_at: Option<u64>,
    /// Free-form metadata (numberRouteKey, mode, sessionKey, ...).
    pub metadata: serde_json::Value,
}

/// SQLite-backed call store. The upstream store persists chunked JSON
/// records through the plugin-state runtime; the Rust port keeps one row per
/// call with the metadata as a JSON column.
pub struct VoiceCallStore {
    conn: Mutex<rusqlite::Connection>,
}

impl VoiceCallStore {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS voice_calls (
                call_id TEXT PRIMARY KEY,
                provider_call_id TEXT,
                provider TEXT NOT NULL,
                direction TEXT NOT NULL,
                from_number TEXT,
                to_number TEXT,
                state TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                answered_at INTEGER,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_voice_calls_provider_sid
                ON voice_calls(provider_call_id);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Insert or update one call record, then prune to [`MAX_CALL_RECORDS`].
    pub fn upsert(&self, record: &CallRecord) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO voice_calls
                (call_id, provider_call_id, provider, direction, from_number,
                 to_number, state, started_at, answered_at, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(call_id) DO UPDATE SET
                provider_call_id = excluded.provider_call_id,
                provider = excluded.provider,
                direction = excluded.direction,
                from_number = excluded.from_number,
                to_number = excluded.to_number,
                state = excluded.state,
                started_at = excluded.started_at,
                answered_at = excluded.answered_at,
                metadata_json = excluded.metadata_json",
            rusqlite::params![
                record.call_id,
                record.provider_call_id,
                record.provider,
                match record.direction {
                    CallDirection::Inbound => "inbound",
                    CallDirection::Outbound => "outbound",
                },
                record.from,
                record.to,
                record.state.as_str(),
                record.started_at as i64,
                record.answered_at.map(|v| v as i64),
                serde_json::to_string(&record.metadata)?,
            ],
        )?;
        conn.execute(
            "DELETE FROM voice_calls WHERE call_id IN (
                SELECT call_id FROM voice_calls
                ORDER BY started_at DESC, call_id DESC
                LIMIT -1 OFFSET ?1
            )",
            rusqlite::params![MAX_CALL_RECORDS as i64],
        )?;
        Ok(())
    }

    pub fn get(&self, call_id: &str) -> Result<Option<CallRecord>> {
        self.query_one("call_id = ?1", call_id)
    }

    /// Resolve a call by its provider SID (webhooks only carry the SID).
    pub fn get_by_provider_call_id(&self, provider_call_id: &str) -> Result<Option<CallRecord>> {
        self.query_one("provider_call_id = ?1", provider_call_id)
    }

    fn query_one(&self, predicate: &str, param: &str) -> Result<Option<CallRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT call_id, provider_call_id, provider, direction, from_number,
                    to_number, state, started_at, answered_at, metadata_json
             FROM voice_calls WHERE {predicate} LIMIT 1"
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params![param])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_record(row)?)),
            None => Ok(None),
        }
    }

    /// Non-terminal calls, oldest first (reaper input).
    pub fn active_calls(&self) -> Result<Vec<CallRecord>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT call_id, provider_call_id, provider, direction, from_number,
                    to_number, state, started_at, answered_at, metadata_json
             FROM voice_calls ORDER BY started_at ASC",
        )?;
        let mut records = Vec::new();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let record = Self::row_to_record(row)?;
            if !record.state.is_terminal() {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Mark a call terminal.
    pub fn mark_ended(&self, call_id: &str, state: CallState) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE voice_calls SET state = ?2 WHERE call_id = ?1",
            rusqlite::params![call_id, state.as_str()],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM voice_calls", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    fn row_to_record(row: &rusqlite::Row<'_>) -> Result<CallRecord> {
        let direction: String = row.get(3)?;
        let state: String = row.get(6)?;
        let metadata: String = row.get(9)?;
        Ok(CallRecord {
            call_id: row.get(0)?,
            provider_call_id: row.get(1)?,
            provider: row.get(2)?,
            direction: if direction == "outbound" {
                CallDirection::Outbound
            } else {
                CallDirection::Inbound
            },
            from: row.get(4)?,
            to: row.get(5)?,
            state: CallState::parse(&state)
                .ok_or_else(|| anyhow::anyhow!("unknown call state: {state}"))?,
            started_at: row.get::<_, i64>(7)?.max(0) as u64,
            answered_at: row.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64),
            metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
        })
    }
}

// ---------------------------------------------------------------------------
// Stale stream reaper (`webhook/stale-call-reaper.ts`)
// ---------------------------------------------------------------------------

/// Sweep interval the background loop should use (documented parity with
/// upstream's `CHECK_INTERVAL_MS`).
pub const STALE_CALL_CHECK_INTERVAL_MS: u64 = 30_000;

/// Select calls to reap: unanswered, non-terminal, not in a live
/// conversation state, and older than `max_age_seconds`. Inbound Twilio
/// calls may never fire `call.answered`, so speaking/listening states are
/// never reaped even without `answered_at`.
/// Upstream: `startStaleCallReaper` loop body.
pub fn select_stale_calls(calls: &[CallRecord], now_ms: u64, max_age_seconds: u64) -> Vec<String> {
    if max_age_seconds == 0 {
        return Vec::new();
    }
    let max_age_ms = max_age_seconds * 1000;
    calls
        .iter()
        .filter(|call| {
            call.answered_at.is_none()
                && !call.state.is_terminal()
                && !call.state.is_live_conversation()
                && now_ms.saturating_sub(call.started_at) > max_age_ms
        })
        .map(|call| call.call_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Webhook replay tracking (`webhook-replay.ts`)
// ---------------------------------------------------------------------------

const REPLAY_WINDOW_MS: u64 = 10 * 60 * 1000;
const REPLAY_CACHE_MAX_ENTRIES: usize = 10_000;
const REPLAY_CACHE_PRUNE_INTERVAL: u64 = 64;

/// Bounded webhook replay tracker shared by all voice-call webhook routes.
/// Keys are provider-derived idempotency keys (`dedupeKey`).
/// Upstream: `createWebhookReplayCache` / `markWebhookReplay`.
#[derive(Debug, Default)]
pub struct WebhookReplayCache {
    seen_until: HashMap<String, u64>,
    order: Vec<String>,
    calls: u64,
}

impl WebhookReplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(&mut self, now_ms: u64) {
        self.order.retain(|key| {
            let keep = self
                .seen_until
                .get(key)
                .is_some_and(|expires| *expires > now_ms);
            if !keep {
                self.seen_until.remove(key);
            }
            keep
        });
        while self.seen_until.len() > REPLAY_CACHE_MAX_ENTRIES {
            let oldest = self.order.remove(0);
            self.seen_until.remove(&oldest);
        }
    }

    /// Mark a replay key; returns `true` when the key was already seen
    /// inside the 10-minute window (i.e. this request is a replay).
    pub fn mark(&mut self, replay_key: &str, now_ms: u64) -> bool {
        self.calls += 1;
        if self.calls % REPLAY_CACHE_PRUNE_INTERVAL == 0 {
            self.prune(now_ms);
        }
        if self
            .seen_until
            .get(replay_key)
            .is_some_and(|expires| *expires > now_ms)
        {
            return true;
        }
        if !self.seen_until.contains_key(replay_key) {
            self.order.push(replay_key.to_string());
        }
        self.seen_until.insert(replay_key.to_string(), now_ms + REPLAY_WINDOW_MS);
        if self.seen_until.len() > REPLAY_CACHE_MAX_ENTRIES {
            self.prune(now_ms);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Channel plugin
// ---------------------------------------------------------------------------

pub struct VoiceCallChannel {
    enabled: bool,
    config: VoiceCallExtensionConfig,
    #[allow(dead_code)]
    replay_cache: Mutex<WebhookReplayCache>,
}

impl VoiceCallChannel {
    pub fn new(config: &Config) -> Self {
        let raw = config
            .channels
            .extensions
            .get("voicecall")
            .or_else(|| config.channels.extensions.get("voice-call"));
        let ext: VoiceCallExtensionConfig = raw
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let enabled = ext.enabled.unwrap_or(false);
        Self {
            enabled,
            config: ext,
            replay_cache: Mutex::new(WebhookReplayCache::new()),
        }
    }

    /// Effective per-number profile for a dialed number.
    #[allow(dead_code)]
    pub fn effective_profile(&self, phone: Option<&str>) -> EffectiveVoiceCallProfile {
        resolve_effective_profile(&self.config, phone)
    }

    /// Session key for a call under the configured scope.
    #[allow(dead_code)]
    pub fn session_key(&self, call_id: &str, phone: Option<&str>) -> String {
        resolve_voice_call_session_key(
            self.config.agent_id.as_deref(),
            VoiceSessionScope::parse(self.config.session_scope.as_deref()),
            call_id,
            phone,
        )
    }

    /// Record a webhook idempotency key; `true` means drop as replay.
    #[allow(dead_code)]
    pub fn is_webhook_replay(&self, dedupe_key: &str) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        self.replay_cache.lock().mark(dedupe_key, now_ms)
    }
}

#[async_trait]
impl ChannelPlugin for VoiceCallChannel {
    fn id(&self) -> &str {
        "voicecall"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Voice Call".to_string(),
            description: "Phone voice-call channel (Twilio/Telnyx)".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![ChannelCapability::Voice]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if self.enabled {
            for error in validate_voice_call_config(&self.config) {
                tracing::warn!("voice-call config: {error}");
            }
            info!(
                provider = self.config.provider.as_deref().unwrap_or("none"),
                numbers = self.config.numbers.len(),
                "Voice Call channel started"
            );
        }
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        // Outbound call placement requires the live provider stack (Twilio
        // Calls API + webhook server); see module docs for the integration
        // point. Upstream: `manager/outbound.ts`.
        info!(to = to, "VoiceCall: outbound calls not wired in this build");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_numbers() -> VoiceCallExtensionConfig {
        let raw = serde_json::json!({
            "enabled": true,
            "provider": "twilio",
            "agentId": "Concierge",
            "sessionScope": "per-phone",
            "inboundGreeting": "Hello from the default line",
            "responseModel": "claude-sonnet",
            "tts": { "provider": "openai", "voice": "alloy", "speed": 1.0 },
            "numbers": {
                "+15550001234": {
                    "inboundGreeting": "Support line, how can I help?",
                    "agentId": "support",
                    "responseModel": "gpt-5",
                    "tts": { "voice": "verse" }
                },
                "+15559998888": {
                    "responseSystemPrompt": "Sales persona."
                }
            }
        });
        serde_json::from_value(raw).unwrap()
    }

    // --- per-number routing ---

    #[test]
    fn route_key_exact_and_normalized_lookup() {
        let config = config_with_numbers();
        assert_eq!(
            resolve_number_route_key(&config.numbers, Some("+15550001234")).as_deref(),
            Some("+15550001234")
        );
        // Formatting differences resolve through digit normalization.
        assert_eq!(
            resolve_number_route_key(&config.numbers, Some("1 (555) 000-1234")).as_deref(),
            Some("+15550001234")
        );
        assert_eq!(resolve_number_route_key(&config.numbers, Some("+15550000000")), None);
        assert_eq!(resolve_number_route_key(&config.numbers, None), None);
        assert_eq!(resolve_number_route_key(&HashMap::new(), Some("+15550001234")), None);
    }

    #[test]
    fn route_key_for_call_is_inbound_only() {
        assert_eq!(
            resolve_number_route_key_for_call(
                CallDirection::Inbound,
                Some("+15550001234"),
                Some("stored-key"),
            )
            .as_deref(),
            Some("stored-key")
        );
        assert_eq!(
            resolve_number_route_key_for_call(CallDirection::Inbound, Some("+15550001234"), None)
                .as_deref(),
            Some("+15550001234")
        );
        assert_eq!(
            resolve_number_route_key_for_call(CallDirection::Outbound, Some("+15550001234"), None),
            None
        );
    }

    #[test]
    fn effective_profile_overrides_with_fallback_to_defaults() {
        let config = config_with_numbers();
        let profile = resolve_effective_profile(&config, Some("+15550001234"));
        assert_eq!(profile.number_route_key.as_deref(), Some("+15550001234"));
        assert_eq!(profile.inbound_greeting.as_deref(), Some("Support line, how can I help?"));
        assert_eq!(profile.agent_id.as_deref(), Some("support"));
        assert_eq!(profile.response_model.as_deref(), Some("gpt-5"));
        // TTS deep-merge: route voice override, channel provider/speed kept.
        let tts = profile.tts.unwrap();
        assert_eq!(tts["voice"], "verse");
        assert_eq!(tts["provider"], "openai");
        assert_eq!(tts["speed"], 1.0);

        // A route that only sets one field inherits the rest.
        let sparse = resolve_effective_profile(&config, Some("+15559998888"));
        assert_eq!(sparse.response_system_prompt.as_deref(), Some("Sales persona."));
        assert_eq!(sparse.inbound_greeting.as_deref(), Some("Hello from the default line"));
        assert_eq!(sparse.agent_id.as_deref(), Some("Concierge"));

        // Unrouted numbers get pure channel defaults.
        let fallback = resolve_effective_profile(&config, Some("+15550000000"));
        assert_eq!(fallback.number_route_key, None);
        assert_eq!(fallback.inbound_greeting.as_deref(), Some("Hello from the default line"));
        assert_eq!(fallback.response_model.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn deep_merge_skips_nulls_and_merges_nested() {
        let base = serde_json::json!({"a": 1, "nested": {"x": 1, "y": 2}});
        let overlay = serde_json::json!({"a": null, "nested": {"y": 3}, "b": true});
        let merged = deep_merge_defined(&base, &overlay);
        assert_eq!(merged, serde_json::json!({"a": 1, "nested": {"x": 1, "y": 3}, "b": true}));
    }

    // --- session scope ---

    #[test]
    fn session_key_per_phone_is_stable_per_number() {
        let key = resolve_voice_call_session_key(
            Some("Concierge"),
            VoiceSessionScope::PerPhone,
            "CA123",
            Some("+1 (555) 000-1234"),
        );
        assert_eq!(key, "agent:concierge:voice:15550001234");
        // Same number, different call → same session.
        let again = resolve_voice_call_session_key(
            Some("Concierge"),
            VoiceSessionScope::PerPhone,
            "CA456",
            Some("+15550001234"),
        );
        assert_eq!(key, again);
        // No phone → falls back to the call id.
        let fallback = resolve_voice_call_session_key(
            Some("Concierge"),
            VoiceSessionScope::PerPhone,
            "CA789",
            None,
        );
        assert_eq!(fallback, "agent:concierge:voice:ca789");
    }

    #[test]
    fn session_key_per_call_includes_call_sid() {
        let first = resolve_voice_call_session_key(
            None,
            VoiceSessionScope::PerCall,
            "CA123",
            Some("+15550001234"),
        );
        assert_eq!(first, "agent:main:voice:call:ca123");
        let second = resolve_voice_call_session_key(
            None,
            VoiceSessionScope::PerCall,
            "CA456",
            Some("+15550001234"),
        );
        assert_ne!(first, second);
    }

    #[test]
    fn session_scope_parsing_defaults_to_per_phone() {
        assert_eq!(VoiceSessionScope::parse(Some("per-call")), VoiceSessionScope::PerCall);
        assert_eq!(VoiceSessionScope::parse(Some("per-phone")), VoiceSessionScope::PerPhone);
        assert_eq!(VoiceSessionScope::parse(Some("bogus")), VoiceSessionScope::PerPhone);
        assert_eq!(VoiceSessionScope::parse(None), VoiceSessionScope::PerPhone);
    }

    // --- twilio regions ---

    #[test]
    fn region_hostnames() {
        assert_eq!(TwilioRegion::parse("us1"), Some(TwilioRegion::Us1));
        assert_eq!(TwilioRegion::parse("IE1"), Some(TwilioRegion::Ie1));
        assert_eq!(TwilioRegion::parse("au1"), Some(TwilioRegion::Au1));
        assert_eq!(TwilioRegion::parse("eu1"), None);
        assert_eq!(
            resolve_twilio_api_base_url("AC1", Some(TwilioRegion::Ie1)),
            "https://api.dublin.ie1.twilio.com/2010-04-01/Accounts/AC1"
        );
        assert_eq!(
            resolve_twilio_api_base_url("AC1", None),
            "https://api.twilio.com/2010-04-01/Accounts/AC1"
        );
        assert_eq!(
            require_supported_twilio_api_hostname(
                "https://api.sydney.au1.twilio.com/2010-04-01/Accounts/AC1"
            )
            .unwrap(),
            "api.sydney.au1.twilio.com"
        );
        assert!(require_supported_twilio_api_hostname("https://evil.example/x").is_err());
    }

    // --- webhook parsing ---

    #[test]
    fn parses_speech_event_with_confidence() {
        let form = parse_voice_webhook_form(
            "CallSid=CA1&SpeechResult=book+a+table&Confidence=0.82&Direction=inbound&From=%2B15550001234&To=%2B15559998888",
        );
        let event = parse_twilio_voice_webhook_event(&form, None, Some("dk1"), Some("turn9")).unwrap();
        assert_eq!(event.call_id, "CA1");
        assert_eq!(event.provider_call_id, "CA1");
        assert_eq!(event.direction, Some(CallDirection::Inbound));
        assert_eq!(event.from.as_deref(), Some("+15550001234"));
        assert_eq!(event.dedupe_key.as_deref(), Some("dk1"));
        assert_eq!(event.turn_token.as_deref(), Some("turn9"));
        match event.kind {
            VoiceCallEventKind::Speech { ref transcript, confidence } => {
                assert_eq!(transcript, "book a table");
                assert!((confidence - 0.82).abs() < f64::EPSILON);
            }
            ref other => panic!("expected speech, got {other:?}"),
        }
        // Malformed confidence defaults to 0.9.
        let form = parse_voice_webhook_form("CallSid=CA1&SpeechResult=hi&Confidence=high");
        let event = parse_twilio_voice_webhook_event(&form, None, None, None).unwrap();
        assert!(matches!(
            event.kind,
            VoiceCallEventKind::Speech { confidence, .. } if (confidence - 0.9).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn parses_dtmf_and_status_events() {
        let form = parse_voice_webhook_form("CallSid=CA2&Digits=42%23&Direction=outbound-api");
        let event = parse_twilio_voice_webhook_event(&form, Some("call-9"), None, None).unwrap();
        assert_eq!(event.call_id, "call-9");
        assert_eq!(event.provider_call_id, "CA2");
        assert_eq!(event.direction, Some(CallDirection::Outbound));
        assert_eq!(event.kind, VoiceCallEventKind::Dtmf { digits: "42#".to_string() });

        for (status, expected) in [
            ("initiated", VoiceCallEventKind::Initiated),
            ("queued", VoiceCallEventKind::Initiated),
            ("ringing", VoiceCallEventKind::Ringing),
            ("in-progress", VoiceCallEventKind::Answered),
            ("completed", VoiceCallEventKind::Ended { reason: CallEndReason::Completed }),
            ("busy", VoiceCallEventKind::Ended { reason: CallEndReason::Busy }),
            ("no-answer", VoiceCallEventKind::Ended { reason: CallEndReason::NoAnswer }),
            ("failed", VoiceCallEventKind::Ended { reason: CallEndReason::Failed }),
        ] {
            let form = parse_voice_webhook_form(&format!("CallSid=CA3&CallStatus={status}"));
            let event = parse_twilio_voice_webhook_event(&form, None, None, None).unwrap();
            assert_eq!(event.kind, expected, "status {status}");
        }
        // Unknown status → no event.
        let form = parse_voice_webhook_form("CallSid=CA3&CallStatus=mystery");
        assert!(parse_twilio_voice_webhook_event(&form, None, None, None).is_none());
    }

    // --- auto-respond ---

    #[test]
    fn auto_respond_decision() {
        assert!(should_auto_respond(CallDirection::Inbound, None, false));
        assert!(should_auto_respond(CallDirection::Outbound, Some("conversation"), false));
        assert!(!should_auto_respond(CallDirection::Outbound, Some("notify"), false));
        assert!(!should_auto_respond(CallDirection::Outbound, None, false));
        // An explicit waiter already consumed the transcript.
        assert!(!should_auto_respond(CallDirection::Inbound, None, true));
    }

    // --- call store ---

    fn call_record(call_id: &str, state: CallState, started_at: u64) -> CallRecord {
        CallRecord {
            call_id: call_id.to_string(),
            provider_call_id: Some(format!("SID-{call_id}")),
            provider: "twilio".to_string(),
            direction: CallDirection::Inbound,
            from: Some("+15550001234".to_string()),
            to: Some("+15559998888".to_string()),
            state,
            started_at,
            answered_at: None,
            metadata: serde_json::json!({"numberRouteKey": "+15559998888"}),
        }
    }

    #[test]
    fn store_roundtrip_and_sid_lookup() {
        let store = VoiceCallStore::open_in_memory().unwrap();
        let record = call_record("c1", CallState::Ringing, 1_000);
        store.upsert(&record).unwrap();
        assert_eq!(store.get("c1").unwrap().unwrap(), record);
        assert_eq!(
            store.get_by_provider_call_id("SID-c1").unwrap().unwrap().call_id,
            "c1"
        );
        assert!(store.get("missing").unwrap().is_none());

        // Metadata (numberRouteKey) survives the roundtrip for inbound
        // route restoration.
        let loaded = store.get("c1").unwrap().unwrap();
        assert_eq!(loaded.metadata["numberRouteKey"], "+15559998888");

        // Upsert updates in place; mark_ended flips to terminal.
        store.mark_ended("c1", CallState::Completed).unwrap();
        assert!(store.get("c1").unwrap().unwrap().state.is_terminal());
        assert!(store.active_calls().unwrap().is_empty());
    }

    #[test]
    fn store_prunes_oldest_beyond_cap() {
        let store = VoiceCallStore::open_in_memory().unwrap();
        for i in 0..(MAX_CALL_RECORDS + 5) {
            store
                .upsert(&call_record(&format!("c{i:05}"), CallState::Completed, i as u64))
                .unwrap();
        }
        assert_eq!(store.count().unwrap(), MAX_CALL_RECORDS);
        // Oldest rows were pruned; newest kept.
        assert!(store.get("c00000").unwrap().is_none());
        assert!(store.get(&format!("c{:05}", MAX_CALL_RECORDS + 4)).unwrap().is_some());
    }

    // --- stale reaper ---

    #[test]
    fn reaper_selects_only_stranded_calls() {
        let now = 200_000;
        let calls = vec![
            // Stranded: old, unanswered, ringing.
            call_record("stale", CallState::Ringing, 10_000),
            // Live conversation states are never reaped, even unanswered.
            call_record("speaking", CallState::Speaking, 10_000),
            call_record("listening", CallState::Listening, 10_000),
            // Answered calls are never reaped.
            CallRecord {
                answered_at: Some(11_000),
                ..call_record("answered", CallState::Active, 10_000)
            },
            // Terminal calls are skipped.
            call_record("done", CallState::Completed, 10_000),
            // Too young.
            call_record("young", CallState::Ringing, now - 5_000),
        ];
        assert_eq!(select_stale_calls(&calls, now, 60), vec!["stale".to_string()]);
        // Reaper disabled with zero max age.
        assert!(select_stale_calls(&calls, now, 0).is_empty());
    }

    // --- replay cache ---

    #[test]
    fn replay_cache_marks_within_window() {
        let mut cache = WebhookReplayCache::new();
        assert!(!cache.mark("evt-1", 0));
        assert!(cache.mark("evt-1", 1_000));
        // After the 10-minute window the key is fresh again.
        assert!(!cache.mark("evt-1", REPLAY_WINDOW_MS + 1));
        assert!(!cache.mark("evt-2", REPLAY_WINDOW_MS + 1));
    }

    #[test]
    fn replay_cache_prunes_expired_entries() {
        let mut cache = WebhookReplayCache::new();
        for i in 0..100 {
            cache.mark(&format!("evt-{i}"), 0);
        }
        // Advance beyond the TTL; the periodic prune (every 64 calls) clears
        // expired keys.
        for i in 100..200 {
            cache.mark(&format!("evt-{i}"), REPLAY_WINDOW_MS + 1);
        }
        assert!(cache.seen_until.len() <= 100 + REPLAY_CACHE_PRUNE_INTERVAL as usize);
        assert!(!cache.seen_until.contains_key("evt-0"));
    }

    // --- config validation / telnyx surface ---

    #[test]
    fn telnyx_config_validation() {
        let mut config = VoiceCallExtensionConfig {
            provider: Some("telnyx".to_string()),
            ..Default::default()
        };
        let errors = validate_voice_call_config(&config);
        assert_eq!(errors.len(), 3);
        config.telnyx = Some(VoiceCallTelnyxConfig {
            api_key: Some("k".to_string()),
            connection_id: Some("c".to_string()),
            public_key: Some("p".to_string()),
            streaming: Some(serde_json::json!({"codec": "PCMU"})),
        });
        assert!(validate_voice_call_config(&config).is_empty());
        // Media streaming is a documented stub in this build.
        assert!(!telnyx_media_streaming_supported());
    }

    #[test]
    fn twilio_region_validation() {
        let config = VoiceCallExtensionConfig {
            provider: Some("twilio".to_string()),
            twilio: Some(VoiceCallTwilioConfig {
                account_sid: Some("AC1".to_string()),
                auth_token: Some("t".to_string()),
                region: Some("mars1".to_string()),
            }),
            ..Default::default()
        };
        let errors = validate_voice_call_config(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("mars1"));
    }
}
