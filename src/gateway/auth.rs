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
/// Supports both v2 and v3 payloads (v2026.2.26):
///   v2: `"v2|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce"`
///   v3: `"v3|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce|platform|deviceFamily"`
///
/// The version is auto-detected from the presence of `platform`/`device_family`
/// fields on the DeviceParams. v2 clients still work unchanged.
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

    // 5. Reconstruct payload — auto-detect v2 vs v3.
    let scopes_str = scopes.join(",");
    let token_str = token.unwrap_or("");

    let payload = if device.is_v3() {
        // v3: "v3|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce|platform|deviceFamily"
        let platform = device.normalized_platform().unwrap_or_default();
        let device_family = device.normalized_device_family().unwrap_or_default();
        format!(
            "v3|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            device.id,
            client_id,
            client_mode,
            role,
            scopes_str,
            device.signed_at,
            token_str,
            device.nonce,
            platform,
            device_family,
        )
    } else {
        // v2: "v2|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce"
        format!(
            "v2|{}|{}|{}|{}|{}|{}|{}|{}",
            device.id,
            client_id,
            client_mode,
            role,
            scopes_str,
            device.signed_at,
            token_str,
            device.nonce,
        )
    };

    // 6. Verify Ed25519 signature over UTF-8 payload bytes
    match verifying_key.verify(payload.as_bytes(), &signature) {
        Ok(()) => {
            if device.is_v3() {
                debug!(
                    "Device v3 identity verified: platform={:?}, family={:?}",
                    device.normalized_platform(),
                    device.normalized_device_family(),
                );
            }
            DeviceVerifyResult {
                valid: true,
                device_id: Some(device.id.clone()),
                reason: None,
            }
        }
        Err(_) if device.is_v3() => {
            // Fallback: try v2 payload for v3 params (bridge upgrade race).
            let v2_payload = format!(
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
            match verifying_key.verify(v2_payload.as_bytes(), &signature) {
                Ok(()) => {
                    debug!("Device identity verified via v2 fallback for v3 params");
                    DeviceVerifyResult {
                        valid: true,
                        device_id: Some(device.id.clone()),
                        reason: None,
                    }
                }
                Err(e) => DeviceVerifyResult {
                    valid: false,
                    device_id: None,
                    reason: Some(format!("signature verification failed: {}", e)),
                },
            }
        }
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayAuthMode;
    use data_encoding::BASE64URL_NOPAD;
    use ed25519_dalek::{SigningKey, Signer};

    // --- Helper: generate Ed25519 key pair and sign a v2 payload ---
    fn make_device_params(
        device_id: &str,
        client_id: &str,
        client_mode: &str,
        role: &str,
        scopes: &[&str],
        token: Option<&str>,
        nonce: &str,
    ) -> (DeviceParams, SigningKey) {
        let signing_key = {
            let mut secret = [0u8; 32];
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut secret);
            SigningKey::from_bytes(&secret)
        };
        let public_key_bytes = signing_key.verifying_key().to_bytes();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let scopes_str = scopes.join(",");
        let token_str = token.unwrap_or("");
        let payload = format!(
            "v2|{}|{}|{}|{}|{}|{}|{}|{}",
            device_id, client_id, client_mode, role, scopes_str, now_ms, token_str, nonce
        );

        let signature = signing_key.sign(payload.as_bytes());

        let params = DeviceParams {
            id: device_id.to_string(),
            public_key: BASE64URL_NOPAD.encode(&public_key_bytes),
            signature: BASE64URL_NOPAD.encode(&signature.to_bytes()),
            signed_at: now_ms,
            nonce: nonce.to_string(),
            platform: None,
            device_family: None,
        };

        (params, signing_key)
    }

    // =====================================================================
    // resolve_gateway_auth
    // =====================================================================

    #[test]
    fn resolve_auth_env_token_overrides_config_token() {
        let cfg = GatewayAuthConfig {
            mode: GatewayAuthMode::Token,
            token: Some("config-token".into()),
            password: None,
            allow_tailscale: false,
        };
        let resolved = resolve_gateway_auth(Some(&cfg), Some("env-token"));
        assert_eq!(resolved.token.as_deref(), Some("env-token"));
    }

    #[test]
    fn resolve_auth_password_only_switches_to_password_mode() {
        let cfg = GatewayAuthConfig {
            mode: GatewayAuthMode::Token, // default
            token: None,
            password: Some("secret".into()),
            allow_tailscale: false,
        };
        let resolved = resolve_gateway_auth(Some(&cfg), None);
        assert!(matches!(resolved.mode, GatewayAuthMode::Password));
        assert_eq!(resolved.password.as_deref(), Some("secret"));
    }

    #[test]
    fn resolve_auth_defaults_when_no_config() {
        let resolved = resolve_gateway_auth(None, None);
        assert!(resolved.token.is_none());
        assert!(resolved.password.is_none());
    }

    // =====================================================================
    // assert_gateway_auth_configured
    // =====================================================================

    #[test]
    fn assert_configured_token_mode_without_token_fails() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: None,
            password: None,
            allow_tailscale: false,
        };
        assert!(assert_gateway_auth_configured(&auth).is_err());
    }

    #[test]
    fn assert_configured_token_mode_with_token_ok() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: Some("t".into()),
            password: None,
            allow_tailscale: false,
        };
        assert!(assert_gateway_auth_configured(&auth).is_ok());
    }

    #[test]
    fn assert_configured_password_mode_without_password_fails() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Password,
            token: None,
            password: None,
            allow_tailscale: false,
        };
        assert!(assert_gateway_auth_configured(&auth).is_err());
    }

    // =====================================================================
    // authorize_gateway_connect — token mode
    // =====================================================================

    #[test]
    fn token_auth_correct_token_succeeds() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: Some("my-secret".into()),
            password: None,
            allow_tailscale: false,
        };
        let creds = ConnectAuth {
            token: Some("my-secret".into()),
            password: None,
        };
        let result = authorize_gateway_connect(&auth, Some(&creds), false);
        assert!(result.ok);
        assert_eq!(result.method.as_deref(), Some("token"));
    }

    #[test]
    fn token_auth_wrong_token_fails() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: Some("my-secret".into()),
            password: None,
            allow_tailscale: false,
        };
        let creds = ConnectAuth {
            token: Some("wrong".into()),
            password: None,
        };
        let result = authorize_gateway_connect(&auth, Some(&creds), false);
        assert!(!result.ok);
    }

    #[test]
    fn token_auth_password_field_fallback() {
        // Bridge sometimes sends token in the "password" field
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: Some("my-secret".into()),
            password: None,
            allow_tailscale: false,
        };
        let creds = ConnectAuth {
            token: None,
            password: Some("my-secret".into()),
        };
        let result = authorize_gateway_connect(&auth, Some(&creds), false);
        assert!(result.ok);
        assert_eq!(result.method.as_deref(), Some("token"));
    }

    #[test]
    fn no_auth_configured_allows_local() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: None,
            password: None,
            allow_tailscale: false,
        };
        let result = authorize_gateway_connect(&auth, None, true);
        assert!(result.ok);
        assert_eq!(result.method.as_deref(), Some("local"));
    }

    #[test]
    fn no_auth_configured_rejects_remote() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: None,
            password: None,
            allow_tailscale: false,
        };
        let result = authorize_gateway_connect(&auth, None, false);
        assert!(!result.ok);
    }

    // =====================================================================
    // authorize_gateway_connect — password mode
    // =====================================================================

    #[test]
    fn password_auth_correct_password_succeeds() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Password,
            token: None,
            password: Some("pass123".into()),
            allow_tailscale: false,
        };
        let creds = ConnectAuth {
            token: None,
            password: Some("pass123".into()),
        };
        let result = authorize_gateway_connect(&auth, Some(&creds), false);
        assert!(result.ok);
        assert_eq!(result.method.as_deref(), Some("password"));
    }

    #[test]
    fn password_auth_wrong_password_fails() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Password,
            token: None,
            password: Some("pass123".into()),
            allow_tailscale: false,
        };
        let creds = ConnectAuth {
            token: None,
            password: Some("wrong".into()),
        };
        let result = authorize_gateway_connect(&auth, Some(&creds), false);
        assert!(!result.ok);
    }

    // =====================================================================
    // authorize_connect_auth (OC ConnectAuthField wrapper)
    // =====================================================================

    #[test]
    fn connect_auth_field_token_succeeds() {
        let auth = ResolvedGatewayAuth {
            mode: GatewayAuthMode::Token,
            token: Some("tok".into()),
            password: None,
            allow_tailscale: false,
        };
        let field = ConnectAuthField {
            token: Some("tok".into()),
            password: None,
        };
        let result = authorize_connect_auth(&auth, Some(&field), false);
        assert!(result.ok);
    }

    // =====================================================================
    // extract_bearer_token
    // =====================================================================

    #[test]
    fn extract_bearer_standard() {
        assert_eq!(extract_bearer_token("Bearer abc123"), Some("abc123"));
    }

    #[test]
    fn extract_bearer_case_insensitive() {
        assert_eq!(extract_bearer_token("bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer_token("BEARER abc123"), Some("abc123"));
    }

    #[test]
    fn extract_bearer_not_bearer() {
        assert!(extract_bearer_token("Basic abc123").is_none());
        assert!(extract_bearer_token("").is_none());
        assert!(extract_bearer_token("Bearer").is_none());
    }

    // =====================================================================
    // extract_query_token
    // =====================================================================

    #[test]
    fn extract_query_token_found() {
        assert_eq!(
            extract_query_token("foo=bar&token=abc123&baz=1"),
            Some("abc123".into())
        );
    }

    #[test]
    fn extract_query_token_not_found() {
        assert!(extract_query_token("foo=bar&baz=1").is_none());
    }

    #[test]
    fn extract_query_token_first_param() {
        assert_eq!(
            extract_query_token("token=first"),
            Some("first".into())
        );
    }

    // =====================================================================
    // is_local_request
    // =====================================================================

    #[test]
    fn localhost_is_local() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(is_local_request(&addr));
    }

    #[test]
    fn ipv6_loopback_is_local() {
        let addr: std::net::SocketAddr = "[::1]:8080".parse().unwrap();
        assert!(is_local_request(&addr));
    }

    #[test]
    fn remote_ip_is_not_local() {
        let addr: std::net::SocketAddr = "1.2.3.4:8080".parse().unwrap();
        assert!(!is_local_request(&addr));
    }

    // =====================================================================
    // describe_gateway_close_code
    // =====================================================================

    #[test]
    fn known_close_codes() {
        assert_eq!(describe_gateway_close_code(1000), Some("normal closure"));
        assert_eq!(describe_gateway_close_code(1006), Some("abnormal closure (no close frame)"));
        assert_eq!(describe_gateway_close_code(1008), Some("policy violation"));
        assert_eq!(describe_gateway_close_code(1012), Some("service restart"));
    }

    #[test]
    fn unknown_close_code() {
        assert!(describe_gateway_close_code(9999).is_none());
    }

    // =====================================================================
    // Ed25519 device identity verification
    // =====================================================================

    #[test]
    fn valid_device_identity_verification() {
        let nonce = "challenge-nonce-abc";
        let (device, _signing_key) = make_device_params(
            "device-123",
            "gateway-client",
            "bridge",
            "operator",
            &["operator.admin"],
            Some("tok123"),
            nonce,
        );

        let result = verify_device_identity(
            &device,
            "gateway-client",
            "bridge",
            "operator",
            &["operator.admin".to_string()],
            Some("tok123"),
            nonce,
        );

        assert!(result.valid, "Expected valid, got: {:?}", result.reason);
        assert_eq!(result.device_id.as_deref(), Some("device-123"));
    }

    #[test]
    fn device_identity_nonce_mismatch_fails() {
        let (device, _) = make_device_params(
            "dev1", "gc", "bridge", "operator", &["operator.admin"], None, "nonce-A",
        );
        let result = verify_device_identity(
            &device, "gc", "bridge", "operator",
            &["operator.admin".to_string()], None, "nonce-B",
        );
        assert!(!result.valid);
        assert!(result.reason.as_deref().unwrap().contains("nonce mismatch"));
    }

    #[test]
    fn device_identity_stale_signature_fails() {
        let nonce = "test-nonce";
        let signing_key = {
            let mut secret = [0u8; 32];
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut secret);
            SigningKey::from_bytes(&secret)
        };
        let public_key_bytes = signing_key.verifying_key().to_bytes();

        // 5 minutes ago — beyond 2-minute skew
        let stale_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 300_000;

        let payload = format!("v2|dev1|gc|bridge|operator||{}||{}", stale_ms, nonce);
        let signature = signing_key.sign(payload.as_bytes());

        let device = DeviceParams {
            id: "dev1".to_string(),
            public_key: BASE64URL_NOPAD.encode(&public_key_bytes),
            signature: BASE64URL_NOPAD.encode(&signature.to_bytes()),
            signed_at: stale_ms,
            nonce: nonce.to_string(),
            platform: None,
            device_family: None,
        };

        let result = verify_device_identity(
            &device, "gc", "bridge", "operator", &[], None, nonce,
        );
        assert!(!result.valid);
        assert!(result.reason.as_deref().unwrap().contains("too old"));
    }

    #[test]
    fn device_identity_wrong_payload_fails() {
        let nonce = "test-nonce";
        let (mut device, _) = make_device_params(
            "dev1", "gc", "bridge", "operator", &["operator.admin"], None, nonce,
        );
        // Tamper with device ID after signing
        device.id = "tampered-id".to_string();

        let result = verify_device_identity(
            &device, "gc", "bridge", "operator",
            &["operator.admin".to_string()], None, nonce,
        );
        assert!(!result.valid);
        assert!(result.reason.as_deref().unwrap().contains("verification failed"));
    }

    #[test]
    fn device_identity_invalid_public_key() {
        let device = DeviceParams {
            id: "dev1".to_string(),
            public_key: "not-valid-base64url!!!".to_string(),
            signature: "AAAA".to_string(),
            signed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            nonce: "n".to_string(),
            platform: None,
            device_family: None,
        };
        let result = verify_device_identity(&device, "gc", "bridge", "op", &[], None, "n");
        assert!(!result.valid);
        assert!(result.reason.as_deref().unwrap().contains("base64url"));
    }

    #[test]
    fn device_identity_wrong_key_length() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // 16 bytes instead of 32
        let short_key = BASE64URL_NOPAD.encode(&[0u8; 16]);
        let device = DeviceParams {
            id: "dev1".to_string(),
            public_key: short_key,
            signature: BASE64URL_NOPAD.encode(&[0u8; 64]),
            signed_at: now_ms,
            nonce: "n".to_string(),
            platform: None,
            device_family: None,
        };
        let result = verify_device_identity(&device, "gc", "bridge", "op", &[], None, "n");
        assert!(!result.valid);
        assert!(result.reason.as_deref().unwrap().contains("32 bytes"));
    }

    #[test]
    fn device_identity_multiple_scopes() {
        let nonce = "scope-nonce";
        let (device, _) = make_device_params(
            "dev1", "gc", "bridge", "operator",
            &["operator.read", "operator.write"],
            Some("t"),
            nonce,
        );
        let result = verify_device_identity(
            &device, "gc", "bridge", "operator",
            &["operator.read".to_string(), "operator.write".to_string()],
            Some("t"),
            nonce,
        );
        assert!(result.valid, "Expected valid, got: {:?}", result.reason);
    }

    #[test]
    fn device_identity_empty_token() {
        let nonce = "no-tok";
        let (device, _) = make_device_params(
            "dev1", "gc", "bridge", "operator", &[], None, nonce,
        );
        let result = verify_device_identity(
            &device, "gc", "bridge", "operator", &[], None, nonce,
        );
        assert!(result.valid, "Expected valid, got: {:?}", result.reason);
    }

    // =====================================================================
    // Ed25519 device identity verification — v3 (v2026.2.26)
    // =====================================================================

    fn make_device_params_v3(
        device_id: &str,
        client_id: &str,
        client_mode: &str,
        role: &str,
        scopes: &[&str],
        token: Option<&str>,
        nonce: &str,
        platform: &str,
        device_family: &str,
    ) -> (DeviceParams, SigningKey) {
        let signing_key = {
            let mut secret = [0u8; 32];
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut secret);
            SigningKey::from_bytes(&secret)
        };
        let public_key_bytes = signing_key.verifying_key().to_bytes();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let scopes_str = scopes.join(",");
        let token_str = token.unwrap_or("");
        let platform_lower = platform.to_ascii_lowercase();
        let family_lower = device_family.to_ascii_lowercase();
        let payload = format!(
            "v3|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            device_id, client_id, client_mode, role, scopes_str, now_ms,
            token_str, nonce, platform_lower, family_lower
        );

        let signature = signing_key.sign(payload.as_bytes());

        let params = DeviceParams {
            id: device_id.to_string(),
            public_key: BASE64URL_NOPAD.encode(&public_key_bytes),
            signature: BASE64URL_NOPAD.encode(&signature.to_bytes()),
            signed_at: now_ms,
            nonce: nonce.to_string(),
            platform: Some(platform.to_string()),
            device_family: Some(device_family.to_string()),
        };

        (params, signing_key)
    }

    #[test]
    fn valid_v3_device_identity() {
        let nonce = "v3-nonce";
        let (device, _) = make_device_params_v3(
            "dev-v3", "gc", "bridge", "operator",
            &["operator.admin"], Some("tok"), nonce,
            "Darwin", "Desktop",
        );

        let result = verify_device_identity(
            &device, "gc", "bridge", "operator",
            &["operator.admin".to_string()], Some("tok"), nonce,
        );
        assert!(result.valid, "Expected valid v3, got: {:?}", result.reason);
    }

    #[test]
    fn v3_normalizes_platform_case() {
        let nonce = "case-nonce";
        let (device, _) = make_device_params_v3(
            "dev-case", "gc", "bridge", "operator",
            &[], None, nonce,
            "LINUX", "SERVER",
        );

        let result = verify_device_identity(
            &device, "gc", "bridge", "operator", &[], None, nonce,
        );
        assert!(result.valid, "Expected valid with case normalization, got: {:?}", result.reason);
    }

    #[test]
    fn v2_client_still_works_unchanged() {
        // Ensure v2 params without platform/deviceFamily still verify.
        let nonce = "v2-compat";
        let (device, _) = make_device_params(
            "dev-v2", "gc", "bridge", "operator", &["operator.write"],
            Some("t"), nonce,
        );
        assert!(!device.is_v3());

        let result = verify_device_identity(
            &device, "gc", "bridge", "operator",
            &["operator.write".to_string()], Some("t"), nonce,
        );
        assert!(result.valid, "v2 backward compat failed: {:?}", result.reason);
    }

    // =====================================================================
    // base64url_decode
    // =====================================================================

    #[test]
    fn base64url_decode_valid() {
        // "hello" in base64url = "aGVsbG8"
        let decoded = base64url_decode("aGVsbG8").unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn base64url_decode_invalid() {
        assert!(base64url_decode("!!!invalid!!!").is_none());
    }

    // =====================================================================
    // GatewayAuthResult serialization (parity with OC)
    // =====================================================================

    #[test]
    fn auth_result_success_serialization() {
        let r = GatewayAuthResult::success("token");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["method"], "token");
        // user and reason should be absent (skip_serializing_if)
        assert!(v.get("user").is_none());
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn auth_result_failure_serialization() {
        let r = GatewayAuthResult::failure("bad token");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["reason"], "bad token");
        assert!(v.get("method").is_none());
    }

    #[test]
    fn auth_result_success_with_user() {
        let r = GatewayAuthResult::success_with_user("token", "user@example.com");
        assert!(r.ok);
        assert_eq!(r.user.as_deref(), Some("user@example.com"));
    }
}
