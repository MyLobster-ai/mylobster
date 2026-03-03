// Browser automation module.
//
// v2026.2.26: Extension relay reconnect resilience, fill field type parity,
// CORS preflight for relay, auth token on relay endpoints.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

// ============================================================================
// Phase 8: Browser Profile Manager
// ============================================================================

/// An isolated browser profile for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserProfile {
    /// Unique session key identifying this profile.
    pub session_key: String,
    /// Path to the user data directory for this profile.
    pub user_data_dir: PathBuf,
    /// Unix timestamp (seconds) when this profile was created.
    pub created_at: u64,
    /// Whether cookies are enabled in this profile.
    pub cookies_enabled: bool,
    /// Whether JavaScript execution is enabled.
    pub javascript_enabled: bool,
}

/// Manages isolated browser contexts per session.
///
/// Each session gets its own `BrowserProfile` with a dedicated user data
/// directory, allowing full isolation of cookies, local storage, and cache.
pub struct BrowserProfileManager {
    /// Base directory under which profile directories are created.
    base_dir: PathBuf,
    /// Active profiles indexed by session key.
    profiles: HashMap<String, BrowserProfile>,
}

impl BrowserProfileManager {
    /// Create a new profile manager rooted at `base_dir`.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            profiles: HashMap::new(),
        }
    }

    /// Create a new browser profile for the given session key.
    ///
    /// The profile's user data directory is `{base_dir}/{session_key}`.
    /// If a profile for this session already exists it is returned as-is.
    pub fn create_profile(&mut self, session_key: &str) -> anyhow::Result<&BrowserProfile> {
        if self.profiles.contains_key(session_key) {
            info!(session_key, "Browser profile already exists");
            return Ok(self.profiles.get(session_key).unwrap());
        }

        let user_data_dir = self.base_dir.join(session_key);
        std::fs::create_dir_all(&user_data_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create profile dir: {}", e))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let profile = BrowserProfile {
            session_key: session_key.to_string(),
            user_data_dir,
            created_at: now,
            cookies_enabled: true,
            javascript_enabled: true,
        };

        info!(session_key, "Created browser profile");
        self.profiles.insert(session_key.to_string(), profile);
        Ok(self.profiles.get(session_key).unwrap())
    }

    /// Retrieve an existing profile by session key.
    pub fn get_profile(&self, session_key: &str) -> Option<&BrowserProfile> {
        self.profiles.get(session_key)
    }

    /// Destroy a profile, removing it from the manager.
    ///
    /// Note: does NOT delete the user data directory from disk. The caller
    /// is responsible for cleanup if desired.
    pub fn destroy_profile(&mut self, session_key: &str) {
        if self.profiles.remove(session_key).is_some() {
            info!(session_key, "Destroyed browser profile");
        } else {
            debug!(session_key, "No browser profile to destroy");
        }
    }

    /// List all active profiles.
    pub fn list_profiles(&self) -> Vec<&BrowserProfile> {
        self.profiles.values().collect()
    }
}

// ============================================================================
// Phase 8: Extension Loader
// ============================================================================

/// A loaded Chrome extension with parsed manifest metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedExtension {
    /// Extension display name from manifest.
    pub name: String,
    /// Filesystem path to the unpacked extension directory.
    pub path: PathBuf,
    /// Extension version string from manifest.
    pub version: String,
    /// Full parsed manifest.json contents.
    pub manifest: serde_json::Value,
}

/// Loads unpacked Chrome extensions from directories.
///
/// Each extension directory must contain a `manifest.json` file with at
/// minimum `name` and `version` fields.
pub struct ExtensionLoader {
    /// Successfully loaded extensions.
    extensions: Vec<LoadedExtension>,
}

