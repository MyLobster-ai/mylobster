use crate::agents::acp::AcpAgentManager;
use crate::channels::ChannelManager;
use crate::cli::GatewayOpts;
use crate::config::Config;
use crate::gateway::auth::{resolve_gateway_auth, ResolvedGatewayAuth};
use crate::gateway::routes;
use crate::plugins::PluginRegistry;
use crate::routing::RouteManager;
use crate::sessions::SessionStore;

use anyhow::Result;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info};

// ============================================================================
// RPC State — in-memory stores for OpenClaw-compatible RPC methods
// ============================================================================

/// Extended RPC state for full OpenClaw API parity.
/// Each subsystem uses `parking_lot::RwLock` for synchronous access from
/// both sync and async handler functions.
pub struct RpcState {
    // Cron
    pub cron_jobs: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    pub cron_runs: parking_lot::RwLock<Vec<serde_json::Value>>,
    /// Per-job last error reason (v2026.3.11).
    pub cron_last_errors: parking_lot::RwLock<HashMap<String, String>>,
    /// Error count for status endpoint (v2026.3.11).
    pub cron_error_count: parking_lot::RwLock<u64>,
    // Agents
    pub agents: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    pub agent_files: parking_lot::RwLock<HashMap<String, HashMap<String, String>>>,
    // Device pairing
    pub device_pairs: parking_lot::RwLock<Vec<serde_json::Value>>,
    // Node management
    pub nodes: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    pub node_pairs: parking_lot::RwLock<Vec<serde_json::Value>>,
    pub node_invoke_results: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    /// Node pending-work queue (v2026.3.11).
    pub node_pending_work: parking_lot::RwLock<HashMap<String, Vec<serde_json::Value>>>,
    // Exec approvals
    pub exec_policies: parking_lot::RwLock<Vec<serde_json::Value>>,
    pub exec_node_policies: parking_lot::RwLock<Vec<serde_json::Value>>,
    pub exec_requests: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    // TTS
    pub tts_enabled: parking_lot::RwLock<bool>,
    pub tts_provider: parking_lot::RwLock<Option<String>>,
    // Voice wake
    pub voice_wake_triggers: parking_lot::RwLock<Vec<String>>,
    // Wizard
    pub wizard_active: parking_lot::RwLock<bool>,
    pub wizard_step: parking_lot::RwLock<u32>,
    // Usage tracking
    pub usage_input_tokens: parking_lot::RwLock<u64>,
    pub usage_output_tokens: parking_lot::RwLock<u64>,
    pub usage_requests: parking_lot::RwLock<u64>,
    // Heartbeat
    pub last_heartbeat_ms: parking_lot::RwLock<Option<u64>>,
    pub heartbeat_mode: parking_lot::RwLock<String>,
    // ACP agents (v2026.2.26)
    pub acp_manager: RwLock<AcpAgentManager>,
    // Route manager (v2026.2.26)
    pub route_manager: RwLock<RouteManager>,
    /// Model fallback state (v2026.3.11).
    pub model_fallback: parking_lot::RwLock<crate::agents::model_fallback::ModelFallbackState>,
}

