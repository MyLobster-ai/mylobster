//! Gateway trust & auth hardening helpers (v2026.7.1 parity).
//!
//! - Default remote auth rate limiter (`gateway.auth.rateLimit`, loopback
//!   exempt).
//! - Fail-closed non-loopback start without a shared secret.
//! - Forged loopback Origin rejection.
//! - Malformed HTTP/WS request-target rejection (GHSA-6hc3-f4rg-377m).
//! - Trusted package sources reject lookalike sibling paths.
//! - No auth-token forwarding on cross-origin redirects.
//! - Pairing flood guard for node/device pairing requests.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};

// ============================================================================
// Sliding-window rate limiter (auth attempts, pairing floods)
// ============================================================================

/// Default remote auth attempt limit: 10 attempts / 60 s per peer.
pub const DEFAULT_AUTH_MAX_ATTEMPTS: u32 = 10;
pub const DEFAULT_AUTH_WINDOW: Duration = Duration::from_secs(60);

/// Default pairing-request flood guard: 5 requests / 60 s per peer.
pub const DEFAULT_PAIRING_MAX_REQUESTS: u32 = 5;

pub struct SlidingWindowRateLimiter {
    max_events: u32,
    window: Duration,
    events: parking_lot::Mutex<HashMap<String, Vec<Instant>>>,
}

