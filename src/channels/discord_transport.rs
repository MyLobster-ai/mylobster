//! Discord transport behavior (v2026.7.1).
//!
//! Ports of OpenClaw `extensions/discord/src/network-config.ts` (IPv4-preferred
//! REST/gateway DNS ordering), `internal/gateway.ts` (bounded websocket
//! payloads, failed-resume recovery, drop-during-reconnect fix),
//! `internal/rest-errors.ts` (error 10065 treated as disconnected), and
//! `internal/command-deploy.ts` (slash-command deploy hash persistence).
//!
//! Bundled-native port; upstream ships these inside the Discord npm plugin.
//! (Learned 429 bucket cooldowns live in `discord.rs` — `RateLimitBook`.)

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;

// ============================================================================
// IPv4-preferred REST/gateway transport
// ============================================================================

/// Hostnames covered by the Discord IPv4 preference.
pub const DISCORD_DNS_HOSTS: &[&str] = &["discord.com", "discord.gg", "gateway.discord.gg"];

/// Whether a hostname is a Discord transport host (exact or subdomain match).
pub fn is_discord_transport_hostname(hostname: &str) -> bool {
    let normalized = hostname.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    DISCORD_DNS_HOSTS
        .iter()
        .any(|target| normalized == *target || normalized.ends_with(&format!(".{}", target)))
}

/// Reorder resolved addresses so IPv4 comes first (IPv6 kept as fallback).
pub fn reorder_ipv4_first(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    if addresses.len() < 2 {
        return addresses;
    }
    let (v4, v6): (Vec<SocketAddr>, Vec<SocketAddr>) =
        addresses.into_iter().partition(|addr| addr.is_ipv4());
    v4.into_iter().chain(v6).collect()
}

/// Resolve a Discord host `host:port` with IPv4 preference using the system
/// resolver. Non-Discord hosts return their natural order.
pub fn resolve_ipv4_preferred(host: &str, port: u16) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(|iter| iter.collect())
        .unwrap_or_default();
    if is_discord_transport_hostname(host) {
        reorder_ipv4_first(addrs)
    } else {
        addrs
    }
}

// ============================================================================
// Bounded gateway websocket/metadata payloads
// ============================================================================

/// Max gateway websocket payload accepted (16 MiB, matching upstream).
pub const DISCORD_GATEWAY_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Max `/gateway/bot` metadata response accepted.
pub const DISCORD_GATEWAY_METADATA_MAX_BYTES: usize = 1024 * 1024;
/// Default timeout for `/gateway/bot` metadata lookup before falling back to
/// the default gateway URL.
pub const DEFAULT_GATEWAY_INFO_TIMEOUT_MS: u64 = 30_000;

/// Whether a gateway websocket payload is within bounds.
pub fn is_gateway_payload_within_bounds(len_bytes: usize) -> bool {
    len_bytes <= DISCORD_GATEWAY_MAX_PAYLOAD_BYTES
}

/// Whether a `/gateway/bot` metadata response is within bounds.
pub fn is_gateway_metadata_within_bounds(len_bytes: usize) -> bool {
    len_bytes <= DISCORD_GATEWAY_METADATA_MAX_BYTES
}

/// Resolve `gatewayInfoTimeoutMs` (default 30000).
pub fn resolve_gateway_info_timeout_ms(configured: Option<u64>) -> u64 {
    configured.unwrap_or(DEFAULT_GATEWAY_INFO_TIMEOUT_MS)
}

// ============================================================================
// Error 10065 as disconnected
// ============================================================================

/// Discord error code: Unknown Voice State.
pub const DISCORD_UNKNOWN_VOICE_STATE: u64 = 10065;

/// Read the Discord error `code` from an error body.
pub fn read_discord_error_code(body: Option<&Value>) -> Option<u64> {
    body?.get("code")?.as_u64()
}

/// Whether an error is the Unknown Voice State error (10065) — treated as a
/// plain "disconnected" state instead of a hard failure.
pub fn is_unknown_voice_state_error(discord_code: Option<u64>, message: &str) -> bool {
    discord_code == Some(DISCORD_UNKNOWN_VOICE_STATE)
        || message.to_lowercase().contains("unknown voice state")
}

