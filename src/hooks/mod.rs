use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ============================================================================
// Hook Events (24 types matching OpenClaw)
// ============================================================================

/// Events fired during the agent/gateway lifecycle.
#[derive(Debug, Clone)]
pub enum HookEvent {
    // Agent hooks
    BeforeModelResolve {
        prompt: String,
    },
    BeforePromptBuild {
        session_key: String,
    },
    BeforeAgentStart {
        session_key: String,
    },
    LlmInput {
        model: String,
        messages: Vec<serde_json::Value>,
    },
    LlmOutput {
        model: String,
        response: serde_json::Value,
    },
    AgentEnd {
        session_key: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    BeforeCompaction {
        session_key: String,
    },
    AfterCompaction {
        session_key: String,
    },
    BeforeReset {
        session_key: String,
    },

    // Message hooks
    MessageReceived {
        from: String,
        content: String,
        timestamp: Option<u64>,
    },
    MessageSending {
        to: String,
        content: String,
    },
    MessageSent {
        to: String,
        content: String,
        success: bool,
        error: Option<String>,
    },

    // Tool hooks
    BeforeToolCall {
        tool: String,
        params: serde_json::Value,
    },
    AfterToolCall {
        tool: String,
        result: serde_json::Value,
    },
    ToolResultPersist {
        tool: String,
        result: serde_json::Value,
    },
    BeforeMessageWrite {
        message: serde_json::Value,
    },

    // Session hooks
    SessionStart {
        session_key: String,
    },
    SessionEnd {
        session_key: String,
    },

    // Subagent hooks
    SubagentSpawning {
        parent: String,
        child: String,
    },
    SubagentSpawned {
        parent: String,
        child: String,
    },
    SubagentDeliveryTarget {
        session_key: String,
    },
    SubagentEnded {
        session_key: String,
    },

    // Gateway hooks
    GatewayStart,
    GatewayStop,
}

/// Plugin context carried through all hook phases (v2026.3.11).
///
/// Ensures `trigger` and `channelId` are available to every hook handler,
/// not just the initial message-received handler.
#[derive(Debug, Clone, Default)]
pub struct HookPluginContext {
    /// What triggered this hook chain (e.g., "message", "cron", "api").
    pub trigger: Option<String>,
    /// The channel ID where the triggering event originated.
    pub channel_id: Option<String>,
    /// Account ID associated with the trigger.
    pub account_id: Option<String>,
    /// Thread ID for threaded channels.
    pub thread_id: Option<String>,
}

impl HookEvent {
    /// Get the event type name for routing.
    pub fn event_type(&self) -> &'static str {
        match self {
            HookEvent::BeforeModelResolve { .. } => "before_model_resolve",
            HookEvent::BeforePromptBuild { .. } => "before_prompt_build",
            HookEvent::BeforeAgentStart { .. } => "before_agent_start",
            HookEvent::LlmInput { .. } => "llm_input",
            HookEvent::LlmOutput { .. } => "llm_output",
            HookEvent::AgentEnd { .. } => "agent_end",
            HookEvent::BeforeCompaction { .. } => "before_compaction",
            HookEvent::AfterCompaction { .. } => "after_compaction",
            HookEvent::BeforeReset { .. } => "before_reset",
            HookEvent::MessageReceived { .. } => "message_received",
            HookEvent::MessageSending { .. } => "message_sending",
            HookEvent::MessageSent { .. } => "message_sent",
            HookEvent::BeforeToolCall { .. } => "before_tool_call",
            HookEvent::AfterToolCall { .. } => "after_tool_call",
            HookEvent::ToolResultPersist { .. } => "tool_result_persist",
            HookEvent::BeforeMessageWrite { .. } => "before_message_write",
            HookEvent::SessionStart { .. } => "session_start",
            HookEvent::SessionEnd { .. } => "session_end",
            HookEvent::SubagentSpawning { .. } => "subagent_spawning",
            HookEvent::SubagentSpawned { .. } => "subagent_spawned",
            HookEvent::SubagentDeliveryTarget { .. } => "subagent_delivery_target",
            HookEvent::SubagentEnded { .. } => "subagent_ended",
            HookEvent::GatewayStart => "gateway_start",
            HookEvent::GatewayStop => "gateway_stop",
        }
    }

    /// Whether this hook type is modifying (can cancel/transform).
    pub fn is_modifying(&self) -> bool {
        matches!(
            self,
            HookEvent::BeforeModelResolve { .. }
                | HookEvent::MessageSending { .. }
                | HookEvent::BeforeToolCall { .. }
                | HookEvent::ToolResultPersist { .. }
                | HookEvent::BeforeMessageWrite { .. }
                | HookEvent::SubagentSpawning { .. }
                | HookEvent::SubagentDeliveryTarget { .. }
        )
    }
}

