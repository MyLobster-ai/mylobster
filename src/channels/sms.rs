//! Twilio SMS channel.
//!
//! Port of OpenClaw `extensions/sms` at v2026.7.1 (webhook diagnostics +
//! proof follow-ups landed in v2026.6.1):
//! - `src/phone.ts` — phone normalization / allow-from entries
//! - `src/twilio.ts` — X-Twilio-Signature validation (HMAC-SHA1), form
//!   parsing, Messages API request builder, API error envelope
//! - `src/webhook.ts` — inbound webhook decision pipeline (rate limit,
//!   signature, AccountSid match, replay dedupe)
//! - `src/status.ts` — webhook diagnostics probe ("proof follow-ups"):
//!   compares the Twilio-configured smsUrl/method against the expected
//!   public webhook URL and classifies recent inbound message-log entries
//!   (Twilio error 11200 = webhook reachability failure)
//! - `src/send.ts` — plain-text conversion + chunked outbound sends
//!
//! The live HTTP webhook server is owned by the gateway; this module keeps
//! all decisions as pure, testable logic. `evaluate_sms_webhook` is the
//! single entry point a webhook route needs.

use crate::config::Config;
use crate::gateway::GatewayState;

use super::normalize::{ChatType, NormalizedMessage, NormalizedSender};
use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

// ---------------------------------------------------------------------------
// Configuration (`config.channels.extensions["sms"]`)
// ---------------------------------------------------------------------------

const DEFAULT_WEBHOOK_PATH: &str = "/webhooks/sms";
const DEFAULT_TEXT_CHUNK_LIMIT: usize = 1500;

/// Per-account SMS config fields. Upstream: `src/types.ts::SmsChannelConfigFields`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SmsAccountFields {
    pub enabled: Option<bool>,
    pub account_sid: Option<String>,
    pub auth_token: Option<String>,
    pub from_number: Option<String>,
    pub messaging_service_sid: Option<String>,
    pub default_to: Option<String>,
    pub webhook_path: Option<String>,
    pub public_webhook_url: Option<String>,
    pub dangerously_disable_signature_validation: Option<bool>,
    pub dm_policy: Option<String>,
    pub allow_from: Option<serde_json::Value>,
    pub text_chunk_limit: Option<serde_json::Value>,
}

/// Channel-level SMS extension config. Upstream: `src/types.ts::SmsChannelConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SmsExtensionConfig {
    #[serde(flatten)]
    pub base: SmsAccountFields,
    pub accounts: HashMap<String, SmsAccountFields>,
    pub default_account: Option<String>,
}

/// Fully resolved SMS account. Upstream: `src/types.ts::ResolvedSmsAccount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSmsAccount {
    pub account_id: String,
    pub enabled: bool,
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub messaging_service_sid: String,
    pub default_to: String,
    pub webhook_path: String,
    pub public_webhook_url: String,
    pub dangerously_disable_signature_validation: bool,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
    pub text_chunk_limit: usize,
}

fn parse_allow_from(raw: Option<&serde_json::Value>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let entries: Vec<String> = match raw {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        serde_json::Value::String(s) => s.split(',').map(|p| p.to_string()).collect(),
        other => vec![other.to_string()],
    };
    entries
        .iter()
        .map(|e| normalize_sms_allow_from(e))
        .filter(|e| !e.is_empty())
        .collect()
}

fn parse_text_chunk_limit(raw: Option<&serde_json::Value>) -> usize {
    match raw {
        Some(serde_json::Value::Number(n)) => match n.as_u64() {
            Some(v) if v > 0 => v as usize,
            _ => DEFAULT_TEXT_CHUNK_LIMIT,
        },
        Some(serde_json::Value::String(s)) if s.trim().chars().all(|c| c.is_ascii_digit()) => s
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TEXT_CHUNK_LIMIT),
        _ => DEFAULT_TEXT_CHUNK_LIMIT,
    }
}

fn merge_field(account: Option<&String>, base: Option<&String>, env: Option<String>) -> String {
    account
        .or(base)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or(env)
        .unwrap_or_default()
}

/// Resolve one SMS account by merging channel-level fields, per-account
/// overrides, and (for the default account) `TWILIO_*` env fallbacks.
/// Upstream: `src/accounts.ts::resolveSmsAccount`.
pub fn resolve_sms_account(cfg: &SmsExtensionConfig, account_id: Option<&str>) -> ResolvedSmsAccount {
    let id = account_id
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.default_account.as_ref().map(|s| s.trim().to_lowercase()))
        .unwrap_or_else(|| "default".to_string());
    let account = cfg.accounts.get(&id).cloned().unwrap_or_default();
    let base = &cfg.base;
    let use_env = id == "default";
    let env = |key: &str| -> Option<String> {
        if use_env {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        } else {
            None
        }
    };

    ResolvedSmsAccount {
        enabled: account.enabled.or(base.enabled).unwrap_or(true),
        account_sid: merge_field(
            account.account_sid.as_ref(),
            base.account_sid.as_ref(),
            env("TWILIO_ACCOUNT_SID"),
        ),
        auth_token: merge_field(
            account.auth_token.as_ref(),
            base.auth_token.as_ref(),
            env("TWILIO_AUTH_TOKEN"),
        ),
        from_number: normalize_sms_phone_number(&merge_field(
            account.from_number.as_ref(),
            base.from_number.as_ref(),
            env("TWILIO_PHONE_NUMBER").or_else(|| env("TWILIO_SMS_FROM")),
        )),
        messaging_service_sid: merge_field(
            account.messaging_service_sid.as_ref(),
            base.messaging_service_sid.as_ref(),
            env("TWILIO_MESSAGING_SERVICE_SID"),
        ),
        default_to: normalize_sms_phone_number(&merge_field(
            account.default_to.as_ref(),
            base.default_to.as_ref(),
            None,
        )),
        webhook_path: {
            let path = merge_field(
                account.webhook_path.as_ref(),
                base.webhook_path.as_ref(),
                env("SMS_WEBHOOK_PATH"),
            );
            if path.is_empty() {
                DEFAULT_WEBHOOK_PATH.to_string()
            } else {
                path
            }
        },
        public_webhook_url: merge_field(
            account.public_webhook_url.as_ref(),
            base.public_webhook_url.as_ref(),
            env("SMS_PUBLIC_WEBHOOK_URL"),
        ),
        dangerously_disable_signature_validation: account
            .dangerously_disable_signature_validation
            .or(base.dangerously_disable_signature_validation)
            .unwrap_or(false),
        dm_policy: account
            .dm_policy
            .or_else(|| base.dm_policy.clone())
            .unwrap_or_else(|| "pairing".to_string()),
        allow_from: parse_allow_from(account.allow_from.as_ref().or(base.allow_from.as_ref())),
        text_chunk_limit: parse_text_chunk_limit(
            account
                .text_chunk_limit
                .as_ref()
                .or(base.text_chunk_limit.as_ref()),
        ),
        account_id: id,
    }
}

