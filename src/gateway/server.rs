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
use std::sync::atomic::AtomicUsize;
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
    /// Startup gate: shared retryable startup-sidecars error (v2026.5.2).
    pub startup_gate: crate::gateway::startup::StartupGate,
    /// Restart coordination (v2026.5.2 --force/--wait gateway-side support).
    pub restart: crate::gateway::restart::RestartCoordinator,
    /// Session organization state: archive/unread/groups (v2026.7.1).
    pub session_org: crate::gateway::sessions_rpc::SessionOrgState,
    /// Bounded sessions.list cache (v2026.5.2 large-store responsiveness).
    pub sessions_list_cache: crate::gateway::sessions_rpc::SessionsListCache,
    /// Cached health snapshot keyed by channel-state fingerprint (v2026.5.2).
    pub health_cache: crate::gateway::health::HealthSnapshotCache,
    /// Channels stopped via channels.stop (v2026.5.2).
    pub stopped_channels: parking_lot::RwLock<std::collections::HashSet<String>>,
    /// Terminal session registry (v2026.7.1 terminal.* RPCs).
    pub terminals: crate::gateway::system_rpc::TerminalRegistry,
    /// Talk session controller (v2026.7.1 talk.session.* RPCs).
    pub talk_sessions: crate::gateway::system_rpc::TalkSessionController,
    /// Bounded redacted startup errors for stability bundles (v2026.5.2).
    pub startup_errors: crate::gateway::diagnostics::StartupErrorLog,
    /// Idle liveness telemetry (v2026.5.2 — samples never hit warn logs).
    pub idle_liveness: crate::gateway::diagnostics::IdleLivenessTelemetry,
    /// Pairing-request flood guard (v2026.7.1).
    pub pairing_limiter: crate::gateway::trust::SlidingWindowRateLimiter,
    /// Plugin-tool-descriptor hash cache (v2026.5.2).
    pub descriptor_hash_cache: crate::gateway::dispatch::ToolDescriptorHashCache,
    /// Config generation counter for descriptor-hash cache keys (v2026.5.2).
    pub config_generation: crate::gateway::dispatch::ConfigGeneration,
    /// Descriptor-backed plugin method registry (v2026.7.1).
    pub method_registry: crate::gateway::method_registry::MethodRegistry,
    /// Dead-lettered delivery surfacing (v2026.7.1).
    pub dead_letters: crate::gateway::delivery_recovery::DeadLetterQueue,
    /// Control-plane-safe mode flag (crash-loop protection, v2026.7.1).
    pub safe_mode: std::sync::atomic::AtomicBool,
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
            startup_gate: crate::gateway::startup::StartupGate::new(),
            restart: crate::gateway::restart::RestartCoordinator::new(),
            session_org: crate::gateway::sessions_rpc::SessionOrgState::new(),
            sessions_list_cache: crate::gateway::sessions_rpc::SessionsListCache::default(),
            health_cache: crate::gateway::health::HealthSnapshotCache::default(),
            stopped_channels: parking_lot::RwLock::new(std::collections::HashSet::new()),
            terminals: crate::gateway::system_rpc::TerminalRegistry::new(),
            talk_sessions: crate::gateway::system_rpc::TalkSessionController::new(),
            startup_errors: crate::gateway::diagnostics::StartupErrorLog::new(),
            idle_liveness: crate::gateway::diagnostics::IdleLivenessTelemetry::new(),
            pairing_limiter: crate::gateway::trust::SlidingWindowRateLimiter::pairing_default(),
            descriptor_hash_cache: crate::gateway::dispatch::ToolDescriptorHashCache::new(),
            config_generation: crate::gateway::dispatch::ConfigGeneration::new(),
            method_registry: crate::gateway::method_registry::MethodRegistry::new(),
            dead_letters: crate::gateway::delivery_recovery::DeadLetterQueue::new(),
            safe_mode: std::sync::atomic::AtomicBool::new(false),
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
    /// Live count of connected WebSocket clients. Incremented on
    /// `handle_websocket` entry, decremented on exit.
    pub connected_clients: Arc<AtomicUsize>,
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
        let boot_started = std::time::Instant::now();
        let port = opts.port.unwrap_or(config.gateway.port);
        let bind_addr = resolve_bind_address(&config, opts.bind.as_deref(), port);

        info!("Resolving gateway authentication");
        let env_token = std::env::var("MYLOBSTER_GATEWAY_TOKEN").ok();
        let auth = resolve_gateway_auth(Some(&config.gateway.auth), env_token.as_deref());

        // v2026.7.1: fail closed when binding beyond loopback without any
        // shared secret / trusted proxy; likewise reject no-auth Tailscale
        // exposure.
        let bind_is_loopback = bind_addr.ip().is_loopback();
        crate::gateway::trust::require_auth_for_nonloopback(
            bind_is_loopback,
            auth.token.is_some(),
            auth.password.is_some(),
            config
                .gateway
                .trusted_proxies
                .as_ref()
                .map(|p| !p.is_empty())
                .unwrap_or(false),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let tailscale_enabled = !matches!(
            config.gateway.tailscale.mode,
            crate::config::GatewayTailscaleMode::Off
        );
        crate::gateway::trust::reject_noauth_tailscale_exposure(
            tailscale_enabled,
            auth.token.is_some(),
            auth.password.is_some(),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let phase_start = std::time::Instant::now();
        let sessions = SessionStore::new(&config);
        let channels = ChannelManager::new(&config);
        let plugins = PluginRegistry::new(&config);
        crate::gateway::startup::record_startup_phase("stores", phase_start);

        let rpc = RpcState::new();

        // Crash-loop safe mode: repeated unclean boots hold transports until
        // an operator recovers (v2026.7.1).
        let ledger_path = crate::gateway::boot_ledger::boot_ledger_path();
        let prior_boots = crate::gateway::boot_ledger::read_ledger(&ledger_path);
        let safe_mode = crate::gateway::boot_ledger::assess_safe_mode(&prior_boots);
        if safe_mode {
            tracing::warn!(
                "entering control-plane-safe mode after repeated unclean starts; \
                 channels are held until a clean shutdown resets the boot ledger"
            );
            rpc.safe_mode
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if let Err(e) = crate::gateway::boot_ledger::record_boot_start(
            &ledger_path,
            env!("CARGO_PKG_VERSION"),
            now_ms,
        ) {
            tracing::debug!("boot ledger unavailable: {e}");
        }

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
            connected_clients: Arc::new(AtomicUsize::new(0)),
        };

        // v2026.4.29: stale-session recovery — clear busy flags / turn
        // sources left behind by a previous run before serving traffic.
        recover_stale_sessions(&state.sessions);

        // Start channel monitors (held in control-plane-safe mode).
        let phase_start = std::time::Instant::now();
        if !state
            .rpc
            .safe_mode
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Err(e) = state.channels.start_all(&state).await {
                state.rpc.startup_errors.record(&format!("{e:#}"));
                return Err(e);
            }
        }
        crate::gateway::startup::record_startup_phase("channels", phase_start);

        // Sidecars ready — early control-plane RPCs stop returning the
        // shared retryable startup error (v2026.5.2).
        state.rpc.startup_gate.mark_sidecars_ready();

        // Slow-host startup diagnostics + event-loop readiness (v2026.4.27).
        crate::gateway::startup::record_startup_phase("total-boot", boot_started);
        let timeline = crate::gateway::startup::startup_timeline();
        if timeline.is_slow_host() {
            info!(
                "slow-host startup detected ({} ms total); phases: {:?}",
                timeline.total_ms(),
                timeline.slow_phases()
            );
        }
        let lag = crate::gateway::startup::measure_event_loop_lag().await;
        if !crate::gateway::startup::event_loop_ready(lag) {
            info!("event loop lag {}ms at startup (host under load)", lag.as_millis());
        }
        state.rpc.idle_liveness.record_sample(lag.as_millis() as u64);

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

        // Clean shutdown → upgrade this boot's ledger record so crash-loop
        // safe mode never triggers off graceful restarts (v2026.7.1).
        let ledger_path = crate::gateway::boot_ledger::boot_ledger_path();
        if let Err(e) = crate::gateway::boot_ledger::mark_clean_exit(&ledger_path) {
            tracing::debug!("boot ledger clean-exit mark failed: {e}");
        }

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

/// Stale-session recovery (v2026.4.29): clear busy flags and turn sources
/// left behind by an unclean previous run so fresh traffic is never blocked
/// behind phantom in-flight turns.
fn recover_stale_sessions(sessions: &SessionStore) {
    let mut recovered = 0usize;
    for info in sessions.list_sessions() {
        if let Some(handle) = sessions.get_session_handle(&info.session_key) {
            if handle.is_busy() {
                handle.set_busy(false);
                handle.clear_turn_source();
                recovered += 1;
            }
        }
    }
    if recovered > 0 {
        info!("recovered {recovered} stale busy session(s) at startup");
    }
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