impl RpcState {
    pub fn new() -> Self {
        Self {
            cron_jobs: parking_lot::RwLock::new(HashMap::new()),
            cron_runs: parking_lot::RwLock::new(Vec::new()),
            cron_last_errors: parking_lot::RwLock::new(HashMap::new()),
            cron_error_count: parking_lot::RwLock::new(0),
            agents: parking_lot::RwLock::new(HashMap::new()),
            agent_files: parking_lot::RwLock::new(HashMap::new()),
            device_pairs: parking_lot::RwLock::new(Vec::new()),
            nodes: parking_lot::RwLock::new(HashMap::new()),
            node_pairs: parking_lot::RwLock::new(Vec::new()),
            node_invoke_results: parking_lot::RwLock::new(HashMap::new()),
            node_pending_work: parking_lot::RwLock::new(HashMap::new()),
            exec_policies: parking_lot::RwLock::new(Vec::new()),
            exec_node_policies: parking_lot::RwLock::new(Vec::new()),
            exec_requests: parking_lot::RwLock::new(HashMap::new()),
            tts_enabled: parking_lot::RwLock::new(false),
            tts_provider: parking_lot::RwLock::new(None),
            voice_wake_triggers: parking_lot::RwLock::new(Vec::new()),
            wizard_active: parking_lot::RwLock::new(false),
            wizard_step: parking_lot::RwLock::new(0),
            usage_input_tokens: parking_lot::RwLock::new(0),
            usage_output_tokens: parking_lot::RwLock::new(0),
            usage_requests: parking_lot::RwLock::new(0),
            last_heartbeat_ms: parking_lot::RwLock::new(None),
            heartbeat_mode: parking_lot::RwLock::new("auto".to_string()),
            acp_manager: RwLock::new(AcpAgentManager::new()),
            route_manager: RwLock::new(RouteManager::new()),
            model_fallback: parking_lot::RwLock::new(
                crate::agents::model_fallback::ModelFallbackState::default(),
            ),
        }
    }
}

impl Default for RpcState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state for the gateway server.
#[derive(Clone)]
pub struct GatewayState {
    pub config: Arc<RwLock<Config>>,
    pub auth: Arc<ResolvedGatewayAuth>,
    pub sessions: Arc<SessionStore>,
    pub channels: Arc<ChannelManager>,
    pub plugins: Arc<PluginRegistry>,
    pub rpc: Arc<RpcState>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub start_time: std::time::Instant,
    pub version: String,
}

/// The gateway server.
pub struct GatewayServer {
    state: GatewayState,
    addr: SocketAddr,
    shutdown_rx: broadcast::Receiver<()>,
}

impl GatewayServer {
    /// Start the gateway server with the given configuration.
    pub async fn start(config: Config, opts: GatewayOpts) -> Result<Self> {
        let port = opts.port.unwrap_or(config.gateway.port);
        let bind_addr = resolve_bind_address(&config, opts.bind.as_deref(), port);

        info!("Resolving gateway authentication");
        let env_token = std::env::var("MYLOBSTER_GATEWAY_TOKEN").ok();
        let auth = resolve_gateway_auth(Some(&config.gateway.auth), env_token.as_deref());

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let sessions = SessionStore::new(&config);
        let channels = ChannelManager::new(&config);
        let plugins = PluginRegistry::new(&config);

        let rpc = RpcState::new();

        let state = GatewayState {
            config: Arc::new(RwLock::new(config)),
            auth: Arc::new(auth),
            sessions: Arc::new(sessions),
            channels: Arc::new(channels),
            plugins: Arc::new(plugins),
            rpc: Arc::new(rpc),
            shutdown_tx,
            start_time: std::time::Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };

        // Start channel monitors
        state.channels.start_all(&state).await?;

        info!("Gateway server binding to {}", bind_addr);

        Ok(Self {
            state,
            addr: bind_addr,
            shutdown_rx,
        })
    }

    /// Run the server until shutdown signal is received.
    pub async fn run_until_shutdown(self) -> Result<()> {
        let state = self.state.clone();
        let app = build_router(state.clone());

        let listener = tokio::net::TcpListener::bind(self.addr).await?;
        info!(
            "MyLobster gateway v{} listening on {}",
            state.version, self.addr
        );

        // Print startup banner
        print_startup_banner(&state, &self.addr);

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal(self.state.shutdown_tx.clone()))
        .await?;

        info!("Gateway server shut down gracefully");
        Ok(())
    }

    /// Get the server address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Trigger graceful shutdown.
    pub fn shutdown(&self) {
        let _ = self.state.shutdown_tx.send(());
    }
}

/// Build the Axum router with all routes.
fn build_router(state: GatewayState) -> Router {
    routes::build_routes(state)
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM).
async fn shutdown_signal(shutdown_tx: broadcast::Sender<()>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, initiating shutdown");
        }
        _ = terminate => {
            info!("Received SIGTERM, initiating shutdown");
        }
    }

    let _ = shutdown_tx.send(());
}