// ---------------------------------------------------------------------------
// Phone normalization (`src/phone.ts`)
// ---------------------------------------------------------------------------

/// Strip `sms:` / `twilio-sms:` prefixes, force a leading `+`, and drop
/// every non-digit. Upstream: `normalizeSmsPhoneNumber`.
pub fn normalize_sms_phone_number(raw: &str) -> String {
    let mut trimmed = raw.trim();
    for prefix in ["sms:", "twilio-sms:"] {
        if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
            trimmed = trimmed[prefix.len()..].trim();
            break;
        }
    }
    if trimmed.is_empty() {
        return String::new();
    }
    let with_plus = if trimmed.starts_with('+') {
        trimmed.to_string()
    } else {
        format!("+{trimmed}")
    };
    with_plus
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

/// E.164-shaped check after normalization. Upstream: `looksLikeSmsPhoneNumber`.
pub fn looks_like_sms_phone_number(raw: &str) -> bool {
    let normalized = normalize_sms_phone_number(raw);
    let Some(rest) = normalized.strip_prefix('+') else {
        return false;
    };
    let len = rest.len();
    (7..=15).contains(&len)
        && rest.chars().all(|c| c.is_ascii_digit())
        && !rest.starts_with('0')
}

/// Normalize an allow-from entry, preserving the `*` wildcard.
/// Upstream: `normalizeSmsAllowFrom`.
pub fn normalize_sms_allow_from(raw: &str) -> String {
    if raw.trim() == "*" {
        return "*".to_string();
    }
    normalize_sms_phone_number(raw).to_lowercase()
}

// ---------------------------------------------------------------------------
// Form parsing + Twilio webhook signature (`src/twilio.ts`)
// ---------------------------------------------------------------------------

/// Parse an `application/x-www-form-urlencoded` body into a flat map
/// (first value wins per key). Upstream: `parseTwilioFormBody`.
pub fn parse_twilio_form_body(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        out.entry(key.into_owned()).or_insert_with(|| value.into_owned());
    }
    out
}

// --- SHA-1 / HMAC-SHA1 ---
// Twilio's X-Twilio-Signature is HMAC-SHA1 over the URL + sorted form pairs.
// The `sha2` crate does not ship SHA-1, so a compact block implementation
// lives here (validated against RFC 3174 / RFC 2202 vectors in tests).

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let ml = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..20].copy_from_slice(&sha1(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 20);
    for b in key_block.iter() {
        inner.push(b ^ 0x36);
    }
    inner.extend_from_slice(message);
    for b in key_block.iter() {
        outer.push(b ^ 0x5c);
    }
    outer.extend_from_slice(&sha1(&inner));
    sha1(&outer)
}

/// Constant-time byte-string comparison (mirror of Node `timingSafeEqual`
/// usage in `safeEqual`).
fn safe_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Compute the expected X-Twilio-Signature: base64(HMAC-SHA1(authToken,
/// url + concat(sortedKey + value))). Upstream: `computeTwilioSignature`.
pub fn compute_twilio_signature(
    url: &str,
    auth_token: &str,
    form: &HashMap<String, String>,
) -> String {
    let mut keys: Vec<&String> = form.keys().collect();
    keys.sort();
    let mut data = String::from(url);
    for key in keys {
        data.push_str(key);
        data.push_str(form.get(key).map(String::as_str).unwrap_or(""));
    }
    base64::engine::general_purpose::STANDARD.encode(hmac_sha1(auth_token.as_bytes(), data.as_bytes()))
}

/// Validate an inbound webhook signature. Upstream: `verifyTwilioSignature`.
pub fn verify_twilio_signature(
    signature: Option<&str>,
    url: &str,
    auth_token: &str,
    form: &HashMap<String, String>,
) -> bool {
    let Some(signature) = signature else {
        return false;
    };
    if signature.is_empty() || url.is_empty() || auth_token.is_empty() {
        return false;
    }
    safe_eq(signature, &compute_twilio_signature(url, auth_token, form))
}

/// Twilio signs the URL it actually requested. When the configured public
/// webhook URL has no query, append the live request's query string so
/// signatures still match. Upstream: `resolveTwilioWebhookSignatureUrl`.
pub fn resolve_twilio_webhook_signature_url(public_webhook_url: &str, request_search: &str) -> String {
    let hash_index = public_webhook_url.find('#');
    let before_hash = match hash_index {
        Some(idx) => &public_webhook_url[..idx],
        None => public_webhook_url,
    };
    if before_hash.contains('?') {
        return public_webhook_url.to_string();
    }
    if request_search.is_empty() {
        return public_webhook_url.to_string();
    }
    match hash_index {
        None => format!("{public_webhook_url}{request_search}"),
        Some(idx) => format!(
            "{}{}{}",
            &public_webhook_url[..idx],
            request_search,
            &public_webhook_url[idx..]
        ),
    }
}

// ---------------------------------------------------------------------------
// Inbound message mapping (`src/twilio.ts::buildTwilioInboundMessage`)
// ---------------------------------------------------------------------------

/// Typed inbound SMS payload. Upstream: `src/types.ts::SmsInboundMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsInboundMessage {
    pub message_sid: String,
    pub account_sid: String,
    pub from: String,
    pub to: String,
    pub body: String,
}

/// Build a typed inbound message from a parsed Twilio form, or `None` when
/// mandatory fields are missing. Upstream: `buildTwilioInboundMessage`.
pub fn build_twilio_inbound_message(form: &HashMap<String, String>) -> Option<SmsInboundMessage> {
    let trimmed = |key: &str| -> String {
        form.get(key).map(|v| v.trim().to_string()).unwrap_or_default()
    };
    let from = trimmed("From");
    let to = trimmed("To");
    let body = form.get("Body").cloned().unwrap_or_default();
    let account_sid = trimmed("AccountSid");
    let message_sid = [trimmed("MessageSid"), trimmed("SmsSid"), trimmed("SmsMessageSid")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    if from.is_empty() || to.is_empty() || body.is_empty() || message_sid.is_empty() {
        return None;
    }
    Some(SmsInboundMessage {
        message_sid,
        account_sid,
        from,
        to,
        body,
    })
}

/// Map an inbound SMS to the gateway's [`NormalizedMessage`]. SMS chats are
/// always DMs keyed by the sender's phone number.
pub fn sms_inbound_to_normalized(msg: &SmsInboundMessage, account_id: &str) -> NormalizedMessage {
    let from = normalize_sms_phone_number(&msg.from);
    NormalizedMessage {
        id: msg.message_sid.clone(),
        channel: "sms".to_string(),
        account_id: account_id.to_string(),
        chat_id: from.clone(),
        chat_name: None,
        chat_type: ChatType::Dm,
        sender: NormalizedSender {
            id: from.clone(),
            name: from,
            is_bot: false,
        },
        text: msg.body.clone(),
        attachments: Vec::new(),
        reply_to_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        raw: None,
    }
}

// ---------------------------------------------------------------------------
// Webhook decision pipeline (`src/webhook.ts`)
// ---------------------------------------------------------------------------

const RATE_LIMIT_MAX_REQUESTS: u32 = 30;
const RATE_LIMIT_WINDOW_MS: u64 = 60_000;
const RATE_LIMIT_MAX_TRACKED_KEYS: usize = 5_000;
const REPLAY_CACHE_TTL_MS: u64 = 10 * 60_000;
const REPLAY_CACHE_MAX_KEYS: usize = 10_000;

/// Fixed-window per-key rate limiter. Upstream:
/// `plugin-sdk/webhook-ingress::createFixedWindowRateLimiter` as used by the
/// SMS webhook (30 requests / 60 s, 5 000 tracked keys).
#[derive(Debug, Default)]
pub struct FixedWindowRateLimiter {
    windows: HashMap<String, (u64, u32)>,
}

impl FixedWindowRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the key is over its window budget.
    pub fn is_rate_limited(&mut self, key: &str, now_ms: u64) -> bool {
        let window_start = now_ms - (now_ms % RATE_LIMIT_WINDOW_MS);
        if self.windows.len() >= RATE_LIMIT_MAX_TRACKED_KEYS && !self.windows.contains_key(key) {
            self.windows.retain(|_, (start, _)| *start == window_start);
        }
        let entry = self.windows.entry(key.to_string()).or_insert((window_start, 0));
        if entry.0 != window_start {
            *entry = (window_start, 0);
        }
        entry.1 += 1;
        entry.1 > RATE_LIMIT_MAX_REQUESTS
    }
}

/// Bounded TTL replay cache for inbound MessageSids. Upstream:
/// `src/webhook.ts::rememberWebhookMessage` (10 min TTL, 10 000 keys).
#[derive(Debug, Default)]
pub struct SmsReplayCache {
    seen_until: HashMap<String, u64>,
    order: Vec<String>,
}

impl SmsReplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember `accountId:messageSid`; returns `false` when the message is
    /// a replay still inside its TTL window.
    pub fn remember(&mut self, account_id: &str, message_sid: &str, now_ms: u64) -> bool {
        // Opportunistic prune of expired/oldest entries.
        while let Some(oldest) = self.order.first().cloned() {
            let expired = self
                .seen_until
                .get(&oldest)
                .map(|expires| *expires <= now_ms)
                .unwrap_or(true);
            if expired || self.seen_until.len() > REPLAY_CACHE_MAX_KEYS {
                self.seen_until.remove(&oldest);
                self.order.remove(0);
            } else {
                break;
            }
        }
        let key = format!("{account_id}:{message_sid}");
        if self.seen_until.get(&key).copied().unwrap_or(0) > now_ms {
            return false;
        }
        if !self.seen_until.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.seen_until.insert(key, now_ms + REPLAY_CACHE_TTL_MS);
        true
    }
}

