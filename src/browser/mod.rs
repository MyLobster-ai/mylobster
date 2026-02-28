// Browser automation module.
//
// v2026.2.26: Extension relay reconnect resilience, fill field type parity,
// CORS preflight for relay, auth token on relay endpoints.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

// ============================================================================
// v2026.2.26: Extension Relay Reconnect Resilience
// ============================================================================

/// Configuration for extension relay reconnection behavior.
#[derive(Debug, Clone)]
pub struct RelayReconnectConfig {
    /// Initial delay before first reconnect attempt.
    pub initial_delay_ms: u64,
    /// Maximum delay between reconnect attempts.
    pub max_delay_ms: u64,
    /// Backoff multiplier applied to each subsequent attempt.
    pub backoff_multiplier: f64,
    /// Maximum number of reconnect attempts before giving up.
    pub max_attempts: u32,
}

impl Default for RelayReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 1000,
            max_delay_ms: 30_000,
            backoff_multiplier: 1.5,
            max_attempts: 20,
        }
    }
}

/// Tracks the state of an extension relay connection.
pub struct RelayConnectionState {
    /// Whether the relay is currently connected.
    connected: AtomicBool,
    /// Number of consecutive reconnect attempts.
    reconnect_attempts: AtomicU64,
    /// Configuration for reconnection behavior.
    config: RelayReconnectConfig,
}

impl RelayConnectionState {
    pub fn new(config: RelayReconnectConfig) -> Self {
        Self {
            connected: AtomicBool::new(false),
            reconnect_attempts: AtomicU64::new(0),
            config,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        self.reconnect_attempts.store(0, Ordering::Relaxed);
        info!("Extension relay connected");
    }

    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
        warn!("Extension relay disconnected");
    }

    /// Calculate the delay before the next reconnect attempt.
    ///
    /// Uses exponential backoff with the configured multiplier and cap.
    pub fn next_reconnect_delay(&self) -> Option<Duration> {
        let attempts = self.reconnect_attempts.fetch_add(1, Ordering::Relaxed);

        if attempts >= self.config.max_attempts as u64 {
            warn!(
                "Extension relay reconnect: max attempts ({}) reached",
                self.config.max_attempts
            );
            return None;
        }

        let delay_ms = (self.config.initial_delay_ms as f64
            * self.config.backoff_multiplier.powi(attempts as i32))
            as u64;
        let capped = delay_ms.min(self.config.max_delay_ms);

        debug!(
            "Extension relay reconnect attempt {}/{}, delay {}ms",
            attempts + 1,
            self.config.max_attempts,
            capped
        );

        Some(Duration::from_millis(capped))
    }

    /// Reset reconnect state (e.g., after successful connection).
    pub fn reset_reconnect(&self) {
        self.reconnect_attempts.store(0, Ordering::Relaxed);
    }
}

// ============================================================================
// v2026.2.26: Fill Field Type Parity
// ============================================================================

/// Supported fill field types for browser form automation.
///
/// Mirrors the field types that the browser extension supports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FillFieldType {
    /// Standard text input.
    Text,
    /// Password field (masked input).
    Password,
    /// Email input.
    Email,
    /// URL input.
    Url,
    /// Telephone number input.
    Tel,
    /// Numeric input.
    Number,
    /// Search input.
    Search,
    /// Date input.
    Date,
    /// Time input.
    Time,
    /// DateTime-local input.
    DatetimeLocal,
    /// Textarea (multi-line text).
    Textarea,
    /// Select / dropdown.
    Select,
    /// Checkbox.
    Checkbox,
    /// Radio button.
    Radio,
    /// File upload.
    File,
    /// Color picker.
    Color,
    /// Range / slider.
    Range,
    /// Hidden field.
    Hidden,
    /// Content-editable element (not a standard input).
    ContentEditable,
}

/// A fill instruction for the browser extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillInstruction {
    /// CSS selector or XPath to locate the field.
    pub selector: String,
    /// Value to fill.
    pub value: String,
    /// Field type hint.
    pub field_type: FillFieldType,
    /// Whether to clear existing content before filling.
    #[serde(default = "default_true")]
    pub clear_first: bool,
    /// Whether to trigger change/input events after filling.
    #[serde(default = "default_true")]
    pub trigger_events: bool,
}

fn default_true() -> bool {
    true
}