// ============================================================================
// Failed-resume recovery + reconnect state
// ============================================================================

/// Default max reconnect attempts (upstream gateway option).
pub const DISCORD_GATEWAY_MAX_RECONNECT_ATTEMPTS: u32 = 50;

/// What the next connection attempt should send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayConnectPlan {
    /// Fresh Identify (no resumable session).
    Identify,
    /// Resume with the stored session.
    Resume {
        gateway_url: String,
        session_id: String,
        sequence: u64,
    },
}

/// Gateway session/resume state machine: tracks session id, resume URL, and
/// sequence; a failed resume (non-resumable invalid session) clears the state
/// so the next attempt performs a full re-identify instead of looping on
/// broken resumes.
#[derive(Debug, Default, Clone)]
pub struct GatewayResumeState {
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    sequence: Option<u64>,
    reconnect_attempts: u32,
}

impl GatewayResumeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// READY received: store the resumable session and reset attempts.
    pub fn on_ready(&mut self, session_id: &str, resume_gateway_url: Option<&str>) {
        self.session_id = Some(session_id.to_string());
        self.resume_gateway_url = resume_gateway_url.map(|url| url.to_string());
        self.reconnect_attempts = 0;
    }

    /// RESUMED received: session is healthy again.
    pub fn on_resumed(&mut self) {
        self.reconnect_attempts = 0;
    }

    /// Dispatch received with a sequence number.
    pub fn on_dispatch(&mut self, sequence: u64) {
        self.sequence = Some(sequence);
    }

    /// Invalid Session opcode: `resumable=false` is a failed resume — drop the
    /// stored session so the next attempt re-identifies from scratch.
    pub fn on_invalid_session(&mut self, resumable: bool) {
        if !resumable {
            self.session_id = None;
            self.resume_gateway_url = None;
            self.sequence = None;
        }
    }

    /// Socket closed; returns `None` when the reconnect budget is exhausted.
    pub fn on_close(&mut self, max_attempts: u32) -> Option<u32> {
        self.reconnect_attempts += 1;
        if self.reconnect_attempts > max_attempts {
            None
        } else {
            Some(self.reconnect_attempts)
        }
    }

    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    /// Plan the next connection attempt.
    pub fn connect_plan(&self, prefer_resume: bool) -> GatewayConnectPlan {
        if prefer_resume {
            if let (Some(session_id), Some(url), Some(sequence)) = (
                self.session_id.as_ref(),
                self.resume_gateway_url.as_ref(),
                self.sequence,
            ) {
                return GatewayConnectPlan::Resume {
                    gateway_url: url.clone(),
                    session_id: session_id.clone(),
                    sequence,
                };
            }
        }
        GatewayConnectPlan::Identify
    }
}

/// Bounded buffer holding outbound payloads while the gateway reconnects, so
/// sends issued mid-reconnect drain in order once the socket is back instead
/// of being dropped (drop-during-reconnect fix). Oldest entries are evicted
/// when the buffer overflows.
#[derive(Debug)]
pub struct ReconnectSendBuffer {
    max_entries: usize,
    entries: VecDeque<Value>,
    dropped: u64,
}

impl ReconnectSendBuffer {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            entries: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Queue a payload during reconnect.
    pub fn enqueue(&mut self, payload: Value) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(payload);
    }

    /// Drain all buffered payloads in send order (call on reconnect).
    pub fn drain(&mut self) -> Vec<Value> {
        self.entries.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Count of payloads evicted due to overflow.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ============================================================================
// Slash-command deploy hash persistence
// ============================================================================

fn stable_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), stable_value(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(entries) => Value::Array(entries.iter().map(stable_value).collect()),
        other => other.clone(),
    }
}

fn stable_command_key(command: &Value) -> String {
    let kind = command.get("type").and_then(|t| t.as_u64()).unwrap_or(1);
    let name = command.get("name").and_then(|n| n.as_str()).unwrap_or("");
    format!("{}:{}", kind, name)
}

