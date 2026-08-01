//! Descriptor-backed gateway method registry (v2026.7.1 parity).
//!
//! Plugin-owned RPC methods are registered with scope metadata and a
//! visibility flag. Registration performs hidden-core collision checks so a
//! plugin can never shadow a built-in method (including internal/hidden core
//! methods that are not advertised).

use std::collections::HashMap;

/// Core (built-in) gateway RPC method names. Plugin registrations that
/// collide with any of these are rejected. Kept sorted for readability;
/// lookup is via `is_core_method`.
pub const CORE_METHODS: &[&str] = &[
    "acp.list", "acp.send", "acp.spawn", "acp.stop",
    "agent", "agent.identity.get", "agent.wait",
    "agents.bind", "agents.bindings", "agents.create", "agents.delete",
    "agents.files.get", "agents.files.list", "agents.files.set", "agents.list",
    "agents.unbind", "agents.update", "agents.workspace.get", "agents.workspace.list",
    "artifacts.download", "artifacts.get", "artifacts.list",
    "browser.request",
    "channels.logout", "channels.status", "channels.stop",
    "chat.abort", "chat.cancel", "chat.history", "chat.send",
    "config.apply", "config.get", "config.patch", "config.reload",
    "config.schema", "config.set", "config.validate",
    "connect",
    "cron.add", "cron.get", "cron.list", "cron.remove", "cron.run",
    "cron.runs", "cron.status", "cron.update",
    "device.info", "device.pair.approve", "device.pair.list",
    "device.pair.reject", "device.pair.remove", "device.status",
    "device.token.revoke", "device.token.rotate",
    "doctor.memory.remHarness", "doctor.memory.status",
    "exec.approval.request", "exec.approval.resolve", "exec.approval.waitDecision",
    "exec.approvals.get", "exec.approvals.node.get", "exec.approvals.node.set",
    "exec.approvals.set",
    "gateway.info", "gateway.restart",
    "health",
    "last-heartbeat", "logs.tail",
    "memory.search", "models.list",
    "node.canvas.capability.refresh", "node.describe", "node.event",
    "node.invoke", "node.invoke.result", "node.list",
    "node.pair.approve", "node.pair.list", "node.pair.reject",
    "node.pair.request", "node.pair.verify", "node.pending.drain",
    "node.pending.enqueue", "node.presence.alive", "node.rename",
    "notifications.list",
    "presence.set", "push.test",
    "secrets.reload", "secrets.resolve",
    "send", "sessions.archive", "sessions.cleanup", "sessions.compact",
    "sessions.delete", "sessions.describe", "sessions.fork", "sessions.get",
    "sessions.groups", "sessions.list", "sessions.patch", "sessions.preview",
    "sessions.rename", "sessions.reset", "sessions.resolve", "sessions.unread",
    "sessions.usage",
    "set-heartbeats", "skills.bins", "skills.install", "skills.status",
    "skills.update", "startup.timeline", "status", "system-event",
    "system-presence", "system.info",
    "talk.config", "talk.mode", "talk.session.start", "talk.session.status",
    "talk.session.stop", "talk.wake",
    "tasks.cancel", "tasks.get", "tasks.list",
    "terminal.detach", "terminal.list", "terminal.reattach", "terminal.text",
    "tools.catalog", "tools.effective", "tools.invoke", "tools.list",
    "tts.convert", "tts.disable", "tts.enable", "tts.providers",
    "tts.setProvider", "tts.speak", "tts.status",
    "update.run", "usage.cost", "usage.status",
    "voicewake.get", "voicewake.set",
    "wake", "web.login.start", "web.login.wait",
    "wizard.cancel", "wizard.next", "wizard.start", "wizard.status",
];

pub fn is_core_method(name: &str) -> bool {
    CORE_METHODS.contains(&name)
}

/// Visibility of a registered method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodVisibility {
    /// Advertised in method listings.
    Advertised,
    /// Callable but not advertised (internal handlers).
    Internal,
}

/// Descriptor for a plugin-owned RPC method.
#[derive(Debug, Clone)]
pub struct MethodDescriptor {
    pub name: String,
    /// Owning plugin id.
    pub plugin_id: String,
    /// Required scope (e.g. "operator.read", "operator.write",
    /// "operator.admin"). `None` = any authenticated connection.
    pub required_scope: Option<String>,
    pub visibility: MethodVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// Collides with a built-in method (hidden-core collisions included).
    CoreCollision(String),
    /// Already registered by another plugin.
    DuplicateRegistration { method: String, owner: String },
    /// Invalid method name.
    InvalidName(String),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::CoreCollision(m) => {
                write!(f, "method '{m}' collides with a core gateway method")
            }
            RegisterError::DuplicateRegistration { method, owner } => {
                write!(f, "method '{method}' already registered by plugin '{owner}'")
            }
            RegisterError::InvalidName(m) => write!(f, "invalid method name '{m}'"),
        }
    }
}

/// Validate a plugin method name: dotted lowercase segments.
pub fn valid_method_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_')
}

/// Descriptor-backed registry for plugin-owned methods.
#[derive(Default)]
pub struct MethodRegistry {
    methods: parking_lot::RwLock<HashMap<String, MethodDescriptor>>,
}