// ============================================================================
// v2026.2.26: CORS Preflight Configuration
// ============================================================================

/// CORS configuration for browser extension relay endpoints.
#[derive(Debug, Clone)]
pub struct RelayCorsConfig {
    /// Allowed origins for CORS. Empty = allow all.
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    pub allowed_methods: Vec<String>,
    /// Allowed headers.
    pub allowed_headers: Vec<String>,
    /// Whether to allow credentials.
    pub allow_credentials: bool,
    /// Max age for preflight cache (seconds).
    pub max_age_seconds: u32,
}

impl Default for RelayCorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec![],
            allowed_methods: vec![
                "GET".to_string(),
                "POST".to_string(),
                "OPTIONS".to_string(),
            ],
            allowed_headers: vec![
                "Content-Type".to_string(),
                "Authorization".to_string(),
                "X-Relay-Token".to_string(),
            ],
            allow_credentials: true,
            max_age_seconds: 3600,
        }
    }
}

impl RelayCorsConfig {
    /// Check if an origin is allowed.
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        if self.allowed_origins.is_empty() {
            return true;
        }
        self.allowed_origins.iter().any(|o| o == origin || o == "*")
    }

    /// Build CORS headers for a response.
    pub fn build_headers(&self, origin: Option<&str>) -> Vec<(String, String)> {
        let mut headers = Vec::new();

        let origin_value = if let Some(o) = origin {
            if self.is_origin_allowed(o) {
                o.to_string()
            } else {
                return headers; // No CORS headers if origin not allowed
            }
        } else if self.allowed_origins.is_empty() {
            "*".to_string()
        } else {
            return headers;
        };

        headers.push(("Access-Control-Allow-Origin".to_string(), origin_value));
        headers.push((
            "Access-Control-Allow-Methods".to_string(),
            self.allowed_methods.join(", "),
        ));
        headers.push((
            "Access-Control-Allow-Headers".to_string(),
            self.allowed_headers.join(", "),
        ));

        if self.allow_credentials {
            headers.push((
                "Access-Control-Allow-Credentials".to_string(),
                "true".to_string(),
            ));
        }

        headers.push((
            "Access-Control-Max-Age".to_string(),
            self.max_age_seconds.to_string(),
        ));

        headers
    }
}

// ============================================================================
// v2026.2.26: Relay Auth Token
// ============================================================================

