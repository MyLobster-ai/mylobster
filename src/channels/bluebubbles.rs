//! BlueBubbles iMessage bridge channel (legacy backend).
//!
//! Port of the OpenClaw BlueBubbles extension behavior at v2026.5.2. Upstream
//! removed this channel in v2026.7.1 in favor of `channels.imessage` with the
//! `imsg` backend (see `imessage.rs`, including the config-migration helper
//! `migrate_bluebubbles_extension_config`); mylobster keeps it selectable via
//! `channels.imessage.provider = "bluebubbles"` or a legacy
//! `channels.extensions["bluebubbles"]` blob.
//!
//! v5.2 parity implemented here:
//! - opt-in `replyContextApiFallback` (fetch reply context via REST when the
//!   webhook lacks it) with an SSRF guard (configured host only, redirects
//!   blocked), dedupe coalescing of concurrent fetches, and log redaction of
//!   `?password=` / `?token=` query params and `Authorization:` headers;
//! - Apple UTI audio attachment classification (`public.audio`,
//!   `com.apple.coreaudio-format`, ...).

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::{info, warn};
use url::Url;

// ============================================================================
// Extension config (from `config.channels.extensions["bluebubbles"]`)
// ============================================================================

/// Local view of the legacy BlueBubbles extension config blob.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueBubblesExtensionConfig {
    pub enabled: Option<bool>,
    /// BlueBubbles server API URL (e.g. `http://192.168.1.100:1234`).
    #[serde(alias = "serverUrl", alias = "url")]
    pub api_url: Option<String>,
    /// BlueBubbles server password.
    #[serde(alias = "apiPassword")]
    pub password: Option<String>,
    /// Opt-in: fetch reply context via the BlueBubbles REST API when the
    /// webhook payload lacks it (v5.2 `replyContextApiFallback`).
    pub reply_context_api_fallback: Option<bool>,
    /// Max inbound attachment size in MB.
    pub media_max_mb: Option<f64>,
}

