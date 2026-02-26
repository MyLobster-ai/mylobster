use crate::config::Config;
use crate::gateway::{SessionInfo, SessionPatchParams};
use crate::providers::ProviderMessage;

use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Turn-Source Binding (v2026.2.24)
// ============================================================================

/// Identifies the originating channel/target for the current turn.
///
/// When a shared session is used by multiple channels, the turn source
/// records which channel initiated the current turn so that the reply
/// can be routed back to the correct destination.
#[derive(Debug, Clone, Default)]
pub struct TurnSource {
    pub channel: Option<String>,
    pub to: Option<String>,
    pub account_id: Option<String>,
    pub thread_id: Option<String>,
}

// ============================================================================
// Session Handle
// ============================================================================

/// A handle to a single session's data.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    info: parking_lot::RwLock<SessionInfo>,
    history: parking_lot::RwLock<Vec<ProviderMessage>>,
    /// Turn-source binding for reply routing (v2026.2.24).
    turn_source: parking_lot::RwLock<Option<TurnSource>>,
}

impl SessionHandle {
    fn new(info: SessionInfo) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                info: parking_lot::RwLock::new(info),
                history: parking_lot::RwLock::new(Vec::new()),
                turn_source: parking_lot::RwLock::new(None),
            }),
        }
    }

    /// Get the full conversation history for this session.
    pub fn get_history(&self) -> Vec<ProviderMessage> {
        self.inner.history.read().clone()
    }

    /// Append a message to this session's conversation history.
    pub fn add_message(&self, msg: ProviderMessage) {
        self.inner.history.write().push(msg);
    }

    /// Get a snapshot of the session info.
    fn info(&self) -> SessionInfo {
        self.inner.info.read().clone()
    }

    /// Apply a patch to the session info.
    fn patch(&self, params: &SessionPatchParams) {
        let mut info = self.inner.info.write();
        if let Some(ref title) = params.title {
            info.title = Some(String::clone(title));
        }
        if let Some(ref model) = params.model {
            info.model = Some(String::clone(model));
        }
        if let Some(ref thinking) = params.thinking {
            info.thinking = Some(String::clone(thinking));
        }
        info.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Get the current turn source for reply routing.
    pub fn get_turn_source(&self) -> Option<TurnSource> {
        self.inner.turn_source.read().clone()
    }

    /// Set the turn source for the current turn.
    pub fn set_turn_source(&self, source: TurnSource) {
        *self.inner.turn_source.write() = Some(source);
    }

    /// Clear the turn source (e.g. at end of turn).
    pub fn clear_turn_source(&self) {
        *self.inner.turn_source.write() = None;
    }
}

// ============================================================================
// Session Store
// ============================================================================

/// Session store that manages conversation sessions.
pub struct SessionStore {
    sessions: DashMap<String, SessionHandle>,
    _config: Config,
}

impl SessionStore {
    /// Create a new session store from configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            sessions: DashMap::new(),
            _config: config.clone(),
        }
    }

    /// List all active sessions.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|entry| entry.value().info())
            .collect()
    }

    /// Get a session by its key.
    pub fn get_session(&self, key: &str) -> Option<SessionInfo> {
        self.sessions.get(key).map(|entry| entry.value().info())
    }

    /// Return the number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    /// Delete a session by its key.
    pub fn delete_session(&self, key: &str) {
        self.sessions.remove(key);
    }

    /// Patch (update) a session's metadata.
    pub fn patch_session(&self, params: &SessionPatchParams) {
        if let Some(entry) = self.sessions.get(&params.session_key) {
            entry.value().patch(params);
        }
    }

    /// Get a session handle by its key (for direct access to history etc.).
    pub fn get_session_handle(&self, key: &str) -> Option<SessionHandle> {
        self.sessions.get(key).map(|entry| entry.value().clone())
    }

    /// Reset a session, clearing its conversation history.
    pub fn reset_session(&self, key: &str) -> bool {
        if let Some(entry) = self.sessions.get(key) {
            entry.value().inner.history.write().clear();
            entry.value().inner.turn_source.write().take();
            let mut info = entry.value().inner.info.write();
            info.updated_at = chrono::Utc::now().to_rfc3339();
            true
        } else {
            false
        }
    }

    /// Preview sessions — returns session info with message count.
    pub fn preview_sessions(&self) -> Vec<serde_json::Value> {
        self.sessions
            .iter()
            .map(|entry| {
                let info = entry.value().info();
                let msg_count = entry.value().inner.history.read().len();
                serde_json::json!({
                    "sessionKey": info.session_key,
                    "title": info.title,
                    "model": info.model,
                    "messageCount": msg_count,
                    "createdAt": info.created_at,
                    "updatedAt": info.updated_at,
                })
            })
            .collect()
    }

    /// Get usage stats for a session.
    pub fn get_session_usage(&self, key: &str) -> Option<serde_json::Value> {
        self.sessions.get(key).map(|entry| {
            let msg_count = entry.value().inner.history.read().len();
            serde_json::json!({
                "sessionKey": key,
                "messageCount": msg_count,
                "inputTokens": 0,
                "outputTokens": 0,
            })
        })
    }

    /// Resolve a session reference to a session key.
    /// Accepts session key, session ID, or partial match.
    pub fn resolve_session(&self, reference: &str) -> Option<String> {
        // Exact session key match
        if self.sessions.contains_key(reference) {
            return Some(reference.to_string());
        }
        // Match by session ID
        for entry in self.sessions.iter() {
            let info = entry.value().info();
            if info.id == reference {
                return Some(info.session_key);
            }
        }
        // Partial key match
        for entry in self.sessions.iter() {
            if entry.key().contains(reference) {
                return Some(entry.key().clone());
            }
        }
        None
    }

    /// Compact a session — no-op for in-memory store, but returns success.
    pub fn compact_session(&self, key: &str) -> bool {
        self.sessions.contains_key(key)
    }

    /// Get an existing session or create a new one for the given key.
    pub fn get_or_create_session(&self, key: &str, config: &Config) -> SessionHandle {
        if let Some(entry) = self.sessions.get(key) {
            return entry.value().clone();
        }

        let now = chrono::Utc::now().to_rfc3339();
        let model = config
            .agent
            .model
            .primary_model()
            .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

        let info = SessionInfo {
            id: Uuid::new_v4().to_string(),
            session_key: key.to_string(),
            agent_id: "default".to_string(),
            title: None,
            model: Some(model),
            thinking: None,
            created_at: now.clone(),
            updated_at: now,
        };

        let handle = SessionHandle::new(info);
        self.sessions.insert(key.to_string(), handle.clone());
        handle
    }
}
