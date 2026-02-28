//! Agent routing and binding resolution (v2026.2.26).
//!
//! Resolves which agent should handle a given session based on configured
//! bindings. Bindings match on channel, account_id, peer, and thread
//! patterns. Account-scoped route management prevents cross-account
//! agent hijacking.
//!
//! Ported from OpenClaw `src/routing/`.

use crate::config::{AgentBinding, AgentBindingMatch};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

// ============================================================================
// Types
// ============================================================================

/// Context for resolving an agent binding.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    /// Channel the message came from (e.g., "telegram", "discord").
    pub channel: Option<String>,
    /// Account ID within the channel.
    pub account_id: Option<String>,
    /// Peer address (sender).
    pub peer: Option<String>,
    /// Thread/topic ID.
    pub thread_id: Option<String>,
    /// Session key.
    pub session_key: Option<String>,
}

/// Result of agent resolution.
#[derive(Debug, Clone)]
pub struct RoutingResult {
    /// The agent ID to use.
    pub agent_id: String,
    /// The binding that matched, if any.
    pub matched_binding: Option<AgentBinding>,
    /// Whether this is the default agent (no binding matched).
    pub is_default: bool,
}

/// An account-scoped route entry for management.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteEntry {
    pub agent_id: String,
    pub binding: AgentBindingMatch,
    pub account_id: Option<String>,
    pub created_at: u64,
}

// ============================================================================
// Core Functions
// ============================================================================

/// Resolve which agent should handle a session based on bindings.
///
/// Bindings are evaluated in order. The first match wins.
/// If no binding matches, returns the default agent ID.
pub fn resolve_agent_for_session(
    bindings: &[AgentBinding],
    context: &RoutingContext,
    default_agent_id: &str,
) -> RoutingResult {
    for binding in bindings {
        if matches_binding(&binding.match_rule, context) {
            debug!(
                "Routing matched binding for agent '{}' (channel={:?}, peer={:?})",
                binding.agent_id, context.channel, context.peer
            );
            return RoutingResult {
                agent_id: binding.agent_id.clone(),
                matched_binding: Some(binding.clone()),
                is_default: false,
            };
        }
    }

    RoutingResult {
        agent_id: default_agent_id.to_string(),
        matched_binding: None,
        is_default: true,
    }
}

