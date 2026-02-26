//! Trusted-proxy control-UI bypass policy (v2026.2.25).
//!
//! Centralizes control-UI auth decisions so that gateway connection
//! handlers and route guards share a single policy resolution path.
//! Ported from OpenClaw's connect-policy module.

use crate::config::GatewayAuthMode;

/// Resolved authentication policy for the control UI.
#[derive(Debug, Clone)]
pub struct ControlUiAuthPolicy {
    /// Whether the control UI requires device authentication.
    pub require_device_auth: bool,
    /// Whether the control UI requires a token or password.
    pub require_credential: bool,
    /// Whether insecure (non-TLS) auth is allowed.
    pub allow_insecure: bool,
}

/// Resolve the effective control-UI auth policy from gateway configuration.
///
/// Takes into account `dangerously_disable_device_auth`, `allow_insecure_auth`,
/// and the gateway's auth mode.
pub fn resolve_control_ui_auth_policy(
    auth_mode: GatewayAuthMode,
    dangerously_disable_device_auth: bool,
    allow_insecure_auth: bool,
) -> ControlUiAuthPolicy {
    ControlUiAuthPolicy {
        require_device_auth: !dangerously_disable_device_auth,
        require_credential: auth_mode != GatewayAuthMode::Token || !allow_insecure_auth,
        allow_insecure: allow_insecure_auth,
    }
}

/// Check whether a connection qualifies as a trusted-proxy control-UI
/// operator — i.e., the request came from a reverse proxy that has already
/// authenticated the user.
///
/// A request is considered trusted-proxy if:
/// 1. The role is `"operator"` (not a regular user).
/// 2. The auth mode is token-based.
/// 3. The authentication method used was `"proxy"` or `"tailscale"`.
pub fn is_trusted_proxy_control_ui_operator_auth(
    role: &str,
    auth_mode: GatewayAuthMode,
    auth_method: &str,
) -> bool {
    role == "operator"
        && auth_mode == GatewayAuthMode::Token
        && (auth_method == "proxy" || auth_method == "tailscale")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_requires_everything() {
        let policy = resolve_control_ui_auth_policy(
            GatewayAuthMode::Token,
            false,
            false,
        );
        assert!(policy.require_device_auth);
        assert!(policy.require_credential);
        assert!(!policy.allow_insecure);
    }

    #[test]
    fn disable_device_auth() {
        let policy = resolve_control_ui_auth_policy(
            GatewayAuthMode::Token,
            true,
            false,
        );
        assert!(!policy.require_device_auth);
    }

    #[test]
    fn insecure_auth_relaxes_credential() {
        let policy = resolve_control_ui_auth_policy(
            GatewayAuthMode::Token,
            false,
            true,
        );
        assert!(!policy.require_credential);
        assert!(policy.allow_insecure);
    }

    #[test]
    fn password_mode_always_requires_credential() {
        let policy = resolve_control_ui_auth_policy(
            GatewayAuthMode::Password,
            false,
            true,
        );
        assert!(policy.require_credential);
    }

    #[test]
    fn trusted_proxy_operator() {
        assert!(is_trusted_proxy_control_ui_operator_auth(
            "operator",
            GatewayAuthMode::Token,
            "proxy",
        ));
    }

    #[test]
    fn trusted_proxy_tailscale() {
        assert!(is_trusted_proxy_control_ui_operator_auth(
            "operator",
            GatewayAuthMode::Token,
            "tailscale",
        ));
    }

    #[test]
    fn non_operator_not_trusted() {
        assert!(!is_trusted_proxy_control_ui_operator_auth(
            "user",
            GatewayAuthMode::Token,
            "proxy",
        ));
    }

    #[test]
    fn password_mode_not_trusted() {
        assert!(!is_trusted_proxy_control_ui_operator_auth(
            "operator",
            GatewayAuthMode::Password,
            "proxy",
        ));
    }

    #[test]
    fn bearer_auth_not_trusted() {
        assert!(!is_trusted_proxy_control_ui_operator_auth(
            "operator",
            GatewayAuthMode::Token,
            "bearer",
        ));
    }
}
