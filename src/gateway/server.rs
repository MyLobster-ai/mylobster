use crate::channels::ChannelManager;
use crate::cli::GatewayOpts;
use crate::config::Config;
use crate::gateway::auth::{resolve_gateway_auth, ResolvedGatewayAuth};
use crate::gateway::routes;
use crate::plugins::PluginRegistry;
use crate::sessions::SessionStore;

use anyhow::Result;
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info};

/// Shared state for the gateway server.
#[derive(Clone)]
pub struct GatewayState {
    pub config: Arc<RwLock<Config>>,
    pub auth: Arc<ResolvedGatewayAuth>,
    pub sessions: Arc<SessionStore>,
    pub channels: Arc<ChannelManager>,
    pub plugins: Arc<PluginRegistry>,
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

        let state = GatewayState {
            config: Arc::new(RwLock::new(config)),
            auth: Arc::new(auth),
            sessions: Arc::new(sessions),
            channels: Arc::new(channels),
            plugins: Arc::new(plugins),
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

        axum::serve(listener, app)
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
