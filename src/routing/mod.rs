//! Agent routing and binding resolution (v2026.2.26).
//!
//! Resolves which agent should handle a given session based on configured
//! bindings. Bindings match on channel, account_id, peer, and thread
//! patterns. Account-scoped route management prevents cross-account
//! agent hijacking.
//!
//! Ported from OpenClaw `src/routing/`.

pub mod access_groups;
pub mod dock;
pub mod policy;
pub mod target;

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
// DM main-session route pinning (v2026.5.2)
// ============================================================================

/// Decision for a DM-driven main-session route update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmRoutePin {
    /// Peer the main-session route should point at after the update.
    pub peer: String,
    /// True when the proposed peer was overridden to stay on the DM owner.
    pub pinned: bool,
}

/// Keep DM main-session route updates pinned to the configured DM owner.
///
/// Ported behavior (OpenClaw v2026.5.2, Discord/Mattermost/Matrix channels):
/// when a DM updates the shared main session's reply route, the route must
/// not drift to an arbitrary DM peer — if the channel has configured DM
/// owner(s) (`allowFrom`/owner list), the route stays pinned to the owner
/// peer (the proposing peer is used only when it IS an owner).
///
/// `owner_ids` are matched case-insensitively. With no configured owners the
/// proposed peer is accepted unchanged.
///
/// HANDOFF: applied in this cluster's channels (Mattermost/Matrix); the
/// Discord channel cluster should call this from its DM route mutator too.
pub fn pin_dm_main_session_route(
    owner_ids: &[String],
    proposed_peer: &str,
    current_route_peer: Option<&str>,
) -> DmRoutePin {
    let proposed = proposed_peer.trim();
    let owners: Vec<&str> = owner_ids
        .iter()
        .map(|o| o.trim())
        .filter(|o| !o.is_empty() && *o != "*")
        .collect();
    if owners.is_empty() {
        return DmRoutePin {
            peer: proposed.to_string(),
            pinned: false,
        };
    }
    let proposed_lower = proposed.to_lowercase();
    if owners.iter().any(|o| o.to_lowercase() == proposed_lower) {
        return DmRoutePin {
            peer: proposed.to_string(),
            pinned: false,
        };
    }
    // Non-owner DM peer: keep the current route if it already points at an
    // owner, else pin to the first configured owner.
    if let Some(current) = current_route_peer {
        let current_lower = current.trim().to_lowercase();
        if owners.iter().any(|o| o.to_lowercase() == current_lower) {
            return DmRoutePin {
                peer: current.trim().to_string(),
                pinned: true,
            };
        }
    }
    DmRoutePin {
        peer: owners[0].to_string(),
        pinned: true,
    }
}

// ============================================================================
// Cross-channel session delivery identity (v2026.7.1)
// ============================================================================

/// Identity a session turn should use for status/reactions/threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeliveryIdentity {
    pub channel: String,
    pub account_id: String,
    pub peer: String,
    /// True when the session previously belonged to a different channel and
    /// the stale identity was discarded.
    pub switched_channel: bool,
}

/// Resolve the delivery identity for the current turn of a shared DM session.
///
/// v2026.7.1 Channels row "Cross-channel session identity": a shared DM
/// session must not carry the previous channel's identity after a channel
/// switch — status reactions, typing, and thread targeting always use the
/// *current* inbound channel/account/peer, never the session's stored last
/// route.
pub fn resolve_session_delivery_identity(
    session_last_channel: Option<&str>,
    current_channel: &str,
    current_account_id: &str,
    current_peer: &str,
) -> SessionDeliveryIdentity {
    let switched = session_last_channel
        .map(|prev| !prev.trim().is_empty() && !prev.trim().eq_ignore_ascii_case(current_channel))
        .unwrap_or(false);
    SessionDeliveryIdentity {
        channel: current_channel.trim().to_lowercase(),
        account_id: current_account_id.trim().to_string(),
        peer: current_peer.trim().to_string(),
        switched_channel: switched,
    }
}

/// Per-channel-peer session isolation key (v2026.7.1 "per-channel-peer
/// session isolation"): the same human on two channels gets distinct DM
/// session keys unless identity-linked elsewhere.
pub fn channel_peer_session_key(channel: &str, account_id: &str, peer: &str) -> String {
    format!(
        "{}:{}:{}",
        channel.trim().to_lowercase(),
        account_id.trim().to_lowercase(),
        peer.trim().to_lowercase()
    )
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

    // ====================================================================
    // DM route pinning + session delivery identity
    // ====================================================================

    #[test]
    fn dm_route_pin_no_owners_accepts_proposed() {
        let pin = pin_dm_main_session_route(&[], "peer9", None);
        assert_eq!(pin.peer, "peer9");
        assert!(!pin.pinned);
        // Wildcard-only owner lists behave like no owners.
        let pin = pin_dm_main_session_route(&["*".into()], "peer9", None);
        assert_eq!(pin.peer, "peer9");
        assert!(!pin.pinned);
    }

    #[test]
    fn dm_route_pin_owner_peer_passes() {
        let owners = vec!["Owner1".to_string(), "owner2".to_string()];
        let pin = pin_dm_main_session_route(&owners, "OWNER2", None);
        assert_eq!(pin.peer, "OWNER2");
        assert!(!pin.pinned);
    }

    #[test]
    fn dm_route_pin_non_owner_stays_on_owner() {
        let owners = vec!["owner1".to_string()];
        // Current route already on an owner: keep it.
        let pin = pin_dm_main_session_route(&owners, "intruder", Some("owner1"));
        assert_eq!(pin.peer, "owner1");
        assert!(pin.pinned);
        // No current route: pin to first configured owner.
        let pin = pin_dm_main_session_route(&owners, "intruder", None);
        assert_eq!(pin.peer, "owner1");
        assert!(pin.pinned);
        // Current route drifted off-owner previously: re-pin to owner.
        let pin = pin_dm_main_session_route(&owners, "intruder", Some("other"));
        assert_eq!(pin.peer, "owner1");
        assert!(pin.pinned);
    }

    #[test]
    fn session_delivery_identity_uses_current_channel() {
        let id = resolve_session_delivery_identity(Some("telegram"), "Discord", "acct", "peer1");
        assert_eq!(id.channel, "discord");
        assert!(id.switched_channel);
        let id = resolve_session_delivery_identity(Some("discord"), "discord", "acct", "peer1");
        assert!(!id.switched_channel);
        let id = resolve_session_delivery_identity(None, "slack", "a", "p");
        assert!(!id.switched_channel);
    }

    #[test]
    fn channel_peer_key_isolates_channels() {
        assert_ne!(
            channel_peer_session_key("telegram", "default", "111"),
            channel_peer_session_key("discord", "default", "111")
        );
        assert_eq!(
            channel_peer_session_key(" Telegram ", "Default", "AbC"),
            "telegram:default:abc"
        );
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