impl ExtensionLoader {
    /// Create a new empty extension loader.
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
        }
    }

    /// Load an unpacked extension from `ext_path`.
    ///
    /// Reads `manifest.json` from the directory, extracts `name` and `version`,
    /// and stores the loaded extension. Returns an error if the manifest is
    /// missing or malformed.
    pub fn load_extension(&mut self, ext_path: &Path) -> anyhow::Result<&LoadedExtension> {
        let manifest_path = ext_path.join("manifest.json");
        let manifest_content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| anyhow::anyhow!("Failed to read manifest.json at {:?}: {}", manifest_path, e))?;

        let manifest: serde_json::Value = serde_json::from_str(&manifest_content)
            .map_err(|e| anyhow::anyhow!("Invalid manifest.json: {}", e))?;

        let name = manifest
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("manifest.json missing 'name' field"))?
            .to_string();

        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("manifest.json missing 'version' field"))?
            .to_string();

        let ext = LoadedExtension {
            name: name.clone(),
            path: ext_path.to_path_buf(),
            version: version.clone(),
            manifest,
        };

        info!(name = %ext.name, version = %ext.version, "Loaded extension");
        self.extensions.push(ext);
        Ok(self.extensions.last().unwrap())
    }

    /// Return all loaded extensions.
    pub fn loaded_extensions(&self) -> &[LoadedExtension] {
        &self.extensions
    }
}

impl Default for ExtensionLoader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Phase 8: Download Manager
// ============================================================================

/// Status of a tracked download.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DownloadStatus {
    /// Download has been registered but not started.
    Pending,
    /// Download is actively receiving data.
    InProgress,
    /// Download completed successfully.
    Completed,
    /// Download failed with an error message.
    Failed(String),
    /// Download was cancelled by the user or system.
    Cancelled,
}

/// A tracked file download entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEntry {
    /// Unique download identifier.
    pub id: String,
    /// Source URL.
    pub url: String,
    /// Local filesystem destination path.
    pub destination: PathBuf,
    /// Current download status.
    pub status: DownloadStatus,
    /// Unix timestamp (seconds) when the download was started.
    pub started_at: u64,
    /// Unix timestamp (seconds) when the download completed (if finished).
    pub completed_at: Option<u64>,
    /// Number of bytes downloaded so far.
    pub bytes_downloaded: u64,
    /// Total file size in bytes, if known from Content-Length.
    pub total_bytes: Option<u64>,
}

/// Captures and tracks file downloads initiated by browser automation.
pub struct DownloadManager {
    /// All tracked downloads, ordered by insertion.
    downloads: Vec<DownloadEntry>,
    /// Index from download ID to position in `downloads`.
    index: HashMap<String, usize>,
}

impl DownloadManager {
    /// Create a new empty download manager.
    pub fn new() -> Self {
        Self {
            downloads: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Register a new download and return its unique ID.
    pub fn track_download(&mut self, url: &str, dest: PathBuf) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = DownloadEntry {
            id: id.clone(),
            url: url.to_string(),
            destination: dest,
            status: DownloadStatus::Pending,
            started_at: now,
            completed_at: None,
            bytes_downloaded: 0,
            total_bytes: None,
        };

        info!(id = %id, url = %url, "Tracking new download");
        let idx = self.downloads.len();
        self.downloads.push(entry);
        self.index.insert(id.clone(), idx);
        id
    }

    /// Look up a download by its ID.
    pub fn get_download(&self, id: &str) -> Option<&DownloadEntry> {
        self.index.get(id).map(|&idx| &self.downloads[idx])
    }

    /// Return all tracked downloads.
    pub fn list_downloads(&self) -> &[DownloadEntry] {
        &self.downloads
    }
}

impl Default for DownloadManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Phase 8: Trace Logger
// ============================================================================

/// A single trace entry for network, DOM, or screenshot events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TraceEntry {
    /// A network request/response pair.
    Network {
        url: String,
        method: String,
        status: u16,
        duration_ms: u64,
        timestamp: u64,
    },
    /// A DOM event (click, input, mutation, etc.).
    DomEvent {
        event_type: String,
        selector: String,
        details: String,
        timestamp: u64,
    },
    /// A captured screenshot.
    Screenshot {
        path: PathBuf,
        width: u32,
        height: u32,
        timestamp: u64,
    },
}