impl BlueBubblesExtensionConfig {
    /// Parse the `channels.extensions["bluebubbles"]` JSON blob.
    pub fn from_extension_value(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

// ============================================================================
// Log redaction (v5.2: redact `?password=` / `?token=` query params and
// `Authorization:` headers from BlueBubbles logs)
// ============================================================================

static QUERY_SECRET_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)([?&](?:password|token)=)[^&\s\x22']*").unwrap());
static AUTH_HEADER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(authorization\s*:\s*)[^\r\n]*").unwrap());

/// Redact BlueBubbles secrets from a log line: `?password=`/`?token=`
/// (and `&`-separated) query values and `Authorization:` header values.
pub fn redact_bluebubbles_log(text: &str) -> String {
    let redacted = QUERY_SECRET_RE.replace_all(text, "${1}[REDACTED]");
    AUTH_HEADER_RE.replace_all(&redacted, "${1}[REDACTED]").to_string()
}

// ============================================================================
// Reply-context API fallback: SSRF guard + dedupe coalescing
// ============================================================================

/// SSRF guard for the reply-context fallback fetch: the candidate URL must
/// target exactly the configured BlueBubbles host (scheme + host + port).
/// Anything else — attacker-controlled hosts smuggled through webhook
/// payloads, scheme downgrades, userinfo tricks — is rejected. Redirects are
/// additionally blocked at the client (see [`build_reply_context_client`]) so
/// the configured host cannot bounce the request elsewhere.
pub fn validate_reply_context_url(configured_api_url: &str, candidate: &str) -> Result<Url> {
    let configured = Url::parse(configured_api_url.trim())
        .map_err(|e| anyhow::anyhow!("BlueBubbles api_url invalid: {e}"))?;
    let candidate = Url::parse(candidate.trim())
        .map_err(|e| anyhow::anyhow!("BlueBubbles reply-context URL invalid: {e}"))?;
    if !matches!(candidate.scheme(), "http" | "https") {
        anyhow::bail!("BlueBubbles reply-context URL has unsupported scheme");
    }
    if !candidate.username().is_empty() || candidate.password().is_some() {
        anyhow::bail!("BlueBubbles reply-context URL must not include credentials");
    }
    let same_host = configured.scheme() == candidate.scheme()
        && configured.host_str().map(str::to_lowercase)
            == candidate.host_str().map(str::to_lowercase)
        && configured.port_or_known_default() == candidate.port_or_known_default();
    if !same_host {
        anyhow::bail!(
            "BlueBubbles reply-context URL rejected: not the configured server host"
        );
    }
    Ok(candidate)
}

/// Build the URL for a reply-context lookup against the configured server
/// only (never a payload-provided URL).
pub fn build_reply_context_message_url(
    configured_api_url: &str,
    message_guid: &str,
    password: &str,
) -> Result<Url> {
    let mut url = Url::parse(configured_api_url.trim())
        .map_err(|e| anyhow::anyhow!("BlueBubbles api_url invalid: {e}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("BlueBubbles api_url cannot be a base"))?;
        segments.pop_if_empty();
        segments.extend(["api", "v1", "message", message_guid]);
    }
    url.query_pairs_mut()
        .append_pair("password", password)
        .append_pair("with", "chats");
    Ok(url)
}

/// HTTP client for the reply-context fallback: redirects are blocked so the
/// SSRF host check cannot be bypassed by a 3xx off the configured server.
pub fn build_reply_context_client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Dedupe key for coalescing concurrent reply-context fetches: one in-flight
/// fetch per (account, message guid).
pub fn reply_context_dedupe_key(account_id: &str, message_guid: &str) -> String {
    format!("{account_id}:{}", message_guid.trim())
}

/// Coalesces concurrent reply-context fetches for the same message id: the
/// first caller wins the fetch, later callers observe `begin() == false` and
/// wait for / reuse the first result instead of issuing a duplicate request.
#[derive(Default)]
pub struct ReplyContextCoalescer {
    in_flight: Mutex<HashSet<String>>,
}

impl ReplyContextCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to claim the fetch for `key`. `false` means another fetch for the
    /// same key is already in flight — coalesce onto it.
    pub fn begin(&self, key: &str) -> bool {
        self.in_flight.lock().insert(key.to_string())
    }

    /// Mark the fetch finished (success or failure) so later requests for the
    /// same message can fetch fresh context again.
    pub fn finish(&self, key: &str) {
        self.in_flight.lock().remove(key);
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.lock().len()
    }
}

/// Whether the reply-context API fallback is active (opt-in; requires the
/// server connection details).
pub fn reply_context_fallback_enabled(config: &BlueBubblesExtensionConfig) -> bool {
    config.reply_context_api_fallback.unwrap_or(false)
        && config.api_url.as_deref().map(str::trim).is_some_and(|u| !u.is_empty())
        && config.password.as_deref().is_some_and(|p| !p.is_empty())
}

// ============================================================================
// Apple UTI audio attachment classification (v5.2)
// ============================================================================

/// Coarse attachment kind for routing (audio transcription vs image vs file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueBubblesAttachmentKind {
    Audio,
    Image,
    Video,
    File,
}

/// Apple UTIs that identify audio attachments. BlueBubbles surfaces the UTI
/// in the attachment metadata; MIME is often missing for voice memos, so the
/// UTI must classify as audio on its own (v5.2 parity row).
const APPLE_AUDIO_UTIS: &[&str] = &[
    "public.audio",
    "com.apple.coreaudio-format",
    "com.apple.m4a-audio",
    "public.mp3",
    "public.mpeg-4-audio",
    "public.aiff-audio",
    "public.aifc-audio",
    "com.microsoft.waveform-audio",
    "org.xiph.opus-audio",
    "public.au-audio",
];

/// Classify an Apple UTI string. Anything under the `public.audio` hierarchy
/// or the known concrete audio UTIs is audio.
pub fn classify_apple_uti(uti: &str) -> Option<BlueBubblesAttachmentKind> {
    let normalized = uti.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    if APPLE_AUDIO_UTIS.contains(&normalized.as_str()) || normalized.ends_with("-audio") {
        return Some(BlueBubblesAttachmentKind::Audio);
    }
    match normalized.as_str() {
        "public.image" | "public.jpeg" | "public.png" | "public.heic" | "public.heif"
        | "com.compuserve.gif" | "public.tiff" => Some(BlueBubblesAttachmentKind::Image),
        "public.movie" | "public.video" | "public.mpeg-4" | "com.apple.quicktime-movie" => {
            Some(BlueBubblesAttachmentKind::Video)
        }
        _ => None,
    }
}

/// Full classifier: UTI first (authoritative for Apple voice memos), then
/// MIME, then filename extension; unknown -> `File`.
pub fn classify_bluebubbles_attachment(
    uti: Option<&str>,
    mime_type: Option<&str>,
    filename: Option<&str>,
) -> BlueBubblesAttachmentKind {
    if let Some(kind) = uti.and_then(classify_apple_uti) {
        return kind;
    }
    if let Some(mime) = mime_type.map(str::trim).map(str::to_lowercase) {
        if mime.starts_with("audio/") {
            return BlueBubblesAttachmentKind::Audio;
        }
        if mime.starts_with("image/") {
            return BlueBubblesAttachmentKind::Image;
        }
        if mime.starts_with("video/") {
            return BlueBubblesAttachmentKind::Video;
        }
    }
    if let Some(ext) = filename
        .and_then(|f| f.rsplit('.').next())
        .map(str::to_lowercase)
    {
        match ext.as_str() {
            "caf" | "m4a" | "mp3" | "wav" | "aiff" | "aif" | "opus" | "ogg" | "amr" => {
                return BlueBubblesAttachmentKind::Audio;
            }
            "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "tiff" | "webp" => {
                return BlueBubblesAttachmentKind::Image;
            }
            "mov" | "mp4" | "m4v" => return BlueBubblesAttachmentKind::Video,
            _ => {}
        }
    }
    BlueBubblesAttachmentKind::File
}

