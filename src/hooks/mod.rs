use std::collections::HashMap;
use std::sync::Arc;

/// Events fired during the message lifecycle.
#[derive(Debug, Clone)]
pub enum HookEvent {
    MessageReceived {
        from: String,
        content: String,
        timestamp: Option<u64>,
    },
    MessageSent {
        to: String,
        content: String,
        success: bool,
        error: Option<String>,
    },
}

/// A fire-and-forget hook handler.
pub type HookHandler = Arc<dyn Fn(HookEvent) + Send + Sync>;

/// Registry for message lifecycle hooks.
#[derive(Default)]
pub struct HookRegistry {
    handlers: HashMap<String, Vec<HookHandler>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler for an event type.
    pub fn on(&mut self, event_type: &str, handler: HookHandler) {
        self.handlers
            .entry(event_type.to_string())
            .or_default()
            .push(handler);
    }

    /// Fire an event to all registered handlers (fire-and-forget).
    pub fn emit(&self, event_type: &str, event: HookEvent) {
        if let Some(handlers) = self.handlers.get(event_type) {
            for handler in handlers {
                let handler = handler.clone();
                let event = event.clone();
                // Fire-and-forget: spawn a blocking task to avoid blocking the async runtime
                std::thread::spawn(move || handler(event));
            }
        }
    }
}
