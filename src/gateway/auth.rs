use crate::config::{GatewayAuthConfig, GatewayAuthMode};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

// ============================================================================
// Types
// ============================================================================

/// The resolved authentication mode after processing config and env.
#[derive(Debug, Clone)]
pub struct ResolvedGatewayAuth {
    pub mode: GatewayAuthMode,
    pub token: Option<String>,
    pub password: Option<String>,
    pub allow_tailscale: bool,
}

/// Result of an authentication attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayAuthResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl GatewayAuthResult {
    pub fn success(method: &str) -> Self {
        Self {
            ok: true,
            method: Some(method.to_string()),
            user: None,
            reason: None,
        }
    }

    pub fn success_with_user(method: &str, user: &str) -> Self {
        Self {
            ok: true,
            method: Some(method.to_string()),
            user: Some(user.to_string()),
            reason: None,
        }
    }

    pub fn failure(reason: &str) -> Self {
        Self {
            ok: false,
            method: None,
            user: None,
            reason: Some(reason.to_string()),
        }
    }
}

/// Authentication credentials provided during connect.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectAuth {
    pub token: Option<String>,
    pub password: Option<String>,
}

// ============================================================================
// Core Functions
// ============================================================================

/// Resolve gateway auth configuration from config and environment.
pub fn resolve_gateway_auth(
    auth_config: Option<&GatewayAuthConfig>,
    env_token: Option<&str>,
) -> ResolvedGatewayAuth {
    let config = auth_config.cloned().unwrap_or_default();

    // Token from env overrides config
    let token = env_token.map(String::from).or(config.token);

    let password = config.password;

    let mode = if password.is_some() && token.is_none() {
        GatewayAuthMode::Password
    } else {
        config.mode
    };

    ResolvedGatewayAuth {
        mode,
        token,
        password,
        allow_tailscale: config.allow_tailscale,
    }
}

/// Check that gateway auth is properly configured.
pub fn assert_gateway_auth_configured(auth: &ResolvedGatewayAuth) -> Result<(), String> {
    match auth.mode {
        GatewayAuthMode::Token => {
            if auth.token.is_none() {
                return Err(
                    "Token auth mode requires MYLOBSTER_GATEWAY_TOKEN or gateway.auth.token"
                        .to_string(),
                );
            }
        }
        GatewayAuthMode::Password => {
            if auth.password.is_none() {
                return Err("Password auth mode requires gateway.auth.password".to_string());
            }
        }
    }
    Ok(())
}

/// Authorize a gateway connect attempt.
pub fn authorize_gateway_connect(
    auth: &ResolvedGatewayAuth,
    connect_auth: Option<&ConnectAuth>,
    is_local: bool,
) -> GatewayAuthResult {
    // If no auth configured, allow local connections
    if auth.token.is_none() && auth.password.is_none() {
        if is_local {
            debug!("Allowing local connection without auth");
            return GatewayAuthResult::success("local");
        }
        return GatewayAuthResult::failure("No authentication configured and request is not local");
    }

    let connect = connect_auth.cloned().unwrap_or_default();

    match auth.mode {
        GatewayAuthMode::Token => {
            if let Some(ref expected_token) = auth.token {
                if let Some(ref provided_token) = connect.token {
                    if safe_equal(expected_token, provided_token) {
                        return GatewayAuthResult::success("token");
                    }
                }
                // Also check password field as fallback
                if let Some(ref provided_password) = connect.password {
                    if safe_equal(expected_token, provided_password) {
                        return GatewayAuthResult::success("token");
                    }
                }
            }
            GatewayAuthResult::failure("Invalid or missing token")
        }
        GatewayAuthMode::Password => {
            if let Some(ref expected_password) = auth.password {
                if let Some(ref provided_password) = connect.password {
                    if safe_equal(expected_password, provided_password) {
                        return GatewayAuthResult::success("password");
                    }
                }
            }
            GatewayAuthResult::failure("Invalid or missing password")
        }
    }
}

/// Timing-safe string comparison.
fn safe_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Check if a request originates from localhost.
pub fn is_local_request(addr: &std::net::SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Extract bearer token from an Authorization header value.
pub fn extract_bearer_token(header: &str) -> Option<&str> {
    let header = header.trim();
    if header.len() > 7 && header[..7].eq_ignore_ascii_case("bearer ") {
        Some(header[7..].trim())
    } else {
        None
    }
}

/// Extract token from query string (e.g., `?token=xxx`).
pub fn extract_query_token(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if key == "token" {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ============================================================================
// Gateway Close Codes
// ============================================================================

/// Describe a WebSocket close code.
pub fn describe_gateway_close_code(code: u16) -> Option<&'static str> {
    match code {
        1000 => Some("normal closure"),
        1006 => Some("abnormal closure (no close frame)"),
        1008 => Some("policy violation"),
        1012 => Some("service restart"),
        _ => None,
    }
}