/// Resolve the bind address from configuration.
fn resolve_bind_address(config: &Config, bind_override: Option<&str>, port: u16) -> SocketAddr {
    let bind = bind_override
        .and_then(|b| b.parse().ok())
        .unwrap_or(config.gateway.bind);

    let host = match bind {
        crate::config::GatewayBindMode::Loopback => "127.0.0.1",
        crate::config::GatewayBindMode::Lan | crate::config::GatewayBindMode::Auto => "0.0.0.0",
        crate::config::GatewayBindMode::Custom => config
            .gateway
            .custom_bind_host
            .as_deref()
            .unwrap_or("0.0.0.0"),
        crate::config::GatewayBindMode::Tailnet => "100.64.0.0", // Tailscale CGNAT range
    };

    format!("{host}:{port}").parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // RpcState initialization (v2026.3.11)
    // ====================================================================

    #[test]
    fn rpc_state_new_initializes_all_fields() {
        let state = RpcState::new();
        assert!(state.cron_jobs.read().is_empty());
        assert!(state.cron_runs.read().is_empty());
        assert!(state.cron_last_errors.read().is_empty());
        assert_eq!(*state.cron_error_count.read(), 0);
        assert!(state.agents.read().is_empty());
        assert!(state.node_pending_work.read().is_empty());
        assert!(!state.model_fallback.read().is_on_cooldown("any-model"));
    }

    #[test]
    fn rpc_state_node_pending_work_operations() {
        let state = RpcState::new();
        {
            let mut pending = state.node_pending_work.write();
            let queue = pending.entry("node-1".to_string()).or_insert_with(Vec::new);
            queue.push(serde_json::json!({"task": "test"}));
        }
        assert_eq!(state.node_pending_work.read().get("node-1").unwrap().len(), 1);
    }

    #[test]
    fn rpc_state_cron_error_tracking() {
        let state = RpcState::new();
        {
            let mut errors = state.cron_last_errors.write();
            errors.insert("job-1".to_string(), "timeout".to_string());
            *state.cron_error_count.write() += 1;
        }
        assert_eq!(
            state.cron_last_errors.read().get("job-1").map(|s| s.as_str()),
            Some("timeout")
        );
        assert_eq!(*state.cron_error_count.read(), 1);
    }

    #[test]
    fn rpc_state_model_fallback_integration() {
        let state = RpcState::new();
        {
            let mut fb = state.model_fallback.write();
            fb.record_failure("claude-sonnet-4-6");
        }
        assert!(state.model_fallback.read().is_on_cooldown("claude-sonnet-4-6"));
        assert!(!state.model_fallback.read().is_on_cooldown("gpt-4o"));
    }

    #[test]
    fn rpc_state_default_is_new() {
        let state = RpcState::default();
        assert!(state.node_pending_work.read().is_empty());
        assert_eq!(*state.cron_error_count.read(), 0);
    }

    // ====================================================================
    // resolve_bind_address
    // ====================================================================

    #[test]
    fn bind_loopback_resolves_to_127() {
        let config = Config::default();
        let addr = resolve_bind_address(&config, None, 18789);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 18789);
    }

    #[test]
    fn bind_override_string() {
        let config = Config::default();
        let addr = resolve_bind_address(&config, Some("lan"), 9000);
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
        assert_eq!(addr.port(), 9000);
    }
}

/// Print startup banner with server info.
fn print_startup_banner(state: &GatewayState, addr: &SocketAddr) {
    let auth_mode = match state.auth.mode {
        crate::config::GatewayAuthMode::Token => {
            if state.auth.token.is_some() {
                "token"
            } else {
                "none (local only)"
            }
        }
        crate::config::GatewayAuthMode::Password => "password",
    };

    info!("-------------------------------------------");
    info!("  MyLobster Gateway v{}", state.version);
    info!("  Listening on: http://{}", addr);
    info!("  Auth mode: {}", auth_mode);
    info!("  WebSocket: ws://{}/ws", addr);
    info!("  Health: http://{}/api/health", addr);
    info!("  OpenAI compat: http://{}/v1/chat/completions", addr);
    info!("-------------------------------------------");
}