/// Validates an auth token on relay endpoints.
///
/// The relay token is separate from the main gateway JWT — it's a simple
/// bearer token that the extension uses to authenticate with the relay.
pub fn validate_relay_token(
    provided: Option<&str>,
    expected: Option<&str>,
) -> bool {
    match (provided, expected) {
        (_, None) => true, // No token required
        (None, Some(_)) => false, // Token required but not provided
        (Some(p), Some(e)) => {
            // Constant-time comparison to prevent timing attacks
            if p.len() != e.len() {
                return false;
            }
            p.bytes()
                .zip(e.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // Relay Connection State
    // ====================================================================

    #[test]
    fn relay_initial_state_disconnected() {
        let state = RelayConnectionState::new(RelayReconnectConfig::default());
        assert!(!state.is_connected());
    }

    #[test]
    fn relay_connect_disconnect_cycle() {
        let state = RelayConnectionState::new(RelayReconnectConfig::default());
        state.mark_connected();
        assert!(state.is_connected());
        state.mark_disconnected();
        assert!(!state.is_connected());
    }

    #[test]
    fn relay_reconnect_backoff() {
        let config = RelayReconnectConfig {
            initial_delay_ms: 100,
            max_delay_ms: 1000,
            backoff_multiplier: 2.0,
            max_attempts: 5,
        };
        let state = RelayConnectionState::new(config);

        let d1 = state.next_reconnect_delay().unwrap();
        assert_eq!(d1.as_millis(), 100); // 100 * 2^0

        let d2 = state.next_reconnect_delay().unwrap();
        assert_eq!(d2.as_millis(), 200); // 100 * 2^1

        let d3 = state.next_reconnect_delay().unwrap();
        assert_eq!(d3.as_millis(), 400); // 100 * 2^2

        let d4 = state.next_reconnect_delay().unwrap();
        assert_eq!(d4.as_millis(), 800); // 100 * 2^3

        let d5 = state.next_reconnect_delay().unwrap();
        assert_eq!(d5.as_millis(), 1000); // capped at max_delay_ms

        // Max attempts reached
        assert!(state.next_reconnect_delay().is_none());
    }

    #[test]
    fn relay_reconnect_reset() {
        let config = RelayReconnectConfig {
            initial_delay_ms: 100,
            max_delay_ms: 1000,
            backoff_multiplier: 2.0,
            max_attempts: 3,
        };
        let state = RelayConnectionState::new(config);

        state.next_reconnect_delay();
        state.next_reconnect_delay();
        state.reset_reconnect();

        // After reset, should start from 0 again
        let d = state.next_reconnect_delay().unwrap();
        assert_eq!(d.as_millis(), 100);
    }

    // ====================================================================
    // Fill Field Type
    // ====================================================================

    #[test]
    fn fill_field_type_serialization() {
        let json = serde_json::to_string(&FillFieldType::ContentEditable).unwrap();
        assert_eq!(json, "\"contentEditable\"");

        let json = serde_json::to_string(&FillFieldType::DatetimeLocal).unwrap();
        assert_eq!(json, "\"datetimeLocal\"");

        let back: FillFieldType = serde_json::from_str("\"password\"").unwrap();
        assert_eq!(back, FillFieldType::Password);
    }

    #[test]
    fn fill_instruction_serialization() {
        let instruction = FillInstruction {
            selector: "#email".to_string(),
            value: "test@example.com".to_string(),
            field_type: FillFieldType::Email,
            clear_first: true,
            trigger_events: true,
        };

        let json = serde_json::to_value(&instruction).unwrap();
        assert_eq!(json["selector"], "#email");
        assert_eq!(json["fieldType"], "email");
        assert!(json["clearFirst"].as_bool().unwrap());
    }

    // ====================================================================
    // CORS Configuration
    // ====================================================================

    #[test]
    fn cors_default_allows_all() {
        let cors = RelayCorsConfig::default();
        assert!(cors.is_origin_allowed("https://example.com"));
        assert!(cors.is_origin_allowed("http://localhost:3000"));
    }

    #[test]
    fn cors_with_allowlist() {
        let cors = RelayCorsConfig {
            allowed_origins: vec!["https://mylobster.ai".to_string()],
            ..Default::default()
        };
        assert!(cors.is_origin_allowed("https://mylobster.ai"));
        assert!(!cors.is_origin_allowed("https://evil.com"));
    }

    #[test]
    fn cors_headers_include_origin() {
        let cors = RelayCorsConfig::default();
        let headers = cors.build_headers(Some("https://example.com"));
        assert!(!headers.is_empty());

        let origin_header = headers.iter().find(|(k, _)| k == "Access-Control-Allow-Origin");
        assert!(origin_header.is_some());
        assert_eq!(origin_header.unwrap().1, "https://example.com");
    }

    #[test]
    fn cors_headers_with_credentials() {
        let cors = RelayCorsConfig::default();
        let headers = cors.build_headers(Some("https://example.com"));
        let cred_header = headers
            .iter()
            .find(|(k, _)| k == "Access-Control-Allow-Credentials");
        assert!(cred_header.is_some());
        assert_eq!(cred_header.unwrap().1, "true");
    }

    #[test]
    fn cors_disallowed_origin_returns_no_headers() {
        let cors = RelayCorsConfig {
            allowed_origins: vec!["https://allowed.com".to_string()],
            ..Default::default()
        };
        let headers = cors.build_headers(Some("https://disallowed.com"));
        assert!(headers.is_empty());
    }

    // ====================================================================
    // Relay Auth Token
    // ====================================================================

    #[test]
    fn relay_token_no_requirement() {
        assert!(validate_relay_token(None, None));
        assert!(validate_relay_token(Some("anything"), None));
    }

    #[test]
    fn relay_token_required_but_missing() {
        assert!(!validate_relay_token(None, Some("secret")));
    }

    #[test]
    fn relay_token_valid() {
        assert!(validate_relay_token(Some("secret"), Some("secret")));
    }

    #[test]
    fn relay_token_invalid() {
        assert!(!validate_relay_token(Some("wrong"), Some("secret")));
    }

    #[test]
    fn relay_token_length_mismatch() {
        assert!(!validate_relay_token(Some("short"), Some("much-longer-token")));
    }
}
