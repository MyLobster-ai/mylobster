use crate::config::{GatewayAuthConfig, GatewayAuthMode};
use crate::gateway::protocol::{ConnectAuthField, DeviceParams};
use ed25519_dalek::{Signature, VerifyingKey, Verifier};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::debug;

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

// ============================================================================
// Connect Auth (OC protocol)
// ============================================================================

/// Authorize a connect request using the OC auth field.
pub fn authorize_connect_auth(
    auth: &ResolvedGatewayAuth,
    connect_auth: Option<&ConnectAuthField>,
    is_local: bool,
) -> GatewayAuthResult {
    // Convert ConnectAuthField to ConnectAuth for reuse
    let legacy = connect_auth.map(|ca| ConnectAuth {
        token: ca.token.clone(),
        password: ca.password.clone(),
    });
    authorize_gateway_connect(auth, legacy.as_ref(), is_local)
}

// ============================================================================
// Ed25519 Device Identity Verification
// ============================================================================

/// Maximum allowed clock skew for device signatures (2 minutes).
const MAX_SIGNATURE_SKEW_MS: u64 = 120_000;

/// Result of device identity verification.
#[derive(Debug)]
pub struct DeviceVerifyResult {
    pub valid: bool,
    pub device_id: Option<String>,
    pub reason: Option<String>,
}

/// Verify an Ed25519 device identity from the connect handshake.
///
/// The bridge signs a v2 payload:
///   `"v2|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce"`
///
/// We reconstruct this payload and verify the Ed25519 signature.
pub fn verify_device_identity(
    device: &DeviceParams,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &[String],
    token: Option<&str>,
    expected_nonce: &str,
) -> DeviceVerifyResult {
    // 1. Validate nonce matches challenge
    if device.nonce != expected_nonce {
        return DeviceVerifyResult {
            valid: false,
            device_id: None,
            reason: Some("nonce mismatch".to_string()),
        };
    }

    // 2. Check signature freshness (within 2-minute skew)
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let skew = if now_ms > device.signed_at {
        now_ms - device.signed_at
    } else {
        device.signed_at - now_ms
    };

    if skew > MAX_SIGNATURE_SKEW_MS {
        return DeviceVerifyResult {
            valid: false,
            device_id: None,
            reason: Some(format!("signature too old: {}ms skew", skew)),
        };
    }

    // 3. Decode raw 32-byte Ed25519 public key from base64url
    let pub_key_bytes = match base64url_decode(&device.public_key) {
        Some(bytes) => bytes,
        None => {
            return DeviceVerifyResult {
                valid: false,
                device_id: None,
                reason: Some("invalid base64url public key".to_string()),
            }
        }
    };

    if pub_key_bytes.len() != 32 {
        return DeviceVerifyResult {
            valid: false,
            device_id: None,
            reason: Some(format!(
                "public key must be 32 bytes, got {}",
                pub_key_bytes.len()
            )),
        };
    }

    let verifying_key = match VerifyingKey::from_bytes(
        pub_key_bytes.as_slice().try_into().unwrap(),
    ) {
        Ok(k) => k,
        Err(e) => {
            return DeviceVerifyResult {
                valid: false,
                device_id: None,
                reason: Some(format!("invalid Ed25519 public key: {}", e)),
            }
        }
    };

    // 4. Decode signature from base64url
    let sig_bytes = match base64url_decode(&device.signature) {
        Some(bytes) => bytes,
        None => {
            return DeviceVerifyResult {
                valid: false,
                device_id: None,
                reason: Some("invalid base64url signature".to_string()),
            }
        }
    };

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(e) => {
            return DeviceVerifyResult {
                valid: false,
                device_id: None,
                reason: Some(format!("invalid Ed25519 signature: {}", e)),
            }
        }
    };

    // 5. Reconstruct v2 payload exactly as bridge builds it:
    //    "v2|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce"
    let scopes_str = scopes.join(",");
    let token_str = token.unwrap_or("");
    let payload = format!(
        "v2|{}|{}|{}|{}|{}|{}|{}|{}",
        device.id,
        client_id,
        client_mode,
        role,
        scopes_str,
        device.signed_at,
        token_str,
        device.nonce,
    );

    // 6. Verify Ed25519 signature over UTF-8 payload bytes
    match verifying_key.verify(payload.as_bytes(), &signature) {
        Ok(()) => DeviceVerifyResult {
            valid: true,
            device_id: Some(device.id.clone()),
            reason: None,
        },
        Err(e) => DeviceVerifyResult {
            valid: false,
            device_id: None,
            reason: Some(format!("signature verification failed: {}", e)),
        },
    }
}

/// Decode a base64url-encoded string (no padding) into bytes.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    use data_encoding::BASE64URL_NOPAD;
    // The bridge strips trailing '=' so we use no-pad variant.
    // base64url uses A-Z, a-z, 0-9, -, _ and is case-sensitive.
    BASE64URL_NOPAD.decode(input.as_bytes()).ok()
}