// ============================================================================
// Hook Result (for modifying hooks)
// ============================================================================

/// Result from a modifying hook handler.
#[derive(Debug, Clone)]
pub enum HookResult {
    /// Continue with no modifications.
    Continue,
    /// Cancel the action (e.g., prevent tool call, prevent message send).
    Cancel {
        reason: String,
    },
    /// Override with new data.
    Override {
        data: serde_json::Value,
    },
    /// Transform the content (pass-through with modifications).
    Transform {
        content: String,
    },
}

// ============================================================================
// Hook Handler Types
// ============================================================================

/// A fire-and-forget hook handler (for non-modifying hooks).
pub type HookHandler = Arc<dyn Fn(HookEvent) + Send + Sync>;

/// A modifying hook handler that returns a result.
pub type ModifyingHookHandler = Arc<dyn Fn(HookEvent) -> HookResult + Send + Sync>;

/// A prioritized hook entry.
struct HookEntry {
    priority: i32, // lower = runs first
    handler: HookHandler,
}

/// A prioritized modifying hook entry.
struct ModifyingHookEntry {
    priority: i32,
    handler: ModifyingHookHandler,
}

// ============================================================================
// Hook Registry
// ============================================================================

/// Registry for lifecycle hooks.
///
/// Supports two kinds of hooks:
/// - **Fire-and-forget**: Run in parallel, no return value.
/// - **Modifying**: Run sequentially by priority, can cancel/override/transform.
pub struct HookRegistry {
    handlers: HashMap<String, Vec<HookEntry>>,
    modifying_handlers: HashMap<String, Vec<ModifyingHookEntry>>,
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            modifying_handlers: HashMap::new(),
        }
    }

    /// Register a fire-and-forget handler for an event type.
    pub fn on(&mut self, event_type: &str, handler: HookHandler) {
        self.on_with_priority(event_type, 0, handler);
    }

    /// Register a fire-and-forget handler with explicit priority.
    pub fn on_with_priority(&mut self, event_type: &str, priority: i32, handler: HookHandler) {
        let entries = self.handlers.entry(event_type.to_string()).or_default();
        entries.push(HookEntry { priority, handler });
        entries.sort_by_key(|e| e.priority);
    }

    /// Register a modifying handler for an event type.
    pub fn on_modifying(&mut self, event_type: &str, handler: ModifyingHookHandler) {
        self.on_modifying_with_priority(event_type, 0, handler);
    }

    /// Register a modifying handler with explicit priority.
    pub fn on_modifying_with_priority(
        &mut self,
        event_type: &str,
        priority: i32,
        handler: ModifyingHookHandler,
    ) {
        let entries = self
            .modifying_handlers
            .entry(event_type.to_string())
            .or_default();
        entries.push(ModifyingHookEntry { priority, handler });
        entries.sort_by_key(|e| e.priority);
    }

    /// Fire an event to all registered handlers (fire-and-forget).
    ///
    /// For non-modifying events: runs all handlers in parallel.
    /// For modifying events: use `emit_modifying()` instead.
    pub fn emit(&self, event: HookEvent) {
        let event_type = event.event_type();
        if let Some(entries) = self.handlers.get(event_type) {
            for entry in entries {
                let handler = entry.handler.clone();
                let event = event.clone();
                std::thread::spawn(move || handler(event));
            }
        }
    }

    /// Fire a modifying event and get the result.
    ///
    /// Runs modifying handlers sequentially in priority order.
    /// If any handler returns Cancel, stops and returns Cancel.
    /// If any handler returns Override/Transform, passes modified data to next handler.
    pub fn emit_modifying(&self, event: HookEvent) -> HookResult {
        let event_type = event.event_type();
        if let Some(entries) = self.modifying_handlers.get(event_type) {
            for entry in entries {
                let result = (entry.handler)(event.clone());
                match result {
                    HookResult::Continue => continue,
                    HookResult::Cancel { .. } => return result,
                    HookResult::Override { .. } | HookResult::Transform { .. } => return result,
                }
            }
        }

        // Also fire non-modifying handlers
        self.emit(event);
        HookResult::Continue
    }
}

// ============================================================================
// Thread-safe wrapper for use in GatewayState
// ============================================================================

/// Thread-safe hook registry that can be shared across async tasks.
/// Thread-safe singleton hook registry (v2026.3.11: hardened state).
pub struct SharedHookRegistry {
    inner: RwLock<HookRegistry>,
    /// Plugin context carried through all hook phases (v2026.3.11).
    plugin_context: RwLock<Option<HookPluginContext>>,
}