/// Network/DOM/screenshot trace logger for browser debugging.
///
/// Collects a timeline of browser events that can be inspected after
/// a session to understand what happened.
pub struct TraceLogger {
    entries: Vec<TraceEntry>,
}

impl TraceLogger {
    /// Create a new empty trace logger.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Return the current unix timestamp in seconds.
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Log a network request/response.
    pub fn log_network(&mut self, request_url: &str, method: &str, status: u16, duration_ms: u64) {
        debug!(url = %request_url, method, status, duration_ms, "Trace: network");
        self.entries.push(TraceEntry::Network {
            url: request_url.to_string(),
            method: method.to_string(),
            status,
            duration_ms,
            timestamp: Self::now_secs(),
        });
    }

    /// Log a DOM event.
    pub fn log_dom_event(&mut self, event_type: &str, selector: &str, details: &str) {
        debug!(event_type, selector, details, "Trace: DOM event");
        self.entries.push(TraceEntry::DomEvent {
            event_type: event_type.to_string(),
            selector: selector.to_string(),
            details: details.to_string(),
            timestamp: Self::now_secs(),
        });
    }

    /// Log a captured screenshot.
    pub fn log_screenshot(&mut self, path: &Path, width: u32, height: u32) {
        debug!(?path, width, height, "Trace: screenshot");
        self.entries.push(TraceEntry::Screenshot {
            path: path.to_path_buf(),
            width,
            height,
            timestamp: Self::now_secs(),
        });
    }

    /// Return all trace entries.
    pub fn entries(&self) -> &[TraceEntry] {
        &self.entries
    }