/// Stable hash of a serialized command set (order-insensitive, sorted keys).
pub fn stable_command_set_hash(commands: &[Value]) -> String {
    let mut stable: Vec<Value> = commands.iter().map(stable_value).collect();
    stable.sort_by_key(|cmd| stable_command_key(cmd));
    let payload = serde_json::to_string(&stable).unwrap_or_default();
    let digest = Sha256::digest(payload.as_bytes());
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Scope cache keys by application id so multi-bot setups sharing one cache
/// file still reconcile each application separately.
pub fn deploy_cache_key(client_id: &str, suffix: &str) -> String {
    format!("app:{}:{}", client_id, suffix)
}

/// Persistent slash-command deploy hash cache: identical command sets skip
/// redeploy across restarts (rate-limit protection for command PUTs).
#[derive(Debug)]
pub struct CommandDeployCache {
    store_path: Option<std::path::PathBuf>,
    hashes: HashMap<String, String>,
}

impl CommandDeployCache {
    /// In-memory cache (no persistence).
    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            hashes: HashMap::new(),
        }
    }

    /// Load (or initialize) a cache persisted at `path`.
    pub fn open(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let hashes = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        Self {
            store_path: Some(path),
            hashes,
        }
    }

    /// Whether this command set differs from the last deployed set for `key`.
    pub fn should_deploy(&self, key: &str, commands: &[Value], force: bool) -> bool {
        if force {
            return true;
        }
        self.hashes.get(key).map(String::as_str) != Some(stable_command_set_hash(commands).as_str())
    }

    /// Record a successful deploy and persist the hash store.
    pub fn mark_deployed(&mut self, key: &str, commands: &[Value]) -> Result<()> {
        self.hashes
            .insert(key.to_string(), stable_command_set_hash(commands));
        if let Some(path) = &self.store_path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&self.hashes)?)?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- IPv4 preference ----------------------------------------------------

    #[test]
    fn discord_hostnames_detected() {
        assert!(is_discord_transport_hostname("discord.com"));
        assert!(is_discord_transport_hostname("Gateway.Discord.gg"));
        assert!(is_discord_transport_hostname("cdn.discord.com"));
        assert!(!is_discord_transport_hostname("example.com"));
        assert!(!is_discord_transport_hostname("notdiscord.com"));
        assert!(!is_discord_transport_hostname(""));
    }

    #[test]
    fn ipv4_ordered_first_with_ipv6_fallback() {
        let v4a: SocketAddr = "1.2.3.4:443".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let v4b: SocketAddr = "5.6.7.8:443".parse().unwrap();
        let ordered = reorder_ipv4_first(vec![v6, v4a, v4b]);
        assert_eq!(ordered, vec![v4a, v4b, v6]);
        // v6-only and single-entry lists unchanged.
        assert_eq!(reorder_ipv4_first(vec![v6]), vec![v6]);
        let v6_only = reorder_ipv4_first(vec![v6, v6]);
        assert_eq!(v6_only, vec![v6, v6]);
    }

    // ---- bounded payloads ---------------------------------------------------

    #[test]
    fn payload_bounds() {
        assert!(is_gateway_payload_within_bounds(16 * 1024 * 1024));
        assert!(!is_gateway_payload_within_bounds(16 * 1024 * 1024 + 1));
        assert!(is_gateway_metadata_within_bounds(1024));
        assert!(!is_gateway_metadata_within_bounds(2 * 1024 * 1024));
        assert_eq!(resolve_gateway_info_timeout_ms(None), 30_000);
        assert_eq!(resolve_gateway_info_timeout_ms(Some(5_000)), 5_000);
    }

    // ---- 10065 as disconnected ----------------------------------------------

    #[test]
    fn unknown_voice_state_classification() {
        assert!(is_unknown_voice_state_error(Some(10065), "boom"));
        assert!(is_unknown_voice_state_error(None, "Unknown Voice State"));
        assert!(!is_unknown_voice_state_error(Some(50001), "missing access"));
        let body = json!({ "code": 10065, "message": "Unknown Voice State" });
        assert_eq!(read_discord_error_code(Some(&body)), Some(10065));
        assert_eq!(read_discord_error_code(None), None);
    }

    // ---- resume state -------------------------------------------------------

    #[test]
    fn failed_resume_falls_back_to_identify() {
        let mut state = GatewayResumeState::new();
        assert_eq!(state.connect_plan(true), GatewayConnectPlan::Identify);
        state.on_ready("sess1", Some("wss://resume.example"));
        state.on_dispatch(42);
        assert_eq!(
            state.connect_plan(true),
            GatewayConnectPlan::Resume {
                gateway_url: "wss://resume.example".to_string(),
                session_id: "sess1".to_string(),
                sequence: 42,
            }
        );
        // prefer_resume=false always identifies.
        assert_eq!(state.connect_plan(false), GatewayConnectPlan::Identify);
        // Resumable invalid session keeps the stored session.
        state.on_invalid_session(true);
        assert!(matches!(state.connect_plan(true), GatewayConnectPlan::Resume { .. }));
        // Failed resume (non-resumable) clears it → full re-identify.
        state.on_invalid_session(false);
        assert_eq!(state.connect_plan(true), GatewayConnectPlan::Identify);
    }

    #[test]
    fn reconnect_budget() {
        let mut state = GatewayResumeState::new();
        for attempt in 1..=3 {
            assert_eq!(state.on_close(3), Some(attempt));
        }
        assert_eq!(state.on_close(3), None);
        // READY resets the budget.
        state.on_ready("sess", None);
        assert_eq!(state.reconnect_attempts(), 0);
        assert_eq!(state.on_close(3), Some(1));
    }

    // ---- drop-during-reconnect fix ------------------------------------------

    #[test]
    fn reconnect_buffer_preserves_order_and_bounds() {
        let mut buffer = ReconnectSendBuffer::new(3);
        for i in 0..5 {
            buffer.enqueue(json!({ "seq": i }));
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.dropped(), 2);
        let drained = buffer.drain();
        assert_eq!(
            drained.iter().map(|v| v["seq"].as_u64().unwrap()).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(buffer.is_empty());
    }

    // ---- deploy hash persistence --------------------------------------------

    fn commands_a() -> Vec<Value> {
        vec![
            json!({ "name": "help", "type": 1, "description": "Show help" }),
            json!({ "description": "Ask", "type": 1, "name": "ask" }),
        ]
    }

    #[test]
    fn hash_is_order_and_key_order_insensitive() {
        let a = stable_command_set_hash(&commands_a());
        let reversed: Vec<Value> = commands_a().into_iter().rev().collect();
        assert_eq!(a, stable_command_set_hash(&reversed));
        let changed = vec![json!({ "name": "help", "type": 1, "description": "Changed" })];
        assert_ne!(a, stable_command_set_hash(&changed));
    }

    #[test]
    fn deploy_cache_skips_identical_sets_across_restarts() {
        let dir = std::env::temp_dir().join("mylobster-discord-deploy-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("command-deploy-cache.json");
        let key = deploy_cache_key("app123", "global:reconcile");
        assert_eq!(key, "app:app123:global:reconcile");

        let mut cache = CommandDeployCache::open(&path);
        assert!(cache.should_deploy(&key, &commands_a(), false));
        cache.mark_deployed(&key, &commands_a()).unwrap();
        assert!(!cache.should_deploy(&key, &commands_a(), false));
        // force always redeploys.
        assert!(cache.should_deploy(&key, &commands_a(), true));

        // A fresh cache instance (restart) loads the persisted hash.
        let reloaded = CommandDeployCache::open(&path);
        assert!(!reloaded.should_deploy(&key, &commands_a(), false));
        // Different app id scopes separately.
        let other_key = deploy_cache_key("app999", "global:reconcile");
        assert!(reloaded.should_deploy(&other_key, &commands_a(), false));
    }

    #[test]
    fn in_memory_cache_works_without_persistence() {
        let mut cache = CommandDeployCache::in_memory();
        let key = deploy_cache_key("a", "g");
        assert!(cache.should_deploy(&key, &commands_a(), false));
        cache.mark_deployed(&key, &commands_a()).unwrap();
        assert!(!cache.should_deploy(&key, &commands_a(), false));
    }
}