// ============================================================================
// BlueBubbles Channel Implementation
// ============================================================================

/// BlueBubbles iMessage bridge channel.
///
/// Connects to a BlueBubbles server (running on a Mac with iMessage) to
/// send and receive iMessage/SMS messages through its REST API.
///
/// BlueBubbles API docs: <https://documenter.getpostman.com/view/765844/UV5RnfwM>
///
/// The server typically runs on `http://<mac-ip>:1234` and requires a
/// password for authentication.
pub struct BlueBubblesChannel {
    /// BlueBubbles server API URL (e.g. `http://192.168.1.100:1234`).
    api_url: Option<String>,
    /// BlueBubbles server password.
    password: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Opt-in reply-context REST fallback.
    reply_context_api_fallback: bool,
    /// Coalesces concurrent reply-context fetches per message guid.
    reply_context_coalescer: ReplyContextCoalescer,
    /// HTTP client for API calls.
    client: Client,
}

impl Default for BlueBubblesChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl BlueBubblesChannel {
    pub fn new() -> Self {
        Self {
            api_url: None,
            password: None,
            enabled: None,
            reply_context_api_fallback: false,
            reply_context_coalescer: ReplyContextCoalescer::new(),
            client: Client::new(),
        }
    }

    /// Create a configured BlueBubbles channel.
    pub fn with_config(api_url: String, password: String) -> Self {
        Self {
            api_url: Some(api_url),
            password: Some(password),
            enabled: Some(true),
            reply_context_api_fallback: false,
            reply_context_coalescer: ReplyContextCoalescer::new(),
            client: Client::new(),
        }
    }

    /// Create a channel from the legacy `channels.extensions["bluebubbles"]`
    /// config blob.
    pub fn from_extension_config(config: &BlueBubblesExtensionConfig) -> Self {
        Self {
            api_url: config.api_url.clone(),
            password: config.password.clone(),
            enabled: config.enabled,
            reply_context_api_fallback: reply_context_fallback_enabled(config),
            reply_context_coalescer: ReplyContextCoalescer::new(),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Fetch reply context for a webhook payload that lacked it. Guarded:
    /// only when opted in, only against the configured host, redirects
    /// blocked, and concurrent fetches for the same message coalesced.
    pub async fn fetch_reply_context(&self, message_guid: &str) -> Result<Option<serde_json::Value>> {
        if !self.reply_context_api_fallback {
            return Ok(None);
        }
        let api_url = self
            .api_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles api_url not configured"))?;
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles password not configured"))?;

        let key = reply_context_dedupe_key("default", message_guid);
        if !self.reply_context_coalescer.begin(&key) {
            // A fetch for this message is already in flight — coalesce.
            return Ok(None);
        }
        let result = async {
            let url = build_reply_context_message_url(api_url, message_guid, password)?;
            // SSRF guard: the built URL must still target the configured host.
            let url = validate_reply_context_url(api_url, url.as_str())?;
            let client = build_reply_context_client()?;
            let resp = client.get(url.clone()).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                anyhow::bail!(
                    "BlueBubbles reply-context fetch failed ({status}) for {}",
                    redact_bluebubbles_log(url.as_str())
                );
            }
            Ok::<_, anyhow::Error>(Some(resp.json::<serde_json::Value>().await?))
        }
        .await;
        self.reply_context_coalescer.finish(&key);
        result
    }
}

#[async_trait]
impl ChannelPlugin for BlueBubblesChannel {
    fn id(&self) -> &str {
        "bluebubbles"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "BlueBubbles".to_string(),
            description: "BlueBubbles iMessage bridge for sending/receiving iMessages".to_string(),
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
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let api_url = match &self.api_url {
            Some(url) => url,
            None => {
                warn!("BlueBubbles channel enabled but no api_url configured");
                return Ok(());
            }
        };

        if self.password.is_none() {
            warn!("BlueBubbles channel enabled but no password configured");
            return Ok(());
        }

        info!(api_url = %api_url, "BlueBubbles channel starting");

        // Verify server connectivity by calling the server info endpoint.
        let password = self.password.as_deref().unwrap_or_default();
        let info_url = format!(
            "{}/api/v1/server/info?password={}",
            api_url.trim_end_matches('/'),
            password,
        );

        match self.client.get(&info_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("BlueBubbles: server connectivity verified");
            }
            Ok(resp) => {
                warn!("BlueBubbles: server returned status {}", resp.status());
            }
            Err(e) => {
                // Never log the password-bearing URL unredacted.
                warn!(
                    "BlueBubbles: failed to reach server: {}",
                    redact_bluebubbles_log(&e.to_string())
                );
            }
        }