/// Outcome of the inbound webhook pipeline, with the HTTP status the route
/// should answer with (Twilio expects TwiML bodies). Upstream:
/// `src/webhook.ts::createSmsWebhookHandler`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsWebhookDecision {
    /// 405 — non-POST request.
    MethodNotAllowed,
    /// 429 — remote address over the fixed-window budget.
    RateLimited,
    /// 400 — body unreadable / over limits.
    InvalidBody,
    /// 403 — X-Twilio-Signature mismatch.
    InvalidSignature,
    /// 400 — mandatory SMS fields missing.
    MissingPayload,
    /// 403 — payload AccountSid does not match the configured account.
    AccountMismatch,
    /// 200 — duplicate MessageSid; acknowledged but dropped.
    Replay,
    /// 200 — accepted; dispatch this message.
    Accept(SmsInboundMessage),
}

impl SmsWebhookDecision {
    /// HTTP status code the webhook route should respond with.
    pub fn status_code(&self) -> u16 {
        match self {
            SmsWebhookDecision::MethodNotAllowed => 405,
            SmsWebhookDecision::RateLimited => 429,
            SmsWebhookDecision::InvalidBody | SmsWebhookDecision::MissingPayload => 400,
            SmsWebhookDecision::InvalidSignature | SmsWebhookDecision::AccountMismatch => 403,
            SmsWebhookDecision::Replay | SmsWebhookDecision::Accept(_) => 200,
        }
    }
}

/// Shared mutable webhook state (rate limiter + replay cache).
#[derive(Debug, Default)]
pub struct SmsWebhookState {
    pub rate_limiter: FixedWindowRateLimiter,
    pub replay_cache: SmsReplayCache,
}

/// Full inbound webhook pipeline as pure logic. `request_search` is the raw
/// query string (`?a=b`) of the live request; `body` may be `None` when the
/// body read failed (over limit / timeout).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_sms_webhook(
    state: &mut SmsWebhookState,
    account: &ResolvedSmsAccount,
    method: &str,
    remote_addr: &str,
    request_search: &str,
    signature_header: Option<&str>,
    body: Option<&str>,
    now_ms: u64,
) -> SmsWebhookDecision {
    if !method.eq_ignore_ascii_case("POST") {
        return SmsWebhookDecision::MethodNotAllowed;
    }
    let key = if remote_addr.is_empty() { "unknown" } else { remote_addr };
    if state.rate_limiter.is_rate_limited(key, now_ms) {
        return SmsWebhookDecision::RateLimited;
    }
    let Some(body) = body else {
        return SmsWebhookDecision::InvalidBody;
    };
    let form = parse_twilio_form_body(body);
    if !account.dangerously_disable_signature_validation {
        let url = resolve_twilio_webhook_signature_url(&account.public_webhook_url, request_search);
        if !verify_twilio_signature(signature_header, &url, &account.auth_token, &form) {
            return SmsWebhookDecision::InvalidSignature;
        }
    }
    let Some(msg) = build_twilio_inbound_message(&form) else {
        return SmsWebhookDecision::MissingPayload;
    };
    if !msg.account_sid.is_empty() && msg.account_sid != account.account_sid {
        return SmsWebhookDecision::AccountMismatch;
    }
    if !state
        .replay_cache
        .remember(&account.account_id, &msg.message_sid, now_ms)
    {
        return SmsWebhookDecision::Replay;
    }
    SmsWebhookDecision::Accept(msg)
}

// ---------------------------------------------------------------------------
// Outbound Messages API (`src/twilio.ts::sendSmsViaTwilio` + `src/send.ts`)
// ---------------------------------------------------------------------------

const TWILIO_ACCOUNTS_URL: &str = "https://api.twilio.com/2010-04-01/Accounts";

