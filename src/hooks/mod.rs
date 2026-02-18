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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_hook_registry_new_is_empty() {
        let registry = HookRegistry::new();
        // Emitting on an unknown event type should be a no-op
        registry.emit(
            "nonexistent",
            HookEvent::MessageReceived {
                from: "test".into(),
                content: "msg".into(),
                timestamp: None,
            },
        );
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

        registry.emit(
            "message_received",
            HookEvent::MessageReceived {
                from: "user1".into(),
                content: "hello".into(),
                timestamp: Some(12345),
            },
        );

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

        registry.emit(
            "message_sent",
            HookEvent::MessageSent {
                to: "user2".into(),
                content: "bye".into(),
                success: true,
                error: None,
            },
        );

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
}