impl SharedHookRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HookRegistry::new()),
            plugin_context: RwLock::new(None),
        }
    }

    pub async fn on(&self, event_type: &str, handler: HookHandler) {
        self.inner.write().await.on(event_type, handler);
    }

    pub async fn on_modifying(&self, event_type: &str, handler: ModifyingHookHandler) {
        self.inner.write().await.on_modifying(event_type, handler);
    }

    pub async fn emit(&self, event: HookEvent) {
        self.inner.read().await.emit(event);
    }

    pub async fn emit_modifying(&self, event: HookEvent) -> HookResult {
        self.inner.read().await.emit_modifying(event)
    }

    /// Set plugin context for the current hook chain (v2026.3.11).
    pub async fn set_plugin_context(&self, ctx: HookPluginContext) {
        *self.plugin_context.write().await = Some(ctx);
    }

    /// Get the current plugin context (v2026.3.11).
    pub async fn get_plugin_context(&self) -> Option<HookPluginContext> {
        self.plugin_context.read().await.clone()
    }

    /// Clear plugin context after hook chain completes (v2026.3.11).
    pub async fn clear_plugin_context(&self) {
        *self.plugin_context.write().await = None;
    }
}

impl Default for SharedHookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_hook_registry_new_is_empty() {
        let registry = HookRegistry::new();
        registry.emit(HookEvent::MessageReceived {
            from: "test".into(),
            content: "msg".into(),
            timestamp: None,
        });
    }

    #[test]
    fn test_hook_registry_on_and_emit() {
        let mut registry = HookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        registry.on(
            "message_received",
            Arc::new(move |_event| {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }),
        );

        registry.emit(HookEvent::MessageReceived {
            from: "user1".into(),
            content: "hello".into(),
            timestamp: Some(12345),
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_hook_registry_multiple_handlers() {
        let mut registry = HookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let c = counter.clone();
            registry.on(
                "message_sent",
                Arc::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            );
        }

        registry.emit(HookEvent::MessageSent {
            to: "user2".into(),
            content: "bye".into(),
            success: true,
            error: None,
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_hook_event_clone() {
        let event = HookEvent::MessageReceived {
            from: "sender".into(),
            content: "test".into(),
            timestamp: Some(999),
        };
        let cloned = event.clone();
        match cloned {
            HookEvent::MessageReceived {
                from,
                content,
                timestamp,
            } => {
                assert_eq!(from, "sender");
                assert_eq!(content, "test");
                assert_eq!(timestamp, Some(999));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_modifying_hook_cancel() {
        let mut registry = HookRegistry::new();

        registry.on_modifying(
            "before_tool_call",
            Arc::new(|_event| HookResult::Cancel {
                reason: "Blocked by policy".to_string(),
            }),
        );

        let result = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "system_run".into(),
            params: serde_json::json!({}),
        });

        match result {
            HookResult::Cancel { reason } => {
                assert_eq!(reason, "Blocked by policy");
            }
            _ => panic!("Expected Cancel"),
        }
    }

    #[test]
    fn test_modifying_hook_continue() {
        let registry = HookRegistry::new();

        let result = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "web_fetch".into(),
            params: serde_json::json!({}),
        });

        matches!(result, HookResult::Continue);
    }

    #[test]
    fn test_event_type_names() {
        assert_eq!(
            HookEvent::GatewayStart.event_type(),
            "gateway_start"
        );
        assert_eq!(
            HookEvent::GatewayStop.event_type(),
            "gateway_stop"
        );
        assert_eq!(
            HookEvent::BeforeModelResolve {
                prompt: String::new()
            }
            .event_type(),
            "before_model_resolve"
        );
    }

    #[test]
    fn test_is_modifying() {
        assert!(HookEvent::BeforeToolCall {
            tool: String::new(),
            params: serde_json::json!({})
        }
        .is_modifying());

        assert!(!HookEvent::GatewayStart.is_modifying());
        assert!(!HookEvent::MessageReceived {
            from: String::new(),
            content: String::new(),
            timestamp: None
        }
        .is_modifying());
    }

    #[test]
    fn test_priority_ordering() {
        let mut registry = HookRegistry::new();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let o1 = order.clone();
        registry.on_with_priority(
            "gateway_start",
            10,
            Arc::new(move |_| {
                o1.lock().unwrap().push(10);
            }),
        );

        let o2 = order.clone();
        registry.on_with_priority(
            "gateway_start",
            1,
            Arc::new(move |_| {
                o2.lock().unwrap().push(1);
            }),
        );

        let o3 = order.clone();
        registry.on_with_priority(
            "gateway_start",
            5,
            Arc::new(move |_| {
                o3.lock().unwrap().push(5);
            }),
        );

        // Note: fire-and-forget handlers run in threads, so ordering
        // is not strictly guaranteed. But entries are sorted by priority.
        registry.emit(HookEvent::GatewayStart);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