        // Integration point: register a webhook endpoint for incoming
        // messages (reply-context fallback fills webhook gaps via
        // fetch_reply_context), or poll the messages endpoint.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("BlueBubbles channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let api_url = self
            .api_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles api_url not configured"))?;

        let password = self
            .password
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles password not configured"))?;

        // `to` is a phone number or iMessage email address.
        let url = format!(
            "{}/api/v1/message/text?password={}",
            api_url.trim_end_matches('/'),
            password,
        );

        let body = serde_json::json!({
            "chatGuid": format!("iMessage;-;{}", to),
            "tempGuid": uuid::Uuid::new_v4().to_string(),
            "message": message,
            "method": "private-api",
        });

        info!(to = %to, "BlueBubbles: sending iMessage");

        let resp = self.client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "BlueBubbles send failed ({}): {}",
                status,
                redact_bluebubbles_log(&text)
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

    // ---- log redaction ----

    #[test]
    fn redacts_password_and_token_query_params() {
        assert_eq!(
            redact_bluebubbles_log("GET http://mac:1234/api/v1/message/text?password=hunter2"),
            "GET http://mac:1234/api/v1/message/text?password=[REDACTED]"
        );
        assert_eq!(
            redact_bluebubbles_log("url?a=1&token=abc123&b=2"),
            "url?a=1&token=[REDACTED]&b=2"
        );
        assert_eq!(
            redact_bluebubbles_log("?PASSWORD=X&Token=Y"),
            "?PASSWORD=[REDACTED]&Token=[REDACTED]"
        );
    }

    #[test]
    fn redacts_authorization_headers_and_leaves_rest() {
        assert_eq!(
            redact_bluebubbles_log("Authorization: Bearer sekrit\nAccept: json"),
            "Authorization: [REDACTED]\nAccept: json"
        );
        assert_eq!(redact_bluebubbles_log("nothing secret here"), "nothing secret here");
    }

    // ---- SSRF guard ----

    #[test]
    fn reply_context_url_must_match_configured_host() {
        let cfg = "http://192.168.1.10:1234";
        assert!(validate_reply_context_url(cfg, "http://192.168.1.10:1234/api/v1/message/G1").is_ok());
        // Different host, port, or scheme rejected.
        assert!(validate_reply_context_url(cfg, "http://evil.example/api").is_err());
        assert!(validate_reply_context_url(cfg, "http://192.168.1.10:9999/api").is_err());
        assert!(validate_reply_context_url(cfg, "https://192.168.1.10:1234/api").is_err());
        // Credentials and non-http schemes rejected.
        assert!(validate_reply_context_url(cfg, "http://u:p@192.168.1.10:1234/api").is_err());
        assert!(validate_reply_context_url(cfg, "file:///etc/passwd").is_err());
        // Host compare is case-insensitive; default ports normalize.
        assert!(validate_reply_context_url("http://Mac.local", "http://mac.LOCAL:80/x").is_ok());
    }

    #[test]
    fn reply_context_message_url_targets_configured_server() {
        let url =
            build_reply_context_message_url("http://mac:1234", "GUID-1", "hunter2").unwrap();
        assert_eq!(url.host_str(), Some("mac"));
        assert_eq!(url.path(), "/api/v1/message/GUID-1");
        assert!(url.query().unwrap().contains("password=hunter2"));
        assert!(url.query().unwrap().contains("with=chats"));
        // Redacted rendering hides the password.
        assert!(!redact_bluebubbles_log(url.as_str()).contains("hunter2"));
        // Still passes the SSRF guard.
        assert!(validate_reply_context_url("http://mac:1234", url.as_str()).is_ok());
    }

    // ---- dedupe coalescing ----