/// A ready-to-send Twilio Messages API request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioSendRequest {
    /// POST target: `.../Accounts/{sid}/Messages.json`.
    pub url: String,
    /// `application/x-www-form-urlencoded` body.
    pub form_body: String,
    /// Value for the `Authorization` header (Basic auth).
    pub authorization: String,
}

/// Build the outbound send request. Requires `fromNumber` or
/// `messagingServiceSid`. Upstream: `sendSmsViaTwilio`.
pub fn build_twilio_send_request(
    account: &ResolvedSmsAccount,
    to: &str,
    text: &str,
) -> Result<TwilioSendRequest> {
    if account.from_number.is_empty() && account.messaging_service_sid.is_empty() {
        anyhow::bail!("Twilio SMS send requires fromNumber or messagingServiceSid.");
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("To", to).append_pair("Body", text);
    if !account.from_number.is_empty() {
        serializer.append_pair("From", &account.from_number);
    } else {
        serializer.append_pair("MessagingServiceSid", &account.messaging_service_sid);
    }
    let credentials = format!("{}:{}", account.account_sid, account.auth_token);
    Ok(TwilioSendRequest {
        url: format!(
            "{TWILIO_ACCOUNTS_URL}/{}/Messages.json",
            url::form_urlencoded::byte_serialize(account.account_sid.as_bytes()).collect::<String>()
        ),
        form_body: serializer.finish(),
        authorization: format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        ),
    })
}

/// Successful send result. Upstream: `src/types.ts::SmsSendResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmsSendResult {
    pub sid: String,
    pub to: String,
    pub from: Option<String>,
    pub status: Option<String>,
}

/// Parse a Messages API response. Ok responses require a `sid`; error
/// responses surface Twilio's `{code, message}` envelope.
/// Upstream: `parseTwilioSuccessPayload` + `TwilioSmsApiError`.
pub fn parse_twilio_send_response(ok: bool, http_status: u16, text: &str) -> Result<SmsSendResult> {
    if !ok {
        let (code, message) = parse_twilio_api_error(text);
        let detail = message.unwrap_or_else(|| {
            if text.is_empty() {
                "unknown".to_string()
            } else {
                text.to_string()
            }
        });
        let code_suffix = code.map(|c| format!(" [code {c}]")).unwrap_or_default();
        anyhow::bail!("Twilio SMS send failed ({http_status}): {detail}{code_suffix}");
    }
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| anyhow::anyhow!("Twilio SMS send returned malformed JSON."))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Twilio SMS send returned malformed JSON."))?;
    let string = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_string);
    Ok(SmsSendResult {
        sid: string("sid").unwrap_or_default(),
        to: string("to").unwrap_or_default(),
        from: string("from"),
        status: string("status"),
    })
}

fn parse_twilio_api_error(text: &str) -> (Option<i64>, Option<String>) {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return (None, None);
    };
    let code = parsed.get("code").and_then(|v| v.as_i64());
    let message = parsed
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (code, message)
}

/// Convert assistant markdown to SMS-friendly plain text: unwrap fenced code
/// blocks, rewrite `[label](url)` as `label (url)`, strip residual markdown,
/// collapse blank runs. Upstream: `src/send.ts::toSmsPlainText`.
pub fn to_sms_plain_text(text: &str) -> String {
    // Unwrap fenced code blocks, keeping their bodies.
    let mut without_fences = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        without_fences.push_str(&rest[..start]);
        let after_start = &rest[start + 3..];
        // Skip the info string up to the first newline.
        let body_start = after_start.find('\n').map(|i| i + 1).unwrap_or(0);
        let body_slice = &after_start[body_start..];
        if let Some(end) = body_slice.find("```") {
            without_fences.push_str(body_slice[..end].trim());
            rest = &body_slice[end + 3..];
        } else {
            without_fences.push_str(after_start);
            rest = "";
        }
    }
    without_fences.push_str(rest);

    // Markdown links → "label (url)" (or bare url when the label matches).
    static LINK_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"\[([^\]]+)\]\((https?://[^)\s]+)\)").unwrap()
    });
    let with_links = LINK_RE.replace_all(&without_fences, |caps: &regex::Captures<'_>| {
        let label = caps[1].trim().to_string();
        let url = caps[2].trim().to_string();
        if !label.is_empty() && label != url {
            format!("{label} ({url})")
        } else {
            url
        }
    });

    static BLANK_RUNS_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\n{3,}").unwrap());
    let stripped = super::normalize::strip_markdown(&with_links).replace("\r\n", "\n");
    BLANK_RUNS_RE.replace_all(&stripped, "\n\n").trim().to_string()
}

/// Split plain text into outbound chunks of at most `limit` characters,
/// preferring newline then whitespace boundaries.
pub fn chunk_sms_text(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut chunks = Vec::new();
    let mut rest: &str = text.trim();
    while !rest.is_empty() {
        let char_count = rest.chars().count();
        if char_count <= limit {
            chunks.push(rest.to_string());
            break;
        }
        let hard_end = rest
            .char_indices()
            .nth(limit)
            .map(|(idx, _)| idx)
            .unwrap_or(rest.len());
        let window = &rest[..hard_end];
        let boundary_is_whitespace = rest[hard_end..]
            .chars()
            .next()
            .map(char::is_whitespace)
            .unwrap_or(false);
        let split_at = if boundary_is_whitespace {
            hard_end
        } else {
            window
                .rfind('\n')
                .or_else(|| window.rfind(char::is_whitespace))
                .filter(|idx| *idx > 0)
                .unwrap_or(hard_end)
        };
        chunks.push(rest[..split_at].trim_end().to_string());
        rest = rest[split_at..].trim_start();
    }
    chunks.retain(|c| !c.is_empty());
    chunks
}

// ---------------------------------------------------------------------------
// GSM-7 / UCS-2 segment calculator
// ---------------------------------------------------------------------------

/// SMS wire encoding for a message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmsEncoding {
    /// GSM 03.38 basic + extension alphabet (7-bit septets).
    Gsm7,
    /// UTF-16 (UCS-2) fallback for any character outside GSM-7.
    Ucs2,
}

/// Segment math for one SMS body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmsSegmentInfo {
    pub encoding: SmsEncoding,
    /// Number of billable message segments.
    pub segments: u32,
    /// Total encoding units (GSM-7 septets or UTF-16 code units).
    pub units: u32,
    /// Units available per segment at this segment count.
    pub units_per_segment: u32,
}

const GSM7_BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑܧ¿abcdefghijklmnopqrstuvwxyzäöñüà";
const GSM7_EXTENSION: &str = "^{}\\[~]|€\u{000C}";

/// Septet cost of one char under GSM-7, or `None` when it forces UCS-2.
fn gsm7_units(ch: char) -> Option<u32> {
    if GSM7_BASIC.contains(ch) {
        Some(1)
    } else if GSM7_EXTENSION.contains(ch) {
        Some(2)
    } else {
        None
    }
}

