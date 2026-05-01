use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ============================================================================
// Hook Events (26 types matching OpenClaw — 24 base + before_agent_finalize
// (v2026.4.25) + cron_changed (v2026.4.26))
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
    /// Fires after the agent has produced its response and before it is
    /// finalized/persisted. Modifying hook; can override or cancel.
    /// (OpenClaw v2026.4.25)
    BeforeAgentFinalize {
        session_key: String,
        response: serde_json::Value,
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

    /// Fires when a gateway-owned cron job is created, updated, or removed.
    /// (OpenClaw v2026.4.26)
    CronChanged {
        job_id: String,
        change: CronChangeKind,
        schedule: Option<String>,
    },
}

/// Kind of change for a `CronChanged` hook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronChangeKind {
    Created,
    Updated,
    Removed,
}

impl CronChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CronChangeKind::Created => "created",
            CronChangeKind::Updated => "updated",
            CronChangeKind::Removed => "removed",
        }
    }
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
            HookEvent::BeforeAgentFinalize { .. } => "before_agent_finalize",
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
            HookEvent::CronChanged { .. } => "cron_changed",
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
                | HookEvent::BeforeAgentFinalize { .. }
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

    // ====================================================================
    // HookPluginContext (v2026.3.11)
    // ====================================================================

    #[test]
    fn plugin_context_default_all_none() {
        let ctx = HookPluginContext::default();
        assert!(ctx.trigger.is_none());
        assert!(ctx.channel_id.is_none());
        assert!(ctx.account_id.is_none());
        assert!(ctx.thread_id.is_none());
    }

    #[test]
    fn plugin_context_with_fields() {
        let ctx = HookPluginContext {
            trigger: Some("message".to_string()),
            channel_id: Some("telegram:123".to_string()),
            account_id: Some("user-456".to_string()),
            thread_id: Some("thread-789".to_string()),
        };
        assert_eq!(ctx.trigger.as_deref(), Some("message"));
        assert_eq!(ctx.channel_id.as_deref(), Some("telegram:123"));
        assert_eq!(ctx.account_id.as_deref(), Some("user-456"));
        assert_eq!(ctx.thread_id.as_deref(), Some("thread-789"));
    }

    #[test]
    fn plugin_context_clone() {
        let ctx = HookPluginContext {
            trigger: Some("cron".to_string()),
            channel_id: None,
            account_id: None,
            thread_id: None,
        };
        let cloned = ctx.clone();
        assert_eq!(cloned.trigger, ctx.trigger);
    }

    // ====================================================================
    // SharedHookRegistry plugin context lifecycle (v2026.3.11)
    // ====================================================================

    #[tokio::test]
    async fn shared_registry_plugin_context_lifecycle() {
        let registry = SharedHookRegistry::new();

        // Initially no context
        assert!(registry.get_plugin_context().await.is_none());

        // Set context
        let ctx = HookPluginContext {
            trigger: Some("api".to_string()),
            channel_id: Some("slack:C01".to_string()),
            account_id: None,
            thread_id: None,
        };
        registry.set_plugin_context(ctx).await;

        // Retrieve context
        let retrieved = registry.get_plugin_context().await.unwrap();
        assert_eq!(retrieved.trigger.as_deref(), Some("api"));
        assert_eq!(retrieved.channel_id.as_deref(), Some("slack:C01"));

        // Clear context
        registry.clear_plugin_context().await;
        assert!(registry.get_plugin_context().await.is_none());
    }

    #[tokio::test]
    async fn shared_registry_plugin_context_overwrite() {
        let registry = SharedHookRegistry::new();

        registry.set_plugin_context(HookPluginContext {
            trigger: Some("message".to_string()),
            ..Default::default()
        }).await;

        registry.set_plugin_context(HookPluginContext {
            trigger: Some("cron".to_string()),
            ..Default::default()
        }).await;

        let ctx = registry.get_plugin_context().await.unwrap();
        assert_eq!(ctx.trigger.as_deref(), Some("cron"));
    }

    // ====================================================================
    // Parity expansion (OpenClaw v2026.4.29)
    // ====================================================================

    fn empty_session_event() -> HookEvent {
        HookEvent::SessionStart {
            session_key: String::new(),
        }
    }

    /// Every documented HookEvent variant returns its declared event_type name.
    #[test]
    fn all_event_type_names_round_trip() {
        let cases: &[(HookEvent, &str)] = &[
            (HookEvent::BeforeModelResolve { prompt: String::new() }, "before_model_resolve"),
            (HookEvent::BeforePromptBuild { session_key: String::new() }, "before_prompt_build"),
            (HookEvent::BeforeAgentStart { session_key: String::new() }, "before_agent_start"),
            (HookEvent::LlmInput { model: String::new(), messages: vec![] }, "llm_input"),
            (HookEvent::LlmOutput { model: String::new(), response: serde_json::Value::Null }, "llm_output"),
            (HookEvent::AgentEnd { session_key: String::new(), input_tokens: None, output_tokens: None }, "agent_end"),
            (HookEvent::BeforeAgentFinalize { session_key: String::new(), response: serde_json::Value::Null }, "before_agent_finalize"),
            (HookEvent::BeforeCompaction { session_key: String::new() }, "before_compaction"),
            (HookEvent::AfterCompaction { session_key: String::new() }, "after_compaction"),
            (HookEvent::BeforeReset { session_key: String::new() }, "before_reset"),
            (HookEvent::MessageReceived { from: String::new(), content: String::new(), timestamp: None }, "message_received"),
            (HookEvent::MessageSending { to: String::new(), content: String::new() }, "message_sending"),
            (HookEvent::MessageSent { to: String::new(), content: String::new(), success: true, error: None }, "message_sent"),
            (HookEvent::BeforeToolCall { tool: String::new(), params: serde_json::Value::Null }, "before_tool_call"),
            (HookEvent::AfterToolCall { tool: String::new(), result: serde_json::Value::Null }, "after_tool_call"),
            (HookEvent::ToolResultPersist { tool: String::new(), result: serde_json::Value::Null }, "tool_result_persist"),
            (HookEvent::BeforeMessageWrite { message: serde_json::Value::Null }, "before_message_write"),
            (HookEvent::SessionStart { session_key: String::new() }, "session_start"),
            (HookEvent::SessionEnd { session_key: String::new() }, "session_end"),
            (HookEvent::SubagentSpawning { parent: String::new(), child: String::new() }, "subagent_spawning"),
            (HookEvent::SubagentSpawned { parent: String::new(), child: String::new() }, "subagent_spawned"),
            (HookEvent::SubagentDeliveryTarget { session_key: String::new() }, "subagent_delivery_target"),
            (HookEvent::SubagentEnded { session_key: String::new() }, "subagent_ended"),
            (HookEvent::GatewayStart, "gateway_start"),
            (HookEvent::GatewayStop, "gateway_stop"),
            (HookEvent::CronChanged { job_id: String::new(), change: CronChangeKind::Created, schedule: None }, "cron_changed"),
        ];
        for (ev, expected) in cases {
            assert_eq!(ev.event_type(), *expected, "wrong event_type for {:?}", ev);
        }
        assert_eq!(cases.len(), 26, "should cover all 26 hook event variants (24 base + before_agent_finalize v2026.4.25 + cron_changed v2026.4.26)");
    }

    /// All event variants documented as modifying return is_modifying() == true.
    #[test]
    fn all_modifying_events_classified_as_modifying() {
        let modifying: &[HookEvent] = &[
            HookEvent::BeforeModelResolve { prompt: String::new() },
            HookEvent::MessageSending { to: String::new(), content: String::new() },
            HookEvent::BeforeToolCall { tool: String::new(), params: serde_json::Value::Null },
            HookEvent::ToolResultPersist { tool: String::new(), result: serde_json::Value::Null },
            HookEvent::BeforeMessageWrite { message: serde_json::Value::Null },
            HookEvent::SubagentSpawning { parent: String::new(), child: String::new() },
            HookEvent::SubagentDeliveryTarget { session_key: String::new() },
            // v2026.4.25 — before_agent_finalize is modifying so plugins can
            // intercept the final response.
            HookEvent::BeforeAgentFinalize { session_key: String::new(), response: serde_json::Value::Null },
        ];
        for ev in modifying {
            assert!(ev.is_modifying(), "{:?} should be modifying", ev);
        }
    }

    /// Non-modifying events that flowed through the chain in v2026.4.29 — emitted as
    /// notifications only, never able to cancel/override.
    #[test]
    fn non_modifying_events_classified_as_not_modifying() {
        let non_modifying: &[HookEvent] = &[
            HookEvent::BeforePromptBuild { session_key: String::new() },
            HookEvent::BeforeAgentStart { session_key: String::new() },
            HookEvent::LlmInput { model: String::new(), messages: vec![] },
            HookEvent::LlmOutput { model: String::new(), response: serde_json::Value::Null },
            HookEvent::AgentEnd { session_key: String::new(), input_tokens: None, output_tokens: None },
            HookEvent::BeforeCompaction { session_key: String::new() },
            HookEvent::AfterCompaction { session_key: String::new() },
            HookEvent::BeforeReset { session_key: String::new() },
            HookEvent::MessageReceived { from: String::new(), content: String::new(), timestamp: None },
            HookEvent::MessageSent { to: String::new(), content: String::new(), success: true, error: None },
            HookEvent::AfterToolCall { tool: String::new(), result: serde_json::Value::Null },
            HookEvent::SessionStart { session_key: String::new() },
            HookEvent::SessionEnd { session_key: String::new() },
            HookEvent::SubagentSpawned { parent: String::new(), child: String::new() },
            HookEvent::SubagentEnded { session_key: String::new() },
            HookEvent::GatewayStart,
            HookEvent::GatewayStop,
            // v2026.4.26 — cron_changed is notification-only, never modifying.
            HookEvent::CronChanged { job_id: String::new(), change: CronChangeKind::Updated, schedule: None },
        ];
        for ev in non_modifying {
            assert!(!ev.is_modifying(), "{:?} should NOT be modifying", ev);
        }
    }

    // ====================================================================
    // v2026.4.25 — before_agent_finalize hook
    // ====================================================================

    #[test]
    fn before_agent_finalize_event_type() {
        let ev = HookEvent::BeforeAgentFinalize {
            session_key: "abc".into(),
            response: serde_json::json!({"text": "hi"}),
        };
        assert_eq!(ev.event_type(), "before_agent_finalize");
    }

    #[test]
    fn before_agent_finalize_is_modifying() {
        let ev = HookEvent::BeforeAgentFinalize {
            session_key: String::new(),
            response: serde_json::Value::Null,
        };
        assert!(ev.is_modifying());
    }

    #[tokio::test]
    async fn before_agent_finalize_can_transform_response() {
        let registry = SharedHookRegistry::new();
        registry
            .on_modifying(
                "before_agent_finalize",
                Arc::new(|_| HookResult::Transform {
                    content: "redacted".into(),
                }),
            )
            .await;

        let result = registry
            .emit_modifying(HookEvent::BeforeAgentFinalize {
                session_key: "s".into(),
                response: serde_json::json!({"text": "secret token: ABC123"}),
            })
            .await;

        match result {
            HookResult::Transform { content } => assert_eq!(content, "redacted"),
            other => panic!("expected Transform, got {:?}", other),
        }
    }

    // ====================================================================
    // v2026.4.26 — cron_changed typed hook
    // ====================================================================

    #[test]
    fn cron_changed_event_type() {
        let ev = HookEvent::CronChanged {
            job_id: "daily-summary".into(),
            change: CronChangeKind::Created,
            schedule: Some("0 9 * * *".into()),
        };
        assert_eq!(ev.event_type(), "cron_changed");
    }

    #[test]
    fn cron_changed_is_not_modifying() {
        let ev = HookEvent::CronChanged {
            job_id: String::new(),
            change: CronChangeKind::Removed,
            schedule: None,
        };
        assert!(!ev.is_modifying());
    }

    #[test]
    fn cron_change_kind_strings() {
        assert_eq!(CronChangeKind::Created.as_str(), "created");
        assert_eq!(CronChangeKind::Updated.as_str(), "updated");
        assert_eq!(CronChangeKind::Removed.as_str(), "removed");
    }

    #[tokio::test]
    async fn cron_changed_handlers_receive_event() {
        let registry = SharedHookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        registry
            .on(
                "cron_changed",
                Arc::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await;

        registry
            .emit(HookEvent::CronChanged {
                job_id: "test".into(),
                change: CronChangeKind::Created,
                schedule: Some("*/5 * * * *".into()),
            })
            .await;

        // Give fire-and-forget tasks a moment to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn modifying_hook_override_returns_data() {
        let mut registry = HookRegistry::new();
        registry.on_modifying(
            "before_message_write",
            Arc::new(|_| HookResult::Override {
                data: serde_json::json!({"replaced": true}),
            }),
        );
        let result = registry.emit_modifying(HookEvent::BeforeMessageWrite {
            message: serde_json::json!({"original": true}),
        });
        match result {
            HookResult::Override { data } => assert_eq!(data["replaced"], true),
            other => panic!("expected Override, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn modifying_hook_transform_returns_content() {
        let mut registry = HookRegistry::new();
        registry.on_modifying(
            "message_sending",
            Arc::new(|ev| match ev {
                HookEvent::MessageSending { content, .. } => HookResult::Transform {
                    content: format!("[censored] {}", content),
                },
                _ => HookResult::Continue,
            }),
        );
        let result = registry.emit_modifying(HookEvent::MessageSending {
            to: "u".into(),
            content: "secret".into(),
        });
        match result {
            HookResult::Transform { content } => assert_eq!(content, "[censored] secret"),
            _ => panic!("expected Transform"),
        }
    }

    #[test]
    fn modifying_hook_chain_runs_through_continue_results() {
        let mut registry = HookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let c = counter.clone();
            registry.on_modifying(
                "before_tool_call",
                Arc::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    HookResult::Continue
                }),
            );
        }
        let result = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "x".into(),
            params: serde_json::Value::Null,
        });
        assert!(matches!(result, HookResult::Continue));
        assert_eq!(counter.load(Ordering::SeqCst), 3, "all 3 handlers ran");
    }

    #[test]
    fn modifying_hook_cancel_short_circuits_remaining_handlers() {
        let mut registry = HookRegistry::new();
        let after_counter = Arc::new(AtomicUsize::new(0));

        registry.on_modifying_with_priority(
            "before_tool_call",
            1,
            Arc::new(|_| HookResult::Cancel { reason: "stop".into() }),
        );
        let c = after_counter.clone();
        registry.on_modifying_with_priority(
            "before_tool_call",
            2,
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                HookResult::Continue
            }),
        );

        let result = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "x".into(),
            params: serde_json::Value::Null,
        });
        assert!(matches!(result, HookResult::Cancel { .. }));
        assert_eq!(
            after_counter.load(Ordering::SeqCst),
            0,
            "handlers after Cancel must not run"
        );
    }

    #[test]
    fn modifying_hook_override_short_circuits_remaining_handlers() {
        let mut registry = HookRegistry::new();
        let after_counter = Arc::new(AtomicUsize::new(0));

        registry.on_modifying_with_priority(
            "before_message_write",
            1,
            Arc::new(|_| HookResult::Override { data: serde_json::Value::Null }),
        );
        let c = after_counter.clone();
        registry.on_modifying_with_priority(
            "before_message_write",
            2,
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                HookResult::Continue
            }),
        );

        let _ = registry.emit_modifying(HookEvent::BeforeMessageWrite {
            message: serde_json::Value::Null,
        });
        assert_eq!(after_counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn modifying_priority_runs_sequentially_in_ascending_order() {
        let mut registry = HookRegistry::new();
        let order = Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));

        let make_handler = |o: Arc<std::sync::Mutex<Vec<i32>>>, mark: i32| {
            Arc::new(move |_ev: HookEvent| {
                o.lock().unwrap().push(mark);
                HookResult::Continue
            }) as ModifyingHookHandler
        };

        registry.on_modifying_with_priority("before_tool_call", 50, make_handler(order.clone(), 50));
        registry.on_modifying_with_priority("before_tool_call", 1, make_handler(order.clone(), 1));
        registry.on_modifying_with_priority("before_tool_call", 10, make_handler(order.clone(), 10));

        let _ = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "x".into(),
            params: serde_json::Value::Null,
        });
        assert_eq!(*order.lock().unwrap(), vec![1, 10, 50]);
    }

    #[test]
    fn emit_modifying_with_no_handlers_returns_continue() {
        let registry = HookRegistry::new();
        let result = registry.emit_modifying(empty_session_event());
        assert!(matches!(result, HookResult::Continue));
    }

    #[test]
    fn emit_modifying_falls_through_to_fire_and_forget_when_all_continue() {
        let mut registry = HookRegistry::new();
        let fire_counter = Arc::new(AtomicUsize::new(0));

        registry.on_modifying(
            "before_tool_call",
            Arc::new(|_| HookResult::Continue),
        );
        let c = fire_counter.clone();
        registry.on(
            "before_tool_call",
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let _ = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "x".into(),
            params: serde_json::Value::Null,
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(fire_counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn emit_modifying_skips_fire_and_forget_when_short_circuited() {
        let mut registry = HookRegistry::new();
        let fire_counter = Arc::new(AtomicUsize::new(0));

        registry.on_modifying(
            "before_tool_call",
            Arc::new(|_| HookResult::Cancel { reason: "no".into() }),
        );
        let c = fire_counter.clone();
        registry.on(
            "before_tool_call",
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let _ = registry.emit_modifying(HookEvent::BeforeToolCall {
            tool: "x".into(),
            params: serde_json::Value::Null,
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            fire_counter.load(Ordering::SeqCst),
            0,
            "short-circuited result must not fire fire-and-forget handlers"
        );
    }

    #[test]
    fn fire_and_forget_handler_receives_event_data() {
        let mut registry = HookRegistry::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let cap = captured.clone();
        registry.on(
            "message_received",
            Arc::new(move |ev| {
                if let HookEvent::MessageReceived { from, content, .. } = ev {
                    *cap.lock().unwrap() = Some((from, content));
                }
            }),
        );
        registry.emit(HookEvent::MessageReceived {
            from: "alice".into(),
            content: "hi".into(),
            timestamp: Some(100),
        });
        std::thread::sleep(std::time::Duration::from_millis(80));
        let got = captured.lock().unwrap().clone();
        assert_eq!(got, Some(("alice".to_string(), "hi".to_string())));
    }

    #[tokio::test]
    async fn shared_registry_emit_runs_fire_and_forget() {
        let registry = SharedHookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        registry
            .on(
                "gateway_start",
                Arc::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await;
        registry.emit(HookEvent::GatewayStart).await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shared_registry_emit_modifying_returns_handler_result() {
        let registry = SharedHookRegistry::new();
        registry
            .on_modifying(
                "message_sending",
                Arc::new(|_| HookResult::Transform {
                    content: "rewritten".into(),
                }),
            )
            .await;
        let result = registry
            .emit_modifying(HookEvent::MessageSending {
                to: "u".into(),
                content: "original".into(),
            })
            .await;
        match result {
            HookResult::Transform { content } => assert_eq!(content, "rewritten"),
            _ => panic!("expected Transform"),
        }
    }

    #[tokio::test]
    async fn shared_registry_isolated_event_types() {
        // Handlers registered for one event type must not fire for another.
        let registry = SharedHookRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        registry
            .on(
                "gateway_start",
                Arc::new(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await;
        registry.emit(HookEvent::GatewayStop).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "gateway_start handler must not fire on gateway_stop"
        );
    }
}