    #[test]
    fn reply_context_fetches_coalesce_per_message() {
        let coalescer = ReplyContextCoalescer::new();
        let key_a = reply_context_dedupe_key("acct", "G-1 ");
        let key_b = reply_context_dedupe_key("acct", "G-2");
        assert_eq!(key_a, "acct:G-1");
        // First fetch wins; concurrent duplicate coalesces.
        assert!(coalescer.begin(&key_a));
        assert!(!coalescer.begin(&key_a));
        // Different message id fetches independently.
        assert!(coalescer.begin(&key_b));
        assert_eq!(coalescer.in_flight_count(), 2);
        // After finish the same message can fetch again.
        coalescer.finish(&key_a);
        assert!(coalescer.begin(&key_a));
    }

    #[test]
    fn reply_context_fallback_is_opt_in() {
        let mut config = BlueBubblesExtensionConfig {
            api_url: Some("http://mac:1234".into()),
            password: Some("p".into()),
            ..Default::default()
        };
        assert!(!reply_context_fallback_enabled(&config));
        config.reply_context_api_fallback = Some(true);
        assert!(reply_context_fallback_enabled(&config));
        // Missing connection details keep it off even when opted in.
        config.password = None;
        assert!(!reply_context_fallback_enabled(&config));
    }

    // ---- extension config parsing ----

    #[test]
    fn extension_config_parses_aliases() {
        let value = serde_json::json!({
            "enabled": true,
            "serverUrl": "http://mac:1234",
            "password": "p",
            "replyContextApiFallback": true,
            "mediaMaxMb": 16,
        });
        let config = BlueBubblesExtensionConfig::from_extension_value(&value);
        assert_eq!(config.enabled, Some(true));
        assert_eq!(config.api_url.as_deref(), Some("http://mac:1234"));
        assert_eq!(config.password.as_deref(), Some("p"));
        assert_eq!(config.reply_context_api_fallback, Some(true));
        assert_eq!(config.media_max_mb, Some(16.0));
        // Garbage degrades to defaults instead of erroring.
        let config = BlueBubblesExtensionConfig::from_extension_value(&serde_json::json!("nope"));
        assert_eq!(config.enabled, None);
    }

    // ---- UTI audio classification ----

    #[test]
    fn apple_audio_utis_classify_as_audio() {
        for uti in [
            "public.audio",
            "com.apple.coreaudio-format",
            "com.apple.m4a-audio",
            "public.mp3",
            "public.mpeg-4-audio",
            "public.aiff-audio",
            "com.microsoft.waveform-audio",
            "PUBLIC.AUDIO",
            " public.audio ",
        ] {
            assert_eq!(
                classify_apple_uti(uti),
                Some(BlueBubblesAttachmentKind::Audio),
                "{uti} should be audio"
            );
        }
        assert_eq!(classify_apple_uti("public.jpeg"), Some(BlueBubblesAttachmentKind::Image));
        assert_eq!(
            classify_apple_uti("com.apple.quicktime-movie"),
            Some(BlueBubblesAttachmentKind::Video)
        );
        assert_eq!(classify_apple_uti("public.data"), None);
        assert_eq!(classify_apple_uti(""), None);
    }

    #[test]
    fn full_classifier_uses_uti_then_mime_then_extension() {
        use BlueBubblesAttachmentKind as Kind;
        // A voice memo: audio UTI, no MIME.
        assert_eq!(
            classify_bluebubbles_attachment(Some("com.apple.coreaudio-format"), None, Some("Audio Message.caf")),
            Kind::Audio
        );
        // UTI wins over conflicting MIME.
        assert_eq!(
            classify_bluebubbles_attachment(Some("public.audio"), Some("application/octet-stream"), None),
            Kind::Audio
        );
        // MIME fallback.
        assert_eq!(classify_bluebubbles_attachment(None, Some("audio/mp4"), None), Kind::Audio);
        assert_eq!(classify_bluebubbles_attachment(None, Some("image/png"), None), Kind::Image);
        assert_eq!(classify_bluebubbles_attachment(None, Some("video/mp4"), None), Kind::Video);
        // Extension fallback.
        assert_eq!(classify_bluebubbles_attachment(None, None, Some("memo.m4a")), Kind::Audio);
        assert_eq!(classify_bluebubbles_attachment(None, None, Some("pic.HEIC")), Kind::Image);
        assert_eq!(classify_bluebubbles_attachment(None, None, Some("clip.mov")), Kind::Video);
        // Unknown -> file.
        assert_eq!(classify_bluebubbles_attachment(None, None, Some("doc.pdf")), Kind::File);
        assert_eq!(classify_bluebubbles_attachment(None, None, None), Kind::File);
    }
}