/// Compute encoding + segment count for a message body.
///
/// GSM-7: 160 septets single-segment, 153 per segment when concatenated.
/// UCS-2: 70 UTF-16 code units single-segment, 67 when concatenated.
pub fn sms_segment_info(text: &str) -> SmsSegmentInfo {
    let mut gsm_units: u32 = 0;
    let mut is_gsm = true;
    for ch in text.chars() {
        match gsm7_units(ch) {
            Some(u) if is_gsm => gsm_units += u,
            _ => {
                is_gsm = false;
                break;
            }
        }
    }
    if is_gsm {
        let (single, multi) = (160u32, 153u32);
        let (segments, per) = if gsm_units == 0 {
            (1, single)
        } else if gsm_units <= single {
            (1, single)
        } else {
            (gsm_units.div_ceil(multi), multi)
        };
        return SmsSegmentInfo {
            encoding: SmsEncoding::Gsm7,
            segments,
            units: gsm_units,
            units_per_segment: per,
        };
    }
    let utf16_units: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
    let (single, multi) = (70u32, 67u32);
    let (segments, per) = if utf16_units <= single {
        (1, single)
    } else {
        (utf16_units.div_ceil(multi), multi)
    };
    SmsSegmentInfo {
        encoding: SmsEncoding::Ucs2,
        segments,
        units: utf16_units,
        units_per_segment: per,
    }
}

// ---------------------------------------------------------------------------
// Webhook diagnostics probe / proof follow-ups (`src/status.ts`)
// ---------------------------------------------------------------------------

/// Twilio error code proving webhook reachability failures in message logs.
pub const TWILIO_ERROR_WEBHOOK_REACHABILITY: &str = "11200";

/// One Twilio incoming phone number record (subset used for diagnostics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TwilioIncomingPhoneNumber {
    pub sid: String,
    pub phone_number: String,
    pub sms_url: String,
    pub sms_method: String,
    pub voice_url: String,
}

/// One Twilio message-log entry (subset used for diagnostics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TwilioMessageLogEntry {
    pub sid: String,
    pub direction: String,
    pub status: String,
    pub error_code: String,
    pub date_created: String,
    pub date_sent: String,
}

/// Result of comparing the Twilio-side webhook wiring against the expected
/// public webhook URL. Upstream: `SmsTwilioWebhookProbe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsWebhookProbe {
    Skipped { reason: String },
    NumberNotFound { expected_number: String },
    Missing { phone_number: String, expected_url: String },
    MethodMismatch {
        phone_number: String,
        expected_url: String,
        configured_url: String,
        configured_method: String,
    },
    UrlMismatch {
        phone_number: String,
        expected_url: String,
        configured_url: String,
        configured_method: String,
    },
    Matches {
        phone_number: String,
        expected_url: String,
        configured_method: String,
    },
}

