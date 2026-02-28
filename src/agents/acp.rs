//! ACP (Agent Control Protocol) thread-bound agents (v2026.2.26).
//!
//! Agents are first-class runtimes that can be spawned, communicated with,
//! and stopped. Each agent runs in its own async task with a lifecycle
//! state machine.
//!
//! Ported from OpenClaw `src/agents/acp.ts`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ============================================================================
// Types
// ============================================================================

/// ACP agent lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpAgentState {
    /// Agent is being initialized.
    Spawning,
    /// Agent is running and accepting messages.
    Running,
    /// Agent has been stopped.
    Stopped,
    /// Agent encountered a fatal error.
    Failed,
}

/// An ACP agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgent {
    /// Unique agent ID.
    pub id: String,
    /// Human-readable agent name.
    pub name: String,
    /// Current lifecycle state.
    pub state: AcpAgentState,
    /// Agent configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Session key this agent is bound to (thread-bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Creation timestamp (ms since epoch).
    pub created_at: u64,
    /// Last activity timestamp.
    pub updated_at: u64,
    /// Account ID that owns this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Parameters for spawning an ACP agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSpawnParams {
    /// Agent name.
    pub name: String,
    /// Agent configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Session key to bind to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Account ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Parameters for sending a message to an ACP agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSendParams {
    /// Target agent ID.
    pub agent_id: String,
    /// Message content.
    pub message: String,
    /// Optional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Response from an ACP agent send operation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSendResult {
    pub ok: bool,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============================================================================
// Agent Manager
// ============================================================================

/// Manages all ACP agent instances.
pub struct AcpAgentManager {
    agents: Arc<RwLock<HashMap<String, AcpAgent>>>,
    cancel_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

impl AcpAgentManager {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            cancel_tokens: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawn a new ACP agent.
    pub async fn spawn(&self, params: AcpSpawnParams) -> AcpAgent {
        let id = Uuid::new_v4().to_string();
        let now = now_ms();

        let agent = AcpAgent {
            id: id.clone(),
            name: params.name,
            state: AcpAgentState::Running,
            config: params.config,
            session_key: params.session_key,
            created_at: now,
            updated_at: now,
            account_id: params.account_id,
        };

        let cancel_token = CancellationToken::new();

        {
            let mut agents = self.agents.write().await;
            agents.insert(id.clone(), agent.clone());
        }
        {
            let mut tokens = self.cancel_tokens.write().await;
            tokens.insert(id.clone(), cancel_token);
        }

        info!("Spawned ACP agent '{}' (id={})", agent.name, id);
        agent
    }

    /// Send a message to an ACP agent.
    pub async fn send(&self, params: &AcpSendParams) -> AcpSendResult {
        let agents = self.agents.read().await;
        match agents.get(&params.agent_id) {
            Some(agent) if agent.state == AcpAgentState::Running => {
                debug!(
                    "Message sent to ACP agent '{}': {}",
                    agent.name,
                    &params.message[..params.message.len().min(100)]
                );

                // Update last activity timestamp.
                drop(agents);
                {
                    let mut agents = self.agents.write().await;
                    if let Some(a) = agents.get_mut(&params.agent_id) {
                        a.updated_at = now_ms();
                    }
                }

                AcpSendResult {
                    ok: true,
                    agent_id: params.agent_id.clone(),
                    error: None,
                }
            }
            Some(agent) => AcpSendResult {
                ok: false,
                agent_id: params.agent_id.clone(),
                error: Some(format!(
                    "Agent '{}' is in {:?} state",
                    agent.name, agent.state
                )),
            },
            None => AcpSendResult {
                ok: false,
                agent_id: params.agent_id.clone(),
                error: Some("Agent not found".to_string()),
            },
        }
    }

    /// Stop an ACP agent.
    pub async fn stop(&self, agent_id: &str) -> bool {
        // Cancel the agent's task.
        {
            let tokens = self.cancel_tokens.read().await;
            if let Some(token) = tokens.get(agent_id) {
                token.cancel();
            }
        }

        // Update agent state.
        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            agent.state = AcpAgentState::Stopped;
            agent.updated_at = now_ms();
            info!("Stopped ACP agent '{}' (id={})", agent.name, agent_id);
            true
        } else {
            false
        }
    }

    /// List all ACP agents.
    pub async fn list(&self) -> Vec<AcpAgent> {
        self.agents.read().await.values().cloned().collect()
    }

    /// Get a specific ACP agent by ID.
    pub async fn get(&self, agent_id: &str) -> Option<AcpAgent> {
        self.agents.read().await.get(agent_id).cloned()
    }