impl MethodRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, descriptor: MethodDescriptor) -> Result<(), RegisterError> {
        if !valid_method_name(&descriptor.name) {
            return Err(RegisterError::InvalidName(descriptor.name));
        }
        if is_core_method(&descriptor.name) {
            return Err(RegisterError::CoreCollision(descriptor.name));
        }
        let mut methods = self.methods.write();
        if let Some(existing) = methods.get(&descriptor.name) {
            if existing.plugin_id != descriptor.plugin_id {
                return Err(RegisterError::DuplicateRegistration {
                    method: descriptor.name,
                    owner: existing.plugin_id.clone(),
                });
            }
        }
        methods.insert(descriptor.name.clone(), descriptor);
        Ok(())
    }

    /// Remove all methods owned by a plugin (on unload/disable).
    pub fn unregister_plugin(&self, plugin_id: &str) -> usize {
        let mut methods = self.methods.write();
        let before = methods.len();
        methods.retain(|_, d| d.plugin_id != plugin_id);
        before - methods.len()
    }

    pub fn resolve(&self, name: &str) -> Option<MethodDescriptor> {
        self.methods.read().get(name).cloned()
    }

    /// Only advertised methods appear in listings.
    pub fn advertised_methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .methods
            .read()
            .values()
            .filter(|d| d.visibility == MethodVisibility::Advertised)
            .map(|d| d.name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.methods.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Check whether connection scopes satisfy a descriptor's required scope.
/// Scope strings follow the `operator.*` hierarchy where `operator.admin`
/// implies everything.
pub fn scopes_satisfy(descriptor: &MethodDescriptor, connection_scopes: &[String]) -> bool {
    match &descriptor.required_scope {
        None => true,
        Some(required) => {
            connection_scopes.iter().any(|s| s == required)
                || connection_scopes.iter().any(|s| s == "operator.admin")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(name: &str, plugin: &str, vis: MethodVisibility) -> MethodDescriptor {
        MethodDescriptor {
            name: name.to_string(),
            plugin_id: plugin.to_string(),
            required_scope: None,
            visibility: vis,
        }
    }

    #[test]
    fn core_collision_rejected_including_hidden() {
        let reg = MethodRegistry::new();
        // Advertised core method
        assert_eq!(
            reg.register(desc("chat.send", "p1", MethodVisibility::Advertised)),
            Err(RegisterError::CoreCollision("chat.send".to_string()))
        );
        // Hidden/internal core methods are also protected
        assert_eq!(
            reg.register(desc("system-event", "p1", MethodVisibility::Advertised)),
            Err(RegisterError::CoreCollision("system-event".to_string()))
        );
    }

    #[test]
    fn duplicate_registration_by_other_plugin_rejected() {
        let reg = MethodRegistry::new();
        reg.register(desc("myplugin.run", "p1", MethodVisibility::Advertised))
            .unwrap();
        let err = reg
            .register(desc("myplugin.run", "p2", MethodVisibility::Advertised))
            .unwrap_err();
        assert!(matches!(err, RegisterError::DuplicateRegistration { .. }));
        // Same plugin may re-register (update)
        assert!(reg
            .register(desc("myplugin.run", "p1", MethodVisibility::Internal))
            .is_ok());
    }

    #[test]
    fn invalid_names_rejected() {
        for bad in ["", ".x", "x.", "a..b", "Upper.Case", "with space", "emoji💥"] {
            assert!(!valid_method_name(bad), "{bad}");
        }
        for good in ["myplugin.action", "a.b.c", "kebab-case.ok", "under_score.ok1"] {
            assert!(valid_method_name(good), "{good}");
        }
    }

    #[test]
    fn advertised_vs_internal_listing() {
        let reg = MethodRegistry::new();
        reg.register(desc("p.pub", "p1", MethodVisibility::Advertised))
            .unwrap();
        reg.register(desc("p.hidden", "p1", MethodVisibility::Internal))
            .unwrap();
        assert_eq!(reg.advertised_methods(), vec!["p.pub".to_string()]);
        // Internal methods still resolve for dispatch
        assert!(reg.resolve("p.hidden").is_some());
    }

    #[test]
    fn unregister_plugin_removes_all() {
        let reg = MethodRegistry::new();
        reg.register(desc("p.a", "p1", MethodVisibility::Advertised))
            .unwrap();
        reg.register(desc("p.b", "p1", MethodVisibility::Internal))
            .unwrap();
        reg.register(desc("q.a", "p2", MethodVisibility::Advertised))
            .unwrap();
        assert_eq!(reg.unregister_plugin("p1"), 2);
        assert_eq!(reg.len(), 1);
        assert!(reg.resolve("q.a").is_some());
    }

    #[test]
    fn scope_metadata_enforced() {
        let mut d = desc("p.secure", "p1", MethodVisibility::Advertised);
        d.required_scope = Some("operator.write".to_string());
        assert!(!scopes_satisfy(&d, &["operator.read".to_string()]));
        assert!(scopes_satisfy(&d, &["operator.write".to_string()]));
        // admin implies all
        assert!(scopes_satisfy(&d, &["operator.admin".to_string()]));
        // no required scope → always satisfied
        let open = desc("p.open", "p1", MethodVisibility::Advertised);
        assert!(scopes_satisfy(&open, &[]));
    }
}