impl SlidingWindowRateLimiter {
    pub fn new(max_events: u32, window: Duration) -> Self {
        Self {
            max_events: max_events.max(1),
            window,
            events: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    pub fn auth_default() -> Self {
        Self::new(DEFAULT_AUTH_MAX_ATTEMPTS, DEFAULT_AUTH_WINDOW)
    }

    pub fn pairing_default() -> Self {
        Self::new(DEFAULT_PAIRING_MAX_REQUESTS, DEFAULT_AUTH_WINDOW)
    }

    /// Record an event for `key`; returns false when the key is over limit.
    pub fn check_and_record(&self, key: &str) -> bool {
        self.check_and_record_at(key, Instant::now())
    }

    fn check_and_record_at(&self, key: &str, now: Instant) -> bool {
        let mut events = self.events.lock();
        // Opportunistic global cleanup to bound memory.
        if events.len() > 1024 {
            let window = self.window;
            events.retain(|_, v| {
                v.retain(|t| now.duration_since(*t) < window);
                !v.is_empty()
            });
        }
        let entry = events.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() >= self.max_events as usize {
            return false;
        }
        entry.push(now);
        true
    }
}

/// Whether an auth attempt from `peer` must pass the rate limiter
/// (loopback peers are exempt per upstream default).
pub fn auth_rate_limit_applies(peer: IpAddr, loopback_exempt: bool) -> bool {
    !(loopback_exempt && peer.is_loopback())
}

// ============================================================================
// Fail-closed non-loopback start (v2026.7.1)
// ============================================================================

/// Fail-closed check: refuse to start a gateway bound beyond loopback with
/// no shared secret (token/password) and no trusted proxy configured.
pub fn require_auth_for_nonloopback(
    bind_is_loopback: bool,
    has_token: bool,
    has_password: bool,
    has_trusted_proxy: bool,
) -> Result<(), String> {
    if bind_is_loopback || has_token || has_password || has_trusted_proxy {
        Ok(())
    } else {
        Err(
            "refusing to start: gateway binds beyond loopback with no auth token/password and \
             no trusted proxy — set gateway.auth.token (or bind loopback)"
                .to_string(),
        )
    }
}

/// Reject a no-auth Tailscale exposure: serving over Tailscale without any
/// shared secret is refused (device identity alone is not sufficient).
pub fn reject_noauth_tailscale_exposure(
    tailscale_enabled: bool,
    has_token: bool,
    has_password: bool,
) -> Result<(), String> {
    if tailscale_enabled && !has_token && !has_password {
        Err("refusing Tailscale exposure without gateway.auth token or password".to_string())
    } else {
        Ok(())
    }
}

// ============================================================================
// Forged loopback origin rejection (v2026.7.1)
// ============================================================================

/// True when `origin` claims to be loopback (localhost/127.x/[::1]).
pub fn origin_claims_loopback(origin: &str) -> bool {
    let lower = origin.to_ascii_lowercase();
    let host = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .or_else(|| lower.strip_prefix("ws://"))
        .or_else(|| lower.strip_prefix("wss://"))
        .unwrap_or(&lower);
    let host = host.split('/').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1"
        || host.starts_with("127.")
}

/// Reject connections whose Origin header claims loopback while the actual
/// peer address is not loopback (forged loopback origin).
pub fn is_forged_loopback_origin(origin: &str, peer: IpAddr) -> bool {
    origin_claims_loopback(origin) && !peer.is_loopback()
}

// ============================================================================
// Malformed HTTP/WS request-target rejection (GHSA-6hc3-f4rg-377m)
// ============================================================================

/// Validate an HTTP/WS request target path. Rejects targets that are not
/// origin-form, contain whitespace/control characters, backslashes, raw
/// non-ASCII bytes, or traversal segments.
pub fn validate_request_target(target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("empty request target".to_string());
    }
    if !target.starts_with('/') {
        return Err("request target must be origin-form (start with '/')".to_string());
    }
    if target.len() > 8192 {
        return Err("request target too long".to_string());
    }
    for c in target.chars() {
        if c.is_ascii_control() || c == ' ' || c == '\\' || !c.is_ascii() {
            return Err(format!("request target contains forbidden character {c:?}"));
        }
    }
    // Path traversal (raw or percent-encoded)
    let lower = target.to_ascii_lowercase();
    if lower.contains("..") || lower.contains("%2e%2e") || lower.contains("%00") {
        return Err("request target contains traversal or null sequence".to_string());
    }
    Ok(())
}

// ============================================================================
// Trusted package sources: lookalike sibling rejection (v2026.7.1)
// ============================================================================

/// True when `candidate` is a lookalike *sibling* of a trusted root — i.e.
/// it shares the trusted root as a string prefix without being contained in
/// it (`/opt/plugins-evil` vs trusted `/opt/plugins`).
pub fn is_lookalike_sibling(trusted_root: &Path, candidate: &Path) -> bool {
    if candidate.starts_with(trusted_root) {
        return false; // properly contained
    }
    let root_str = trusted_root.to_string_lossy();
    let cand_str = candidate.to_string_lossy();
    cand_str.starts_with(root_str.as_ref()) && cand_str.len() > root_str.len()
}

/// True when `candidate` is inside `trusted_root` by path components (the
/// only acceptable containment — string-prefix matches are rejected).
pub fn is_trusted_source(trusted_root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(trusted_root)
}

// ============================================================================
// Redirect auth-forwarding policy (v2026.7.1)
// ============================================================================

/// Whether auth headers may be forwarded to `redirect_url` after starting at
/// `original_url`: scheme + host + port must all match exactly.
pub fn should_forward_auth_on_redirect(original_url: &str, redirect_url: &str) -> bool {
    let parse = |u: &str| -> Option<(String, String, u16)> {
        let parsed = url::Url::parse(u).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let port = parsed.port_or_known_default()?;
        Some((parsed.scheme().to_ascii_lowercase(), host, port))
    };
    match (parse(original_url), parse(redirect_url)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

// ============================================================================
// browser.proxy node admin requirement (v2026.7.1)
// ============================================================================

/// `browser.proxy`-capable nodes require `operator.admin` on the invoking
/// connection.
pub fn node_invoke_requires_admin(node_kind_or_command: &str) -> bool {
    node_kind_or_command == "browser.proxy"
        || node_kind_or_command.starts_with("browser.proxy.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // ---- rate limiter ----

    #[test]
    fn rate_limiter_blocks_after_max() {
        let rl = SlidingWindowRateLimiter::new(3, Duration::from_secs(60));
        assert!(rl.check_and_record("1.2.3.4"));
        assert!(rl.check_and_record("1.2.3.4"));
        assert!(rl.check_and_record("1.2.3.4"));
        assert!(!rl.check_and_record("1.2.3.4"));
        // Other peers unaffected
        assert!(rl.check_and_record("5.6.7.8"));
    }

    #[test]
    fn rate_limiter_window_expiry() {
        let rl = SlidingWindowRateLimiter::new(1, Duration::from_millis(10));
        assert!(rl.check_and_record("k"));
        assert!(!rl.check_and_record("k"));
        std::thread::sleep(Duration::from_millis(15));
        assert!(rl.check_and_record("k"));
    }

    #[test]
    fn loopback_exemption() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        assert!(!auth_rate_limit_applies(lo, true));
        assert!(auth_rate_limit_applies(lo, false));
        assert!(auth_rate_limit_applies(remote, true));
    }

    // ---- fail-closed start ----

    #[test]
    fn nonloopback_start_requires_secret() {
        assert!(require_auth_for_nonloopback(true, false, false, false).is_ok());
        assert!(require_auth_for_nonloopback(false, true, false, false).is_ok());
        assert!(require_auth_for_nonloopback(false, false, true, false).is_ok());
        assert!(require_auth_for_nonloopback(false, false, false, true).is_ok());
        assert!(require_auth_for_nonloopback(false, false, false, false).is_err());
    }

    #[test]
    fn tailscale_noauth_rejected() {
        assert!(reject_noauth_tailscale_exposure(true, false, false).is_err());
        assert!(reject_noauth_tailscale_exposure(true, true, false).is_ok());
        assert!(reject_noauth_tailscale_exposure(false, false, false).is_ok());
    }

    // ---- forged loopback origin ----

    #[test]
    fn loopback_origin_detection() {
        assert!(origin_claims_loopback("http://localhost:3000"));
        assert!(origin_claims_loopback("https://127.0.0.1"));
        assert!(origin_claims_loopback("http://127.8.9.1:8080"));
        assert!(!origin_claims_loopback("https://mylobster.ai"));
        assert!(!origin_claims_loopback("http://localhost.evil.com"));
    }

    #[test]
    fn forged_loopback_origin_rejected() {
        let remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(is_forged_loopback_origin("http://localhost:3000", remote));
        assert!(!is_forged_loopback_origin("http://localhost:3000", lo));
        assert!(!is_forged_loopback_origin("https://app.example.com", remote));
    }

    // ---- request target ----

    #[test]
    fn valid_targets_accepted() {
        assert!(validate_request_target("/ws").is_ok());
        assert!(validate_request_target("/api/chat?token=abc").is_ok());
        assert!(validate_request_target("/v1/chat/completions").is_ok());
    }

    #[test]
    fn malformed_targets_rejected() {
        assert!(validate_request_target("").is_err());
        assert!(validate_request_target("ws://evil/ws").is_err()); // absolute-form
        assert!(validate_request_target("/a b").is_err()); // whitespace
        assert!(validate_request_target("/a\\b").is_err()); // backslash
        assert!(validate_request_target("/a\r\nHost:evil").is_err()); // CRLF
        assert!(validate_request_target("/../etc").is_err()); // traversal
        assert!(validate_request_target("/%2e%2e/x").is_err()); // encoded traversal
        assert!(validate_request_target("/caf\u{e9}").is_err()); // non-ASCII
    }

    // ---- lookalike siblings ----

    #[test]
    fn lookalike_sibling_detection() {
        let root = Path::new("/opt/plugins");
        assert!(is_lookalike_sibling(root, Path::new("/opt/plugins-evil")));
        assert!(is_lookalike_sibling(root, Path::new("/opt/pluginsX/pkg")));
        assert!(!is_lookalike_sibling(root, Path::new("/opt/plugins/pkg")));
        assert!(!is_lookalike_sibling(root, Path::new("/opt/other")));
        assert!(!is_lookalike_sibling(root, Path::new("/opt/plugins")));
    }

    #[test]
    fn trusted_source_uses_components_not_string_prefix() {
        let root = Path::new("/opt/plugins");
        assert!(is_trusted_source(root, Path::new("/opt/plugins/pkg")));
        assert!(!is_trusted_source(root, Path::new("/opt/plugins-evil/pkg")));
    }

    // ---- redirect auth ----

    #[test]
    fn same_origin_redirect_forwards_auth() {
        assert!(should_forward_auth_on_redirect(
            "https://api.example.com/a",
            "https://api.example.com/b"
        ));
        // Default ports normalize
        assert!(should_forward_auth_on_redirect(
            "https://api.example.com/a",
            "https://api.example.com:443/b"
        ));
    }

    #[test]
    fn cross_origin_redirect_strips_auth() {
        assert!(!should_forward_auth_on_redirect(
            "https://api.example.com/a",
            "https://evil.com/b"
        ));
        assert!(!should_forward_auth_on_redirect(
            "https://api.example.com/a",
            "http://api.example.com/b" // scheme downgrade
        ));
        assert!(!should_forward_auth_on_redirect(
            "https://api.example.com/a",
            "https://api.example.com:8443/b" // port change
        ));
        assert!(!should_forward_auth_on_redirect("not a url", "https://x.com"));
    }

    // ---- node admin ----

    #[test]
    fn browser_proxy_requires_admin() {
        assert!(node_invoke_requires_admin("browser.proxy"));
        assert!(node_invoke_requires_admin("browser.proxy.request"));
        assert!(!node_invoke_requires_admin("canvas.render"));
    }
}