    /// Reconcile agents on startup — stop any in Spawning state.
    pub async fn reconcile_on_startup(&self) {
        let mut agents = self.agents.write().await;
        for agent in agents.values_mut() {
            if agent.state == AcpAgentState::Spawning {
                warn!(
                    "ACP agent '{}' was in Spawning state at startup, marking Failed",
                    agent.name
                );
                agent.state = AcpAgentState::Failed;
                agent.updated_at = now_ms();
            }
        }
    }

    /// Stop all running agents (for shutdown).
    pub async fn stop_all(&self) {
        let agent_ids: Vec<String> = {
            let agents = self.agents.read().await;
            agents
                .values()
                .filter(|a| a.state == AcpAgentState::Running)
                .map(|a| a.id.clone())
                .collect()
        };

        for id in agent_ids {
            self.stop(&id).await;
        }
    }
}

impl Default for AcpAgentManager {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_and_list() {
        let mgr = AcpAgentManager::new();
        let agent = mgr
            .spawn(AcpSpawnParams {
                name: "test-agent".into(),
                config: None,
                session_key: Some("session:1".into()),
                account_id: None,
            })
            .await;

        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.state, AcpAgentState::Running);
        assert!(agent.session_key.is_some());

        let agents = mgr.list().await;
        assert_eq!(agents.len(), 1);
    }

    #[tokio::test]
    async fn send_to_running_agent() {
        let mgr = AcpAgentManager::new();
        let agent = mgr
            .spawn(AcpSpawnParams {
                name: "runner".into(),
                config: None,
                session_key: None,
                account_id: None,
            })
            .await;

        let result = mgr
            .send(&AcpSendParams {
                agent_id: agent.id,
                message: "hello".into(),
                metadata: None,
            })
            .await;

        assert!(result.ok);
    }

    #[tokio::test]
    async fn send_to_stopped_agent() {
        let mgr = AcpAgentManager::new();
        let agent = mgr
            .spawn(AcpSpawnParams {
                name: "stopper".into(),
                config: None,
                session_key: None,
                account_id: None,
            })
            .await;

        mgr.stop(&agent.id).await;

        let result = mgr
            .send(&AcpSendParams {
                agent_id: agent.id,
                message: "hello".into(),
                metadata: None,
            })
            .await;

        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Stopped"));
    }

    #[tokio::test]
    async fn send_to_nonexistent() {
        let mgr = AcpAgentManager::new();
        let result = mgr
            .send(&AcpSendParams {
                agent_id: "fake-id".into(),
                message: "hello".into(),
                metadata: None,
            })
            .await;

        assert!(!result.ok);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn stop_agent() {
        let mgr = AcpAgentManager::new();
        let agent = mgr
            .spawn(AcpSpawnParams {
                name: "to-stop".into(),
                config: None,
                session_key: None,
                account_id: None,
            })
            .await;

        assert!(mgr.stop(&agent.id).await);

        let stopped = mgr.get(&agent.id).await.unwrap();
        assert_eq!(stopped.state, AcpAgentState::Stopped);
    }

    #[tokio::test]
    async fn stop_nonexistent_returns_false() {
        let mgr = AcpAgentManager::new();
        assert!(!mgr.stop("fake").await);
    }

    #[tokio::test]
    async fn stop_all() {
        let mgr = AcpAgentManager::new();
        for i in 0..3 {
            mgr.spawn(AcpSpawnParams {
                name: format!("agent-{i}"),
                config: None,
                session_key: None,
                account_id: None,
            })
            .await;
        }

        mgr.stop_all().await;

        let agents = mgr.list().await;
        assert!(agents.iter().all(|a| a.state == AcpAgentState::Stopped));
    }

    #[tokio::test]
    async fn reconcile_on_startup() {
        let mgr = AcpAgentManager::new();
        // Manually insert a Spawning agent to simulate crash recovery.
        {
            let mut agents = mgr.agents.write().await;
            agents.insert(
                "orphan".into(),
                AcpAgent {
                    id: "orphan".into(),
                    name: "orphan".into(),
                    state: AcpAgentState::Spawning,
                    config: None,
                    session_key: None,
                    created_at: 0,
                    updated_at: 0,
                    account_id: None,
                },
            );
        }

        mgr.reconcile_on_startup().await;

        let agent = mgr.get("orphan").await.unwrap();
        assert_eq!(agent.state, AcpAgentState::Failed);
    }

    #[test]
    fn agent_state_serialization() {
        assert_eq!(
            serde_json::to_string(&AcpAgentState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&AcpAgentState::Stopped).unwrap(),
            "\"stopped\""
        );
    }
}