/// Check if a binding rule matches a routing context.
fn matches_binding(rule: &AgentBindingMatch, ctx: &RoutingContext) -> bool {
    // Channel must match if specified.
    if let Some(ref channel) = rule.channel {
        match &ctx.channel {
            Some(ctx_ch) if ctx_ch == channel => {}
            _ => return false,
        }
    }

    // Account ID must match if specified.
    if let Some(ref account_id) = rule.account_id {
        match &ctx.account_id {
            Some(ctx_acct) if ctx_acct == account_id => {}
            _ => return false,
        }
    }

    // Peer must match if specified (supports wildcard).
    if let Some(ref peer) = rule.peer {
        match &ctx.peer {
            Some(ctx_peer) => {
                if peer != "*" && ctx_peer != peer {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Guild ID must match if specified (Discord).
    if let Some(ref guild) = rule.guild_id {
        match &ctx.thread_id {
            Some(ctx_thread) if ctx_thread == guild => {}
            _ => return false,
        }
    }

    // Team ID must match if specified (Slack).
    if let Some(ref team) = rule.team_id {
        match &ctx.thread_id {
            Some(ctx_thread) if ctx_thread == team => {}
            _ => return false,
        }
    }

    true
}

// ============================================================================
// Route Manager
// ============================================================================

/// Manages dynamic agent route bindings at runtime.
pub struct RouteManager {
    routes: Arc<RwLock<Vec<RouteEntry>>>,
}

impl RouteManager {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// List all routes, optionally filtered by account ID.
    pub async fn list(&self, account_id: Option<&str>) -> Vec<RouteEntry> {
        let routes = self.routes.read().await;
        match account_id {
            Some(acct) => routes
                .iter()
                .filter(|r| r.account_id.as_deref() == Some(acct))
                .cloned()
                .collect(),
            None => routes.clone(),
        }
    }

    /// Add a route binding (account-scoped).
    pub async fn bind(&self, entry: RouteEntry) {
        let mut routes = self.routes.write().await;
        routes.push(entry);
    }

    /// Remove a route binding by agent ID (account-scoped).
    ///
    /// Only removes bindings owned by the specified account.
    pub async fn unbind(
        &self,
        agent_id: &str,
        account_id: Option<&str>,
    ) -> bool {
        let mut routes = self.routes.write().await;
        let before = routes.len();
        routes.retain(|r| {
            if r.agent_id != agent_id {
                return true;
            }
            // Only remove if the account matches.
            match (account_id, &r.account_id) {
                (Some(acct), Some(route_acct)) => acct != route_acct,
                (None, _) => false, // No account filter — remove all.
                _ => true,
            }
        });
        routes.len() < before
    }

    /// Convert dynamic routes to AgentBindings for resolution.
    pub async fn to_bindings(&self) -> Vec<AgentBinding> {
        let routes = self.routes.read().await;
        routes
            .iter()
            .map(|r| AgentBinding {
                agent_id: r.agent_id.clone(),
                match_rule: r.binding.clone(),
            })
            .collect()
    }
}

impl Default for RouteManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_binding(agent_id: &str, channel: Option<&str>, peer: Option<&str>) -> AgentBinding {
        AgentBinding {
            agent_id: agent_id.to_string(),
            match_rule: AgentBindingMatch {
                channel: channel.map(String::from),
                account_id: None,
                peer: peer.map(String::from),
                guild_id: None,
                team_id: None,
            },
        }
    }

    // ====================================================================
    // resolve_agent_for_session
    // ====================================================================

    #[test]
    fn no_bindings_returns_default() {
        let result = resolve_agent_for_session(
            &[],
            &RoutingContext::default(),
            "default-agent",
        );
        assert_eq!(result.agent_id, "default-agent");
        assert!(result.is_default);
        assert!(result.matched_binding.is_none());
    }

    #[test]
    fn channel_match() {
        let bindings = vec![
            make_binding("telegram-agent", Some("telegram"), None),
            make_binding("discord-agent", Some("discord"), None),
        ];

        let ctx = RoutingContext {
            channel: Some("telegram".into()),
            ..Default::default()
        };

        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "telegram-agent");
        assert!(!result.is_default);
    }

    #[test]
    fn peer_match() {
        let bindings = vec![make_binding("vip-agent", None, Some("user123"))];

        let ctx = RoutingContext {
            peer: Some("user123".into()),
            ..Default::default()
        };

        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "vip-agent");
    }

    #[test]
    fn peer_wildcard() {
        let bindings = vec![make_binding("catch-all", None, Some("*"))];

        let ctx = RoutingContext {
            peer: Some("anyone".into()),
            ..Default::default()
        };

        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "catch-all");
    }

    #[test]
    fn no_peer_fails_peer_match() {
        let bindings = vec![make_binding("peer-agent", None, Some("user1"))];

        let ctx = RoutingContext::default(); // No peer

        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "default");
    }

    #[test]
    fn first_match_wins() {
        let bindings = vec![
            make_binding("first", Some("telegram"), None),
            make_binding("second", Some("telegram"), None),
        ];

        let ctx = RoutingContext {
            channel: Some("telegram".into()),
            ..Default::default()
        };

        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "first");
    }

    #[test]
    fn combined_channel_and_peer() {
        let bindings = vec![AgentBinding {
            agent_id: "specific".into(),
            match_rule: AgentBindingMatch {
                channel: Some("telegram".into()),
                account_id: None,
                peer: Some("vip@user".into()),
                guild_id: None,
                team_id: None,
            },
        }];

        // Both match
        let ctx = RoutingContext {
            channel: Some("telegram".into()),
            peer: Some("vip@user".into()),
            ..Default::default()
        };
        let result = resolve_agent_for_session(&bindings, &ctx, "default");
        assert_eq!(result.agent_id, "specific");

        // Channel matches but peer doesn't
        let ctx2 = RoutingContext {
            channel: Some("telegram".into()),
            peer: Some("other@user".into()),
            ..Default::default()
        };
        let result2 = resolve_agent_for_session(&bindings, &ctx2, "default");
        assert_eq!(result2.agent_id, "default");
    }

    // ====================================================================
    // RouteManager
    // ====================================================================

    #[tokio::test]
    async fn route_manager_bind_and_list() {
        let mgr = RouteManager::new();
        mgr.bind(RouteEntry {
            agent_id: "agent-1".into(),
            binding: AgentBindingMatch::default(),
            account_id: Some("acct-1".into()),
            created_at: 0,
        })
        .await;

        let routes = mgr.list(None).await;
        assert_eq!(routes.len(), 1);

        let filtered = mgr.list(Some("acct-1")).await;
        assert_eq!(filtered.len(), 1);

        let empty = mgr.list(Some("other-acct")).await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn route_manager_unbind() {
        let mgr = RouteManager::new();
        mgr.bind(RouteEntry {
            agent_id: "agent-1".into(),
            binding: AgentBindingMatch::default(),
            account_id: Some("acct-1".into()),
            created_at: 0,
        })
        .await;

        // Try unbinding with wrong account — should not remove.
        let removed = mgr.unbind("agent-1", Some("wrong-acct")).await;
        assert!(!removed);
        assert_eq!(mgr.list(None).await.len(), 1);

        // Unbind with correct account.
        let removed = mgr.unbind("agent-1", Some("acct-1")).await;
        assert!(removed);
        assert!(mgr.list(None).await.is_empty());
    }

    #[tokio::test]
    async fn route_manager_to_bindings() {
        let mgr = RouteManager::new();
        mgr.bind(RouteEntry {
            agent_id: "agent-1".into(),
            binding: AgentBindingMatch {
                channel: Some("telegram".into()),
                ..Default::default()
            },
            account_id: None,
            created_at: 0,
        })
        .await;

        let bindings = mgr.to_bindings().await;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].agent_id, "agent-1");
        assert_eq!(
            bindings[0].match_rule.channel.as_deref(),
            Some("telegram")
        );
    }
}