fn normalized_url_eq(a: &str, b: &str) -> bool {
    let norm = |u: &str| u.trim().trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

/// Compare the account's expected webhook wiring against the incoming phone
/// number record fetched from Twilio ("proof" that inbound SMS will reach
/// us). Upstream: `src/status.ts::compareTwilioWebhook`.
pub fn compare_twilio_webhook(
    account: &ResolvedSmsAccount,
    number: Option<&TwilioIncomingPhoneNumber>,
) -> SmsWebhookProbe {
    if account.public_webhook_url.trim().is_empty() {
        return SmsWebhookProbe::Skipped {
            reason: "publicWebhookUrl is not configured".to_string(),
        };
    }
    let expected_url = account.public_webhook_url.trim().to_string();
    let Some(number) = number else {
        return SmsWebhookProbe::NumberNotFound {
            expected_number: account.from_number.clone(),
        };
    };
    if number.sms_url.trim().is_empty() {
        return SmsWebhookProbe::Missing {
            phone_number: number.phone_number.clone(),
            expected_url,
        };
    }
    if !number.sms_method.trim().is_empty() && !number.sms_method.eq_ignore_ascii_case("POST") {
        return SmsWebhookProbe::MethodMismatch {
            phone_number: number.phone_number.clone(),
            expected_url,
            configured_url: number.sms_url.clone(),
            configured_method: number.sms_method.clone(),
        };
    }
    if !normalized_url_eq(&number.sms_url, &expected_url) {
        return SmsWebhookProbe::UrlMismatch {
            phone_number: number.phone_number.clone(),
            expected_url,
            configured_url: number.sms_url.clone(),
            configured_method: number.sms_method.clone(),
        };
    }
    SmsWebhookProbe::Matches {
        phone_number: number.phone_number.clone(),
        expected_url,
        configured_method: number.sms_method.clone(),
    }
}

/// Proof follow-up: classify a recent inbound message-log entry into a
/// human-readable hint (error 11200 = Twilio could not reach the webhook).
/// Upstream: `src/status.ts` recent-inbound handling.
pub fn recent_inbound_hint(entry: &TwilioMessageLogEntry) -> Option<String> {
    if entry.error_code.trim() == TWILIO_ERROR_WEBHOOK_REACHABILITY {
        return Some(format!(
            "Twilio logged webhook reachability error 11200 for inbound message {} — the public webhook URL is not reachable from Twilio.",
            entry.sid
        ));
    }
    if entry.status.eq_ignore_ascii_case("received") && entry.error_code.trim().is_empty() {
        return Some(format!(
            "Latest inbound message {} was received cleanly (created {}).",
            entry.sid, entry.date_created
        ));
    }
    None
}

/// Tailscale Funnel setups need the exact SMS path exposed.
/// Upstream: `src/status.ts::addTailscaleHint`.
pub fn tailscale_hint(account: &ResolvedSmsAccount) -> Option<String> {
    let host = url::Url::parse(&account.public_webhook_url)
        .ok()?
        .host_str()?
        .to_string();
    if !host.ends_with(".ts.net") {
        return None;
    }
    Some(format!(
        "Tailscale Funnel must expose the exact SMS path: tailscale funnel --bg --set-path {path} http://127.0.0.1:<gateway-port>{path}",
        path = account.webhook_path
    ))
}

/// Startup security warnings. Upstream: `src/channel.ts::collectSmsSecurityWarnings`.
pub fn collect_sms_security_warnings(account: &ResolvedSmsAccount) -> Vec<String> {
    let mut warnings = Vec::new();
    if account.dangerously_disable_signature_validation {
        warnings.push(
            "- SMS: Twilio signature validation is disabled. Only use this for local testing."
                .to_string(),
        );
    }
    if account.dm_policy == "open" && account.allow_from.iter().any(|e| e == "*") {
        warnings.push(
            "- SMS: dmPolicy=\"open\" allows any phone number to message the bot.".to_string(),
        );
    }
    warnings
}

// ---------------------------------------------------------------------------
// Channel plugin
// ---------------------------------------------------------------------------

pub struct SmsChannel {
    enabled: bool,
    account: ResolvedSmsAccount,
    #[allow(dead_code)]
    webhook_state: Mutex<SmsWebhookState>,
}

impl SmsChannel {
    pub fn new(config: &Config) -> Self {
        let raw = config.channels.extensions.get("sms");
        let enabled = raw
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ext: SmsExtensionConfig = raw
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let account = resolve_sms_account(&ext, None);
        Self {
            enabled,
            account,
            webhook_state: Mutex::new(SmsWebhookState::default()),
        }
    }

    /// Evaluate an inbound webhook request against this channel's account.
    #[allow(dead_code)]
    pub fn handle_webhook(
        &self,
        method: &str,
        remote_addr: &str,
        request_search: &str,
        signature_header: Option<&str>,
        body: Option<&str>,
    ) -> SmsWebhookDecision {
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let mut state = self.webhook_state.lock();
        evaluate_sms_webhook(
            &mut state,
            &self.account,
            method,
            remote_addr,
            request_search,
            signature_header,
            body,
            now_ms,
        )
    }
}

#[async_trait]
impl ChannelPlugin for SmsChannel {
    fn id(&self) -> &str {
        "sms"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "SMS".to_string(),
            description: "Twilio SMS channel".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![ChannelCapability::SendText, ChannelCapability::ReceiveText]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if self.enabled {
            for warning in collect_sms_security_warnings(&self.account) {
                tracing::warn!("{warning}");
            }
            info!(
                account = %self.account.account_id,
                webhook_path = %self.account.webhook_path,
                "SMS channel started"
            );
        }
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.enabled {
            anyhow::bail!("SMS channel is not enabled");
        }
        let to = normalize_sms_phone_number(to);
        if !looks_like_sms_phone_number(&to) {
            anyhow::bail!("SMS send target is not a valid E.164 phone number: {to}");
        }
        let text = to_sms_plain_text(message);
        if text.is_empty() {
            anyhow::bail!("SMS send requires non-empty text.");
        }
        let client = reqwest::Client::new();
        for chunk in chunk_sms_text(&text, self.account.text_chunk_limit) {
            let request = build_twilio_send_request(&self.account, &to, &chunk)?;
            let response = client
                .post(&request.url)
                .header("authorization", &request.authorization)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(request.form_body)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await?;
            let ok = response.status().is_success();
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            let result = parse_twilio_send_response(ok, status, &body)?;
            info!(sid = %result.sid, to = %result.to, "SMS sent");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_account() -> ResolvedSmsAccount {
        ResolvedSmsAccount {
            account_id: "default".to_string(),
            enabled: true,
            account_sid: "AC00000000000000000000000000000000".to_string(),
            auth_token: "12345".to_string(),
            from_number: "+15550001111".to_string(),
            messaging_service_sid: String::new(),
            default_to: String::new(),
            webhook_path: "/webhooks/sms".to_string(),
            public_webhook_url: "https://example.ts.net/webhooks/sms".to_string(),
            dangerously_disable_signature_validation: false,
            dm_policy: "pairing".to_string(),
            allow_from: vec![],
            text_chunk_limit: 1500,
        }
    }

    // --- phone ---

    #[test]
    fn normalizes_phone_numbers() {
        assert_eq!(normalize_sms_phone_number("sms:+1 (555) 000-1234"), "+15550001234");
        assert_eq!(normalize_sms_phone_number("twilio-sms:15550001234"), "+15550001234");
        assert_eq!(normalize_sms_phone_number("15550001234"), "+15550001234");
        assert_eq!(normalize_sms_phone_number(""), "");
    }

    #[test]
    fn phone_shape_check() {
        assert!(looks_like_sms_phone_number("+15550001234"));
        assert!(looks_like_sms_phone_number("15550001234"));
        assert!(!looks_like_sms_phone_number("+0123"));
        assert!(!looks_like_sms_phone_number("hello"));
    }

    #[test]
    fn allow_from_preserves_wildcard() {
        assert_eq!(normalize_sms_allow_from(" * "), "*");
        assert_eq!(normalize_sms_allow_from("(555) 000-1234"), "+5550001234");
    }

    // --- signature ---

    #[test]
    fn sha1_known_vectors() {
        // RFC 3174: SHA1("abc")
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn hmac_sha1_rfc2202_vector() {
        // RFC 2202 test case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn twilio_signature_documented_example() {
        // Twilio's documented signature example:
        // https://www.twilio.com/docs/usage/security#test-the-validity-of-your-webhook-signature
        let mut form = HashMap::new();
        for (k, v) in [
            ("CallSid", "CA1234567890ABCDE"),
            ("Caller", "+12349013030"),
            ("Digits", "1234"),
            ("From", "+12349013030"),
            ("To", "+18005551212"),
        ] {
            form.insert(k.to_string(), v.to_string());
        }
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let signature = compute_twilio_signature(url, "12345", &form);
        // Independently verified with Python hmac/hashlib over the same input.
        assert_eq!(signature, "0/KCTR6DLpKmkAf8muzZqo1nDgQ=");
        assert!(verify_twilio_signature(Some(&signature), url, "12345", &form));
        assert!(!verify_twilio_signature(Some("bogus/sig="), url, "12345", &form));
        assert!(!verify_twilio_signature(None, url, "12345", &form));
    }

    #[test]
    fn signature_url_appends_request_query() {
        assert_eq!(
            resolve_twilio_webhook_signature_url("https://x.example/sms", "?a=1"),
            "https://x.example/sms?a=1"
        );
        // Configured URL already has a query — keep it verbatim.
        assert_eq!(
            resolve_twilio_webhook_signature_url("https://x.example/sms?k=v", "?a=1"),
            "https://x.example/sms?k=v"
        );
        assert_eq!(
            resolve_twilio_webhook_signature_url("https://x.example/sms#frag", "?a=1"),
            "https://x.example/sms?a=1#frag"
        );
        assert_eq!(
            resolve_twilio_webhook_signature_url("https://x.example/sms", ""),
            "https://x.example/sms"
        );
    }

    // --- form + inbound mapping ---

    #[test]
    fn parses_form_urlencoded() {
        let form = parse_twilio_form_body("From=%2B15550001234&Body=hello+world&To=%2B15550009999");
        assert_eq!(form.get("From").unwrap(), "+15550001234");
        assert_eq!(form.get("Body").unwrap(), "hello world");
    }

    #[test]
    fn builds_inbound_message_with_sid_fallbacks() {
        let mut form = HashMap::new();
        form.insert("From".to_string(), "+15550001234".to_string());
        form.insert("To".to_string(), "+15550009999".to_string());
        form.insert("Body".to_string(), "hi".to_string());
        form.insert("SmsSid".to_string(), "SM123".to_string());
        let msg = build_twilio_inbound_message(&form).unwrap();
        assert_eq!(msg.message_sid, "SM123");

        form.remove("SmsSid");
        assert!(build_twilio_inbound_message(&form).is_none());
    }

    #[test]
    fn maps_inbound_to_normalized_message() {
        let msg = SmsInboundMessage {
            message_sid: "SM1".to_string(),
            account_sid: "AC1".to_string(),
            from: "+1 555 000 1234".to_string(),
            to: "+15550009999".to_string(),
            body: "hello".to_string(),
        };
        let normalized = sms_inbound_to_normalized(&msg, "default");
        assert_eq!(normalized.channel, "sms");
        assert_eq!(normalized.chat_id, "+15550001234");
        assert_eq!(normalized.chat_type, ChatType::Dm);
        assert_eq!(normalized.text, "hello");
        assert_eq!(normalized.id, "SM1");
    }

    // --- webhook pipeline ---

    fn signed_body_and_header(account: &ResolvedSmsAccount) -> (String, String) {
        let body =
            "From=%2B15550001234&To=%2B15550001111&Body=hi&MessageSid=SM77&AccountSid=AC00000000000000000000000000000000";
        let form = parse_twilio_form_body(body);
        let signature =
            compute_twilio_signature(&account.public_webhook_url, &account.auth_token, &form);
        (body.to_string(), signature)
    }

    #[test]
    fn webhook_accepts_signed_post_then_drops_replay() {
        let account = test_account();
        let mut state = SmsWebhookState::default();
        let (body, signature) = signed_body_and_header(&account);
        let decision = evaluate_sms_webhook(
            &mut state, &account, "POST", "1.2.3.4", "", Some(&signature), Some(&body), 1_000,
        );
        match &decision {
            SmsWebhookDecision::Accept(msg) => assert_eq!(msg.message_sid, "SM77"),
            other => panic!("expected Accept, got {other:?}"),
        }
        assert_eq!(decision.status_code(), 200);

        // Same MessageSid inside the TTL window → replay drop with 200.
        let replay = evaluate_sms_webhook(
            &mut state, &account, "POST", "1.2.3.4", "", Some(&signature), Some(&body), 2_000,
        );
        assert_eq!(replay, SmsWebhookDecision::Replay);
        assert_eq!(replay.status_code(), 200);
    }

    #[test]
    fn webhook_rejects_bad_method_signature_and_account() {
        let account = test_account();
        let mut state = SmsWebhookState::default();
        assert_eq!(
            evaluate_sms_webhook(&mut state, &account, "GET", "ip", "", None, Some(""), 0),
            SmsWebhookDecision::MethodNotAllowed
        );
        let (body, _) = signed_body_and_header(&account);
        assert_eq!(
            evaluate_sms_webhook(
                &mut state, &account, "POST", "ip", "", Some("nope="), Some(&body), 0
            ),
            SmsWebhookDecision::InvalidSignature
        );
        // Mismatched AccountSid (signature disabled to reach that check).
        let mut open = account.clone();
        open.dangerously_disable_signature_validation = true;
        let bad_body = body.replace("AC00000000000000000000000000000000", "ACother");
        assert_eq!(
            evaluate_sms_webhook(&mut state, &open, "POST", "ip", "", None, Some(&bad_body), 0),
            SmsWebhookDecision::AccountMismatch
        );
        // Missing payload.
        assert_eq!(
            evaluate_sms_webhook(&mut state, &open, "POST", "ip", "", None, Some("Body=x"), 0),
            SmsWebhookDecision::MissingPayload
        );
        // Unreadable body.
        assert_eq!(
            evaluate_sms_webhook(&mut state, &open, "POST", "ip2", "", None, None, 0),
            SmsWebhookDecision::InvalidBody
        );
    }

    #[test]
    fn webhook_rate_limits_per_remote_addr() {
        let mut account = test_account();
        account.dangerously_disable_signature_validation = true;
        let mut state = SmsWebhookState::default();
        let mut last = SmsWebhookDecision::MethodNotAllowed;
        for i in 0..31 {
            let body = format!(
                "From=%2B15550001234&To=%2B15550001111&Body=hi&MessageSid=SM{i}"
            );
            last = evaluate_sms_webhook(
                &mut state, &account, "POST", "9.9.9.9", "", None, Some(&body), 5_000,
            );
        }
        assert_eq!(last, SmsWebhookDecision::RateLimited);
        assert_eq!(last.status_code(), 429);
    }

    #[test]
    fn replay_cache_expires_after_ttl() {
        let mut cache = SmsReplayCache::new();
        assert!(cache.remember("default", "SM1", 0));
        assert!(!cache.remember("default", "SM1", 1_000));
        // Past the 10-minute TTL the SID is accepted again.
        assert!(cache.remember("default", "SM1", REPLAY_CACHE_TTL_MS + 1));
        // Distinct accounts do not collide.
        assert!(cache.remember("other", "SM1", REPLAY_CACHE_TTL_MS + 2));
    }

    // --- outbound ---

    #[test]
    fn builds_send_request_with_from_number() {
        let account = test_account();
        let request = build_twilio_send_request(&account, "+15550002222", "hello").unwrap();
        assert!(request.url.ends_with("/Accounts/AC00000000000000000000000000000000/Messages.json"));
        assert!(request.form_body.contains("To=%2B15550002222"));
        assert!(request.form_body.contains("From=%2B15550001111"));
        assert!(!request.form_body.contains("MessagingServiceSid"));
        assert!(request.authorization.starts_with("Basic "));
    }

    #[test]
    fn builds_send_request_with_messaging_service() {
        let mut account = test_account();
        account.from_number = String::new();
        account.messaging_service_sid = "MG123".to_string();
        let request = build_twilio_send_request(&account, "+15550002222", "hello").unwrap();
        assert!(request.form_body.contains("MessagingServiceSid=MG123"));

        account.messaging_service_sid = String::new();
        assert!(build_twilio_send_request(&account, "+15550002222", "x").is_err());
    }

    #[test]
    fn parses_send_response_and_error_envelope() {
        let result = parse_twilio_send_response(
            true,
            201,
            r#"{"sid":"SM9","to":"+15550002222","from":"+15550001111","status":"queued"}"#,
        )
        .unwrap();
        assert_eq!(result.sid, "SM9");
        assert_eq!(result.status.as_deref(), Some("queued"));

        let err = parse_twilio_send_response(
            false,
            400,
            r#"{"code":21211,"message":"Invalid 'To' number"}"#,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("400"));
        assert!(text.contains("Invalid 'To' number"));
        assert!(text.contains("21211"));

        assert!(parse_twilio_send_response(true, 200, "not json").is_err());
    }

    #[test]
    fn plain_text_conversion() {
        let converted = to_sms_plain_text(
            "See [the docs](https://example.com/d) and run:\n```sh\nls -la\n```\n\n\n\n**bold** end",
        );
        assert!(converted.contains("the docs (https://example.com/d)"));
        assert!(converted.contains("ls -la"));
        assert!(!converted.contains("```"));
        assert!(!converted.contains("**"));
        assert!(!converted.contains("\n\n\n"));
    }

    #[test]
    fn chunks_text_on_boundaries() {
        let chunks = chunk_sms_text("aaaa bbbb cccc", 9);
        assert_eq!(chunks, vec!["aaaa bbbb".to_string(), "cccc".to_string()]);
        assert_eq!(chunk_sms_text("short", 100), vec!["short".to_string()]);
    }

    // --- segments ---

    #[test]
    fn gsm7_single_segment_boundaries() {
        let body_160: String = "a".repeat(160);
        let info = sms_segment_info(&body_160);
        assert_eq!(info.encoding, SmsEncoding::Gsm7);
        assert_eq!(info.segments, 1);
        assert_eq!(info.units, 160);

        let body_161: String = "a".repeat(161);
        let info = sms_segment_info(&body_161);
        assert_eq!(info.segments, 2);
        assert_eq!(info.units_per_segment, 153);
    }

    #[test]
    fn gsm7_extension_chars_cost_two_septets() {
        // '€' is in the GSM-7 extension table: 2 septets each.
        let info = sms_segment_info(&"€".repeat(80));
        assert_eq!(info.encoding, SmsEncoding::Gsm7);
        assert_eq!(info.units, 160);
        assert_eq!(info.segments, 1);
        let info = sms_segment_info(&"€".repeat(81));
        assert_eq!(info.segments, 2);
    }

    #[test]
    fn ucs2_fallback_and_boundaries() {
        let info = sms_segment_info(&"日".repeat(70));
        assert_eq!(info.encoding, SmsEncoding::Ucs2);
        assert_eq!(info.segments, 1);
        let info = sms_segment_info(&"日".repeat(71));
        assert_eq!(info.segments, 2);
        assert_eq!(info.units_per_segment, 67);
        // Astral chars count as two UTF-16 units.
        let info = sms_segment_info(&"😀".repeat(36));
        assert_eq!(info.encoding, SmsEncoding::Ucs2);
        assert_eq!(info.units, 72);
        assert_eq!(info.segments, 2);
    }

    #[test]
    fn empty_body_is_one_gsm_segment() {
        let info = sms_segment_info("");
        assert_eq!(info.encoding, SmsEncoding::Gsm7);
        assert_eq!(info.segments, 1);
        assert_eq!(info.units, 0);
    }

    // --- diagnostics probe ---

    #[test]
    fn webhook_probe_classifications() {
        let account = test_account();
        let mut number = TwilioIncomingPhoneNumber {
            sid: "PN1".to_string(),
            phone_number: "+15550001111".to_string(),
            sms_url: "https://example.ts.net/webhooks/sms".to_string(),
            sms_method: "POST".to_string(),
            voice_url: String::new(),
        };
        assert!(matches!(
            compare_twilio_webhook(&account, Some(&number)),
            SmsWebhookProbe::Matches { .. }
        ));

        number.sms_url = "https://old.example/hook".to_string();
        assert!(matches!(
            compare_twilio_webhook(&account, Some(&number)),
            SmsWebhookProbe::UrlMismatch { .. }
        ));

        number.sms_url = account.public_webhook_url.clone();
        number.sms_method = "GET".to_string();
        assert!(matches!(
            compare_twilio_webhook(&account, Some(&number)),
            SmsWebhookProbe::MethodMismatch { .. }
        ));

        number.sms_url = String::new();
        number.sms_method = "POST".to_string();
        assert!(matches!(
            compare_twilio_webhook(&account, Some(&number)),
            SmsWebhookProbe::Missing { .. }
        ));

        assert!(matches!(
            compare_twilio_webhook(&account, None),
            SmsWebhookProbe::NumberNotFound { .. }
        ));

        let mut unconfigured = account.clone();
        unconfigured.public_webhook_url = String::new();
        assert!(matches!(
            compare_twilio_webhook(&unconfigured, Some(&number)),
            SmsWebhookProbe::Skipped { .. }
        ));
    }

    #[test]
    fn recent_inbound_hints() {
        let entry = TwilioMessageLogEntry {
            sid: "SM5".to_string(),
            direction: "inbound".to_string(),
            status: "received".to_string(),
            error_code: TWILIO_ERROR_WEBHOOK_REACHABILITY.to_string(),
            date_created: "2026-07-23".to_string(),
            date_sent: String::new(),
        };
        assert!(recent_inbound_hint(&entry).unwrap().contains("11200"));

        let clean = TwilioMessageLogEntry {
            error_code: String::new(),
            ..entry
        };
        assert!(recent_inbound_hint(&clean).unwrap().contains("received cleanly"));
    }

    #[test]
    fn tailscale_hint_only_for_ts_net_hosts() {
        let account = test_account();
        assert!(tailscale_hint(&account).unwrap().contains("--set-path /webhooks/sms"));
        let mut other = account;
        other.public_webhook_url = "https://example.com/webhooks/sms".to_string();
        assert!(tailscale_hint(&other).is_none());
    }

    #[test]
    fn security_warnings() {
        let mut account = test_account();
        assert!(collect_sms_security_warnings(&account).is_empty());
        account.dangerously_disable_signature_validation = true;
        account.dm_policy = "open".to_string();
        account.allow_from = vec!["*".to_string()];
        assert_eq!(collect_sms_security_warnings(&account).len(), 2);
    }

    // --- config resolution ---

    #[test]
    fn resolves_account_with_overrides() {
        let raw = serde_json::json!({
            "enabled": true,
            "accountSid": "ACbase",
            "authToken": "tokbase",
            "fromNumber": "+15550001111",
            "textChunkLimit": "600",
            "allowFrom": ["+1 555 000 9999", "*"],
            "accounts": {
                "second": { "accountSid": "ACsecond", "fromNumber": "+15550002222" }
            }
        });
        let ext: SmsExtensionConfig = serde_json::from_value(raw).unwrap();
        let default = resolve_sms_account(&ext, None);
        assert_eq!(default.account_id, "default");
        assert_eq!(default.account_sid, "ACbase");
        assert_eq!(default.webhook_path, DEFAULT_WEBHOOK_PATH);
        assert_eq!(default.text_chunk_limit, 600);
        assert_eq!(default.allow_from, vec!["+15550009999".to_string(), "*".to_string()]);

        let second = resolve_sms_account(&ext, Some("second"));
        assert_eq!(second.account_sid, "ACsecond");
        assert_eq!(second.from_number, "+15550002222");
        // Account entries inherit unset fields from the channel base.
        assert_eq!(second.auth_token, "tokbase");
    }
}