    /// Clear all trace entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for TraceLogger {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Phase 8: Browserless Client
// ============================================================================

/// Client for a remote Browserless.io-compatible browser service.
///
/// Provides headless browser operations (navigate, screenshot, PDF) via
/// HTTP requests to a remote endpoint, useful when a local Chrome install
/// is unavailable.
pub struct BrowserlessClient {
    /// Base endpoint URL (e.g. `https://chrome.browserless.io`).
    endpoint: String,
    /// Optional authentication token.
    token: Option<String>,
    /// HTTP client for making requests.
    client: reqwest::Client,
}

impl BrowserlessClient {
    /// Create a new Browserless client.
    pub fn new(endpoint: &str, token: Option<&str>) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token: token.map(|t| t.to_string()),
            client: reqwest::Client::new(),
        }
    }

    /// Build the full URL for an API path, appending the token if present.
    fn build_url(&self, path: &str) -> String {
        let base = format!("{}{}", self.endpoint, path);
        match &self.token {
            Some(t) => format!("{}?token={}", base, t),
            None => base,
        }
    }

    /// Navigate to a URL and return the page HTML content.
    pub async fn navigate(&self, url: &str) -> anyhow::Result<String> {
        let api_url = self.build_url("/content");
        let body = serde_json::json!({ "url": url });
        info!(url, "Browserless: fetching content");

        let resp = self
            .client
            .post(&api_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Browserless content request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Browserless content returned {}: {}",
                status,
                text
            ));
        }

        resp.text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read content response: {}", e))
    }

    /// Take a screenshot of a URL and return PNG bytes.
    pub async fn screenshot(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let api_url = self.build_url("/screenshot");
        let body = serde_json::json!({ "url": url, "options": { "type": "png", "fullPage": true } });
        info!(url, "Browserless: taking screenshot");

        let resp = self
            .client
            .post(&api_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Browserless screenshot request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Browserless screenshot returned {}: {}",
                status,
                text
            ));
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| anyhow::anyhow!("Failed to read screenshot response: {}", e))
    }

    /// Generate a PDF of a URL and return PDF bytes.
    pub async fn pdf(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let api_url = self.build_url("/pdf");
        let body = serde_json::json!({ "url": url });
        info!(url, "Browserless: generating PDF");

        let resp = self
            .client
            .post(&api_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Browserless PDF request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Browserless PDF returned {}: {}",
                status,
                text
            ));
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| anyhow::anyhow!("Failed to read PDF response: {}", e))
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

    // ====================================================================
    // Browser Profile Manager
    // ====================================================================

    #[test]
    fn profile_manager_create_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = BrowserProfileManager::new(tmp.path().to_path_buf());

        let profile = mgr.create_profile("session-1").unwrap();
        assert_eq!(profile.session_key, "session-1");
        assert!(profile.cookies_enabled);
        assert!(profile.javascript_enabled);
        assert!(profile.user_data_dir.exists());
        assert!(profile.created_at > 0);

        let retrieved = mgr.get_profile("session-1");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().session_key, "session-1");
    }

    #[test]
    fn profile_manager_idempotent_create() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = BrowserProfileManager::new(tmp.path().to_path_buf());

        mgr.create_profile("s1").unwrap();
        // Second create returns existing profile without error
        let p = mgr.create_profile("s1").unwrap();
        assert_eq!(p.session_key, "s1");
        assert_eq!(mgr.list_profiles().len(), 1);
    }

    #[test]
    fn profile_manager_destroy() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = BrowserProfileManager::new(tmp.path().to_path_buf());

        mgr.create_profile("s1").unwrap();
        mgr.create_profile("s2").unwrap();
        assert_eq!(mgr.list_profiles().len(), 2);

        mgr.destroy_profile("s1");
        assert!(mgr.get_profile("s1").is_none());
        assert_eq!(mgr.list_profiles().len(), 1);
    }

    #[test]
    fn profile_manager_destroy_nonexistent_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = BrowserProfileManager::new(tmp.path().to_path_buf());
        mgr.destroy_profile("does-not-exist"); // should not panic
    }

    #[test]
    fn profile_manager_list_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BrowserProfileManager::new(tmp.path().to_path_buf());
        assert!(mgr.list_profiles().is_empty());
    }

    #[test]
    fn browser_profile_serialization() {
        let profile = BrowserProfile {
            session_key: "test-key".to_string(),
            user_data_dir: PathBuf::from("/tmp/test"),
            created_at: 1700000000,
            cookies_enabled: true,
            javascript_enabled: false,
        };

        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["session_key"], "test-key");
        assert_eq!(json["cookies_enabled"], true);
        assert_eq!(json["javascript_enabled"], false);

        let back: BrowserProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back.session_key, "test-key");
        assert!(!back.javascript_enabled);
    }

    // ====================================================================
    // Extension Loader
    // ====================================================================

    #[test]
    fn extension_loader_load_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("my-ext");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.json"),
            r#"{ "name": "Test Extension", "version": "1.2.3", "manifest_version": 3 }"#,
        )
        .unwrap();

        let mut loader = ExtensionLoader::new();
        let ext = loader.load_extension(&ext_dir).unwrap();
        assert_eq!(ext.name, "Test Extension");
        assert_eq!(ext.version, "1.2.3");
        assert_eq!(ext.manifest["manifest_version"], 3);

        assert_eq!(loader.loaded_extensions().len(), 1);
    }

    #[test]
    fn extension_loader_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("bad-ext");
        std::fs::create_dir_all(&ext_dir).unwrap();

        let mut loader = ExtensionLoader::new();
        let result = loader.load_extension(&ext_dir);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("manifest.json"),
            "Error should mention manifest.json"
        );
    }

    #[test]
    fn extension_loader_missing_name_field() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("no-name");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.json"),
            r#"{ "version": "1.0.0" }"#,
        )
        .unwrap();

        let mut loader = ExtensionLoader::new();
        let result = loader.load_extension(&ext_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn extension_loader_missing_version_field() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("no-version");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.json"),
            r#"{ "name": "Ext" }"#,
        )
        .unwrap();

        let mut loader = ExtensionLoader::new();
        let result = loader.load_extension(&ext_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version"));
    }

    #[test]
    fn extension_loader_default_is_empty() {
        let loader = ExtensionLoader::default();
        assert!(loader.loaded_extensions().is_empty());
    }

    #[test]
    fn loaded_extension_serialization() {
        let ext = LoadedExtension {
            name: "My Ext".to_string(),
            path: PathBuf::from("/opt/extensions/my-ext"),
            version: "2.0.0".to_string(),
            manifest: serde_json::json!({ "name": "My Ext", "version": "2.0.0" }),
        };

        let json = serde_json::to_value(&ext).unwrap();
        assert_eq!(json["name"], "My Ext");
        assert_eq!(json["version"], "2.0.0");
    }

    // ====================================================================
    // Download Manager
    // ====================================================================

    #[test]
    fn download_manager_track_and_get() {
        let mut mgr = DownloadManager::new();
        let id = mgr.track_download("https://example.com/file.zip", PathBuf::from("/tmp/file.zip"));

        assert!(!id.is_empty());

        let entry = mgr.get_download(&id).unwrap();
        assert_eq!(entry.url, "https://example.com/file.zip");
        assert_eq!(entry.destination, PathBuf::from("/tmp/file.zip"));
        assert_eq!(entry.status, DownloadStatus::Pending);
        assert_eq!(entry.bytes_downloaded, 0);
        assert!(entry.completed_at.is_none());
        assert!(entry.total_bytes.is_none());
        assert!(entry.started_at > 0);
    }

    #[test]
    fn download_manager_unique_ids() {
        let mut mgr = DownloadManager::new();
        let id1 = mgr.track_download("https://a.com/1", PathBuf::from("/tmp/1"));
        let id2 = mgr.track_download("https://a.com/2", PathBuf::from("/tmp/2"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn download_manager_list() {
        let mut mgr = DownloadManager::new();
        assert!(mgr.list_downloads().is_empty());

        mgr.track_download("https://a.com/1", PathBuf::from("/tmp/1"));
        mgr.track_download("https://a.com/2", PathBuf::from("/tmp/2"));
        assert_eq!(mgr.list_downloads().len(), 2);
    }

    #[test]
    fn download_manager_get_nonexistent() {
        let mgr = DownloadManager::new();
        assert!(mgr.get_download("nonexistent").is_none());
    }

    #[test]
    fn download_status_serialization() {
        let json = serde_json::to_string(&DownloadStatus::Pending).unwrap();
        assert_eq!(json, "\"Pending\"");

        let json = serde_json::to_string(&DownloadStatus::Failed("timeout".to_string())).unwrap();
        let back: DownloadStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, DownloadStatus::Failed("timeout".to_string()));

        let json = serde_json::to_string(&DownloadStatus::Cancelled).unwrap();
        assert_eq!(json, "\"Cancelled\"");
    }

    #[test]
    fn download_manager_default() {
        let mgr = DownloadManager::default();
        assert!(mgr.list_downloads().is_empty());
    }

    // ====================================================================
    // Trace Logger
    // ====================================================================

    #[test]
    fn trace_logger_log_network() {
        let mut logger = TraceLogger::new();
        logger.log_network("https://api.example.com/data", "GET", 200, 150);

        let entries = logger.entries();
        assert_eq!(entries.len(), 1);

        match &entries[0] {
            TraceEntry::Network {
                url,
                method,
                status,
                duration_ms,
                timestamp,
            } => {
                assert_eq!(url, "https://api.example.com/data");
                assert_eq!(method, "GET");
                assert_eq!(*status, 200);
                assert_eq!(*duration_ms, 150);
                assert!(*timestamp > 0);
            }
            _ => panic!("Expected Network entry"),
        }
    }

    #[test]
    fn trace_logger_log_dom_event() {
        let mut logger = TraceLogger::new();
        logger.log_dom_event("click", "#submit-btn", "Button clicked");

        let entries = logger.entries();
        assert_eq!(entries.len(), 1);

        match &entries[0] {
            TraceEntry::DomEvent {
                event_type,
                selector,
                details,
                ..
            } => {
                assert_eq!(event_type, "click");
                assert_eq!(selector, "#submit-btn");
                assert_eq!(details, "Button clicked");
            }
            _ => panic!("Expected DomEvent entry"),
        }
    }

    #[test]
    fn trace_logger_log_screenshot() {
        let mut logger = TraceLogger::new();
        logger.log_screenshot(Path::new("/tmp/shot.png"), 1920, 1080);

        let entries = logger.entries();
        assert_eq!(entries.len(), 1);

        match &entries[0] {
            TraceEntry::Screenshot {
                path,
                width,
                height,
                ..
            } => {
                assert_eq!(path, Path::new("/tmp/shot.png"));
                assert_eq!(*width, 1920);
                assert_eq!(*height, 1080);
            }
            _ => panic!("Expected Screenshot entry"),
        }
    }

    #[test]
    fn trace_logger_mixed_entries() {
        let mut logger = TraceLogger::new();
        logger.log_network("https://a.com", "POST", 201, 50);
        logger.log_dom_event("input", "#name", "typed");
        logger.log_screenshot(Path::new("/tmp/s.png"), 800, 600);

        assert_eq!(logger.entries().len(), 3);
    }

    #[test]
    fn trace_logger_clear() {
        let mut logger = TraceLogger::new();
        logger.log_network("https://a.com", "GET", 200, 10);
        logger.log_dom_event("click", "a", "link");
        assert_eq!(logger.entries().len(), 2);

        logger.clear();
        assert!(logger.entries().is_empty());
    }

    #[test]
    fn trace_logger_default() {
        let logger = TraceLogger::default();
        assert!(logger.entries().is_empty());
    }

    #[test]
    fn trace_entry_serialization() {
        let entry = TraceEntry::Network {
            url: "https://example.com".to_string(),
            method: "GET".to_string(),
            status: 200,
            duration_ms: 42,
            timestamp: 1700000000,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "network");
        assert_eq!(json["url"], "https://example.com");
        assert_eq!(json["status"], 200);

        let dom = TraceEntry::DomEvent {
            event_type: "click".to_string(),
            selector: "#btn".to_string(),
            details: "clicked".to_string(),
            timestamp: 1700000001,
        };
        let json = serde_json::to_value(&dom).unwrap();
        assert_eq!(json["type"], "domEvent");

        let shot = TraceEntry::Screenshot {
            path: PathBuf::from("/tmp/s.png"),
            width: 1024,
            height: 768,
            timestamp: 1700000002,
        };
        let json = serde_json::to_value(&shot).unwrap();
        assert_eq!(json["type"], "screenshot");
    }

    // ====================================================================
    // Browserless Client
    // ====================================================================

    #[test]
    fn browserless_build_url_without_token() {
        let client = BrowserlessClient::new("https://chrome.browserless.io", None);
        assert_eq!(
            client.build_url("/content"),
            "https://chrome.browserless.io/content"
        );
    }

    #[test]
    fn browserless_build_url_with_token() {
        let client = BrowserlessClient::new("https://chrome.browserless.io", Some("abc123"));
        assert_eq!(
            client.build_url("/content"),
            "https://chrome.browserless.io/content?token=abc123"
        );
    }

    #[test]
    fn browserless_strips_trailing_slash() {
        let client = BrowserlessClient::new("https://chrome.browserless.io/", Some("t"));
        assert_eq!(
            client.build_url("/pdf"),
            "https://chrome.browserless.io/pdf?token=t"
        );
    }

    #[test]
    fn browserless_new_stores_fields() {
        let client = BrowserlessClient::new("https://example.com", Some("secret"));
        assert_eq!(client.endpoint, "https://example.com");
        assert_eq!(client.token, Some("secret".to_string()));
    }
}
