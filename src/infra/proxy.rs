//! Proxy-backed fetch infrastructure (v2026.7.1 parity).
//!
//! Ports the behavior of upstream `src/infra/net/proxy-fetch.ts`:
//! - resolve a trusted HTTP(S) proxy from standard environment variables
//!   (malformed URLs resolve to no proxy rather than erroring);
//! - build a proxy-backed HTTP client;
//! - normalize multipart requests before proxy-backed fetches: the HTTP
//!   client must own the multipart boundary, so caller-supplied
//!   `Content-Type` / `Content-Length` headers are dropped (upstream rebuilds
//!   standard FormData bodies as undici FormData for the same reason —
//!   forwarding a stale boundary corrupts audio-transcription uploads).

use reqwest::header::{HeaderMap, CONTENT_LENGTH, CONTENT_TYPE};

/// Proxy environment variables in resolution order (uppercase preferred).
const PROXY_ENV_VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];

/// Resolve a proxy URL from the standard environment variables.
///
/// Malformed values are ignored (upstream `resolveProxyFetchFromEnv`
/// gracefully returns undefined). Only http(s) proxies are accepted.
pub fn resolve_proxy_url_from_env(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    for var in PROXY_ENV_VARS {
        let Some(raw) = env(var) else { continue };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        match url::Url::parse(trimmed) {
            Ok(url) if url.scheme() == "http" || url.scheme() == "https" => {
                return Some(trimmed.to_string());
            }
            _ => continue,
        }
    }
    None
}

/// `NO_PROXY` / `no_proxy` exclusion check: exact host match or dot-suffix
/// match; `*` disables proxying entirely.
pub fn host_bypasses_proxy(host: &str, no_proxy: Option<&str>) -> bool {
    let Some(no_proxy) = no_proxy else { return false };
    let host = host.to_ascii_lowercase();
    for entry in no_proxy.split(',') {
        let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" || host == entry || host.ends_with(&format!(".{entry}")) {
            return true;
        }
    }
    false
}

/// Build a reqwest client routed through the given proxy.
pub fn make_proxy_client(proxy_url: &str) -> anyhow::Result<reqwest::Client> {
    let proxy = reqwest::Proxy::all(proxy_url)?;
    Ok(reqwest::Client::builder().proxy(proxy).build()?)
}

// ============================================================================
// Loopback routing (v2026.7.1 `proxy.loopbackMode`)
// ============================================================================

/// How loopback targets route when a managed proxy is active (upstream
/// `proxy.loopbackMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyLoopbackMode {
    /// Default: only the gateway process may reach loopback directly; the
    /// request bypasses the proxy.
    #[default]
    GatewayOnly,
    /// Loopback targets go through the proxy like everything else.
    Proxy,
    /// Loopback targets are refused outright.
    Block,
}

impl ProxyLoopbackMode {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("proxy") => ProxyLoopbackMode::Proxy,
            Some("block") => ProxyLoopbackMode::Block,
            _ => ProxyLoopbackMode::GatewayOnly,
        }
    }
}

/// Routing decision for one request under an active managed proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyRoute {
    /// Send directly (no proxy).
    Direct,
    /// Send through the managed proxy.
    Proxied,
    /// Refuse the request.
    Blocked,
}

fn is_loopback_host(host: &str) -> bool {
    let bare = host
        .trim()
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Resolve the proxy route for a URL under an active managed proxy
/// (v2026.7.1). Loopback targets follow `loopbackMode`; everything else is
/// proxied unless NO_PROXY matches (see [`matches_no_proxy`]). Local
/// CDP/DevTools endpoints are loopback targets, so gateway-only mode keeps
/// browser control working without tunneling debugger traffic through the
/// proxy.
pub fn resolve_proxy_route(
    url: &url::Url,
    loopback_mode: ProxyLoopbackMode,
    no_proxy: Option<&str>,
) -> ProxyRoute {
    let host = url.host_str().unwrap_or("");
    if is_loopback_host(host) {
        return match loopback_mode {
            ProxyLoopbackMode::GatewayOnly => ProxyRoute::Direct,
            ProxyLoopbackMode::Proxy => ProxyRoute::Proxied,
            ProxyLoopbackMode::Block => ProxyRoute::Blocked,
        };
    }
    if matches_no_proxy(url, no_proxy) {
        return ProxyRoute::Direct;
    }
    ProxyRoute::Proxied
}

// ============================================================================
// Retry-After (v2026.7.1)
// ============================================================================

/// Parse a `Retry-After` header value into a delay (v2026.7.1: proxy-backed
/// fetches honor server backpressure). Accepts delta-seconds or an HTTP-date;
/// past dates resolve to zero. Malformed values return `None`.
pub fn parse_retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let parsed = chrono::DateTime::parse_from_rfc2822(trimmed).ok()?;
    let delta = parsed.with_timezone(&chrono::Utc) - now;
    Some(std::time::Duration::from_secs(delta.num_seconds().max(0) as u64))
}

// ============================================================================
// NO_PROXY matching (v2026.7.1 full undici-parity semantics)
// ============================================================================

fn parse_ipv4_u32(host: &str) -> Option<u32> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut value: u32 = 0;
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let octet: u32 = part.parse().ok()?;
        if octet > 255 {
            return None;
        }
        value = (value << 8) | octet;
    }
    Some(value)
}

fn matches_ipv4_no_proxy_pattern(target_host: &str, entry_host: &str) -> bool {
    let Some(target) = parse_ipv4_u32(target_host) else {
        return false;
    };

    // CIDR entry: `100.64.0.0/10`.
    if let Some((network_str, prefix_str)) = entry_host.split_once('/') {
        let (Some(network), Ok(prefix)) = (parse_ipv4_u32(network_str), prefix_str.parse::<u32>())
        else {
            return false;
        };
        if prefix > 32 {
            return false;
        }
        let mask: u32 = if prefix == 0 { 0 } else { (u32::MAX) << (32 - prefix) };
        return (target & mask) == (network & mask);
    }

    // Octet-wildcard entry: `100.64.*`.
    if !entry_host.contains('*') {
        return false;
    }
    let target_parts: Vec<&str> = target_host.split('.').collect();
    let pattern_parts: Vec<&str> = entry_host.split('.').collect();
    if pattern_parts.is_empty() || pattern_parts.len() > 4 {
        return false;
    }
    for (index, part) in pattern_parts.iter().enumerate() {
        if *part == "*" {
            if index == pattern_parts.len() - 1 {
                return true;
            }
            continue;
        }
        let target_part = target_parts.get(index).copied().unwrap_or("");
        let (Ok(pattern_num), Ok(target_num)) = (part.parse::<u32>(), target_part.parse::<u32>())
        else {
            return false;
        };
        if part.len() > 3 || pattern_num != target_num {
            return false;
        }
    }
    pattern_parts.len() == target_parts.len()
}

/// Check whether a target URL should bypass the proxy per `NO_PROXY` /
/// `no_proxy` (v2026.7.1, mirrors undici `EnvHttpProxyAgent` semantics plus
/// the OpenClaw IPv4 CIDR / octet-wildcard extension):
/// - entries split on commas AND whitespace, case-insensitive;
/// - bare `*` bypasses everything;
/// - exact host, leading-dot (`.example.com`), `*.` wildcard, and
///   subdomain-suffix matches (apex also matches wildcard entries);
/// - optional `:port` must match the target port (with protocol defaults);
/// - IPv6 literals in bracketed (`[::1]`) or bare (`::1`) form;
/// - IPv4 CIDR (`100.64.0.0/10`) and octet wildcards (`100.64.*`).
pub fn matches_no_proxy(url: &url::Url, no_proxy: Option<&str>) -> bool {
    let Some(raw) = no_proxy.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };

    let target_host = url
        .host_str()
        .unwrap_or("")
        .to_ascii_lowercase()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if target_host.is_empty() {
        return false;
    }
    if raw == "*" {
        return true;
    }

    let target_port = match url.port() {
        Some(p) => p.to_string(),
        None => match url.scheme() {
            "https" => "443".to_string(),
            "http" => "80".to_string(),
            _ => String::new(),
        },
    };

    for raw_entry in raw.split([',', ' ', '\t', '\n', '\r']) {
        let entry = raw_entry.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        let (entry_host, entry_port): (String, Option<String>) = if entry.starts_with('[') {
            // Bracketed IPv6 with optional port: `[::1]:8080`.
            let Some(close) = entry.find(']') else { continue };
            let host = entry[1..close].to_string();
            let port = entry[close + 1..]
                .strip_prefix(':')
                .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                .map(String::from);
            (host, port)
        } else {
            let first_colon = entry.find(':');
            let last_colon = entry.rfind(':');
            match (first_colon, last_colon) {
                (Some(first), Some(last))
                    if first == last
                        && entry[last + 1..].chars().all(|c| c.is_ascii_digit())
                        && !entry[last + 1..].is_empty() =>
                {
                    (entry[..last].to_string(), Some(entry[last + 1..].to_string()))
                }
                _ => (entry.clone(), None),
            }
        };

        if let Some(port) = &entry_port {
            if *port != target_port {
                continue;
            }
        }

        // Mirror undici: `.example.com` and `*.example.com` both normalize to
        // `example.com`, and the apex host matches those entries too.
        let normalized = entry_host
            .strip_prefix("*.")
            .or_else(|| entry_host.strip_prefix('.'))
            .unwrap_or(&entry_host)
            .to_string();
        if normalized.is_empty() || normalized == "*" {
            continue;
        }

        if matches_ipv4_no_proxy_pattern(&target_host, &normalized) {
            return true;
        }
        if target_host == normalized {
            return true;
        }
        if target_host.ends_with(&format!(".{normalized}")) {
            return true;
        }
    }
    false
}

/// Read `no_proxy` / `NO_PROXY` with lowercase precedence (undici parity;
/// covers the "both casings" CDP/DevTools bypass requirement).
pub fn read_no_proxy_env(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    // Undici: a set-but-empty lowercase var intentionally shadows uppercase.
    if let Some(lower) = env("no_proxy") {
        let trimmed = lower.trim().to_string();
        return if trimmed.is_empty() { None } else { Some(trimmed) };
    }
    env("NO_PROXY")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Normalize caller-supplied headers for a multipart (FormData-style) body
/// before a proxy-backed fetch.
///
/// The proxy-backed client generates its own multipart boundary; forwarding
/// caller `Content-Type: multipart/form-data; boundary=...` or a stale
/// `Content-Length` would corrupt the request (audio transcription uploads
/// were the observed failure upstream). Non-multipart requests keep their
/// headers untouched.
pub fn normalize_multipart_headers(headers: &HeaderMap, body_is_multipart: bool) -> HeaderMap {
    if !body_is_multipart {
        return headers.clone();
    }
    let mut normalized = headers.clone();
    normalized.remove(CONTENT_TYPE);
    normalized.remove(CONTENT_LENGTH);
    normalized
}

/// True when a Content-Type header declares a multipart body — the signal
/// that the body must be rebuilt so the client owns the boundary.
pub fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().starts_with("multipart/"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn proxy_env_resolution_prefers_https_proxy() {
        let env = |var: &str| match var {
            "HTTPS_PROXY" => Some("http://secure-proxy:8080".to_string()),
            "HTTP_PROXY" => Some("http://plain-proxy:8080".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_proxy_url_from_env(env).as_deref(),
            Some("http://secure-proxy:8080")
        );
    }

    #[test]
    fn proxy_env_resolution_skips_malformed_urls() {
        let env = |var: &str| match var {
            "HTTPS_PROXY" => Some("not a url".to_string()),
            "HTTP_PROXY" => Some("http://fallback:3128".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_proxy_url_from_env(env).as_deref(),
            Some("http://fallback:3128")
        );
    }

    #[test]
    fn proxy_env_resolution_rejects_non_http_schemes() {
        let env = |var: &str| {
            (var == "HTTPS_PROXY").then(|| "socks5://proxy:1080".to_string())
        };
        assert_eq!(resolve_proxy_url_from_env(env), None);
    }

    #[test]
    fn proxy_env_resolution_returns_none_when_unset() {
        assert_eq!(resolve_proxy_url_from_env(|_| None), None);
    }

    #[test]
    fn no_proxy_matching() {
        assert!(host_bypasses_proxy("internal.corp", Some("internal.corp")));
        assert!(host_bypasses_proxy("api.internal.corp", Some(".internal.corp")));
        assert!(host_bypasses_proxy("api.internal.corp", Some("internal.corp")));
        assert!(host_bypasses_proxy("anything.example", Some("*")));
        assert!(!host_bypasses_proxy("internal.corp.evil.com", Some("internal.corp")));
        assert!(!host_bypasses_proxy("example.com", Some("internal.corp")));
        assert!(!host_bypasses_proxy("example.com", None));
    }

    #[test]
    fn multipart_headers_are_stripped_for_proxy_fetches() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=stale123"),
        );
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("4242"));
        headers.insert("authorization", HeaderValue::from_static("Bearer tok"));

        assert!(is_multipart_content_type(&headers));
        let normalized = normalize_multipart_headers(&headers, true);
        assert!(normalized.get(CONTENT_TYPE).is_none(), "stale boundary must be dropped");
        assert!(normalized.get(CONTENT_LENGTH).is_none());
        assert_eq!(
            normalized.get("authorization").unwrap(),
            "Bearer tok",
            "non-multipart headers survive"
        );
    }

    #[test]
    fn non_multipart_headers_are_untouched() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("2"));
        assert!(!is_multipart_content_type(&headers));
        let normalized = normalize_multipart_headers(&headers, false);
        assert_eq!(normalized.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(normalized.get(CONTENT_LENGTH).unwrap(), "2");
    }

    #[test]
    fn proxy_client_builds_for_valid_url() {
        assert!(make_proxy_client("http://127.0.0.1:3128").is_ok());
        assert!(make_proxy_client("not a url").is_err());
    }

    // ---- loopback mode (v2026.7.1) -----------------------------------------

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn loopback_mode_parsing_defaults_to_gateway_only() {
        assert_eq!(ProxyLoopbackMode::parse(None), ProxyLoopbackMode::GatewayOnly);
        assert_eq!(ProxyLoopbackMode::parse(Some("bogus")), ProxyLoopbackMode::GatewayOnly);
        assert_eq!(ProxyLoopbackMode::parse(Some("proxy")), ProxyLoopbackMode::Proxy);
        assert_eq!(ProxyLoopbackMode::parse(Some("block")), ProxyLoopbackMode::Block);
    }

    #[test]
    fn loopback_targets_follow_loopback_mode() {
        for target in ["http://localhost:9222/json", "http://127.0.0.1:8080/", "http://[::1]/"] {
            assert_eq!(
                resolve_proxy_route(&u(target), ProxyLoopbackMode::GatewayOnly, None),
                ProxyRoute::Direct,
                "{target} gateway-only"
            );
            assert_eq!(
                resolve_proxy_route(&u(target), ProxyLoopbackMode::Proxy, None),
                ProxyRoute::Proxied,
                "{target} proxy"
            );
            assert_eq!(
                resolve_proxy_route(&u(target), ProxyLoopbackMode::Block, None),
                ProxyRoute::Blocked,
                "{target} block"
            );
        }
    }

    #[test]
    fn non_loopback_targets_proxy_unless_no_proxy_matches() {
        assert_eq!(
            resolve_proxy_route(&u("https://example.com/"), ProxyLoopbackMode::GatewayOnly, None),
            ProxyRoute::Proxied
        );
        assert_eq!(
            resolve_proxy_route(
                &u("https://internal.corp/"),
                ProxyLoopbackMode::GatewayOnly,
                Some("internal.corp"),
            ),
            ProxyRoute::Direct
        );
    }

    // ---- Retry-After (v2026.7.1) -------------------------------------------

    fn test_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn retry_after_parses_delta_seconds() {
        assert_eq!(
            parse_retry_after("120", test_now()),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after("0", test_now()), Some(std::time::Duration::ZERO));
    }

    #[test]
    fn retry_after_parses_http_dates() {
        assert_eq!(
            parse_retry_after("Wed, 01 Jul 2026 12:01:30 GMT", test_now()),
            Some(std::time::Duration::from_secs(90))
        );
        // Past dates clamp to zero rather than going negative.
        assert_eq!(
            parse_retry_after("Wed, 01 Jul 2026 11:00:00 GMT", test_now()),
            Some(std::time::Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_rejects_malformed_values() {
        assert_eq!(parse_retry_after("", test_now()), None);
        assert_eq!(parse_retry_after("soon", test_now()), None);
        assert_eq!(parse_retry_after("-5", test_now()), None);
    }

    // ---- NO_PROXY matching (v2026.7.1) -------------------------------------

    #[test]
    fn no_proxy_exact_and_subdomain_matches() {
        let np = Some("example.com");
        assert!(matches_no_proxy(&u("https://example.com/"), np));
        assert!(matches_no_proxy(&u("https://api.example.com/"), np));
        assert!(!matches_no_proxy(&u("https://example.com.evil.net/"), np));
        assert!(!matches_no_proxy(&u("https://other.net/"), np));
    }

    #[test]
    fn no_proxy_wildcard_and_leading_dot_match_apex_too() {
        for entry in [".example.com", "*.example.com"] {
            assert!(matches_no_proxy(&u("https://foo.example.com/"), Some(entry)), "{entry}");
            // Undici normalization means the apex matches as well.
            assert!(matches_no_proxy(&u("https://example.com/"), Some(entry)), "{entry}");
        }
        assert!(matches_no_proxy(&u("https://anything.net/"), Some("*")));
    }

    #[test]
    fn no_proxy_splits_on_commas_and_whitespace() {
        let np = Some("localhost *.corp,\tinternal.net");
        assert!(matches_no_proxy(&u("http://localhost/"), np));
        assert!(matches_no_proxy(&u("https://api.corp/"), np));
        assert!(matches_no_proxy(&u("https://x.internal.net/"), np));
        assert!(!matches_no_proxy(&u("https://example.com/"), np));
    }

    #[test]
    fn no_proxy_port_entries_must_match_target_port() {
        assert!(matches_no_proxy(&u("https://example.com/"), Some("example.com:443")));
        assert!(!matches_no_proxy(&u("https://example.com/"), Some("example.com:8443")));
        assert!(matches_no_proxy(&u("http://example.com:8080/"), Some("example.com:8080")));
        assert!(!matches_no_proxy(&u("http://example.com/"), Some("example.com:8080")));
    }

    #[test]
    fn no_proxy_ipv6_bracketed_and_bare() {
        assert!(matches_no_proxy(&u("http://[::1]:9222/"), Some("::1")));
        assert!(matches_no_proxy(&u("http://[::1]:9222/"), Some("[::1]")));
        assert!(matches_no_proxy(&u("http://[::1]:9222/"), Some("[::1]:9222")));
        assert!(!matches_no_proxy(&u("http://[::1]:9222/"), Some("[::1]:9333")));
        assert!(!matches_no_proxy(&u("http://[fd00::1]/"), Some("::1")));
    }

    #[test]
    fn no_proxy_ipv4_cidr_extension() {
        // Fake-IP proxy stacks: CGNAT range bypass.
        let np = Some("100.64.0.0/10");
        assert!(matches_no_proxy(&u("http://100.64.0.1/"), np));
        assert!(matches_no_proxy(&u("http://100.127.255.254/"), np));
        assert!(!matches_no_proxy(&u("http://100.128.0.1/"), np));
        assert!(!matches_no_proxy(&u("http://10.0.0.1/"), np));
        // RFC 2544 benchmark range.
        assert!(matches_no_proxy(&u("http://198.18.5.5/"), Some("198.18.0.0/15")));
        // /0 matches everything IPv4.
        assert!(matches_no_proxy(&u("http://8.8.8.8/"), Some("0.0.0.0/0")));
        // Malformed CIDR entries never match.
        assert!(!matches_no_proxy(&u("http://8.8.8.8/"), Some("8.8.8.8/33")));
    }

    #[test]
    fn no_proxy_ipv4_octet_wildcards() {
        let np = Some("100.64.*");
        assert!(matches_no_proxy(&u("http://100.64.3.7/"), np));
        assert!(!matches_no_proxy(&u("http://100.65.3.7/"), np));
        // Wildcard entries only apply to IPv4 targets.
        assert!(!matches_no_proxy(&u("https://host.example/"), np));
        // Full-length pattern with non-final wildcard.
        assert!(matches_no_proxy(&u("http://10.1.2.3/"), Some("10.*.2.3")));
        assert!(!matches_no_proxy(&u("http://10.1.9.3/"), Some("10.*.2.3")));
    }

    #[test]
    fn no_proxy_env_reader_prefers_lowercase_and_shadows() {
        let both = |var: &str| match var {
            "no_proxy" => Some("lower.example".to_string()),
            "NO_PROXY" => Some("upper.example".to_string()),
            _ => None,
        };
        assert_eq!(read_no_proxy_env(both).as_deref(), Some("lower.example"));

        // Empty lowercase shadows uppercase entirely (undici semantics).
        let shadowed = |var: &str| match var {
            "no_proxy" => Some("".to_string()),
            "NO_PROXY" => Some("upper.example".to_string()),
            _ => None,
        };
        assert_eq!(read_no_proxy_env(shadowed), None);

        let upper_only =
            |var: &str| (var == "NO_PROXY").then(|| "upper.example".to_string());
        assert_eq!(read_no_proxy_env(upper_only).as_deref(), Some("upper.example"));
    }

    #[test]
    fn local_cdp_devtools_bypass_via_either_casing() {
        // The local CDP/DevTools endpoint must bypass the proxy whether the
        // operator sets no_proxy or NO_PROXY.
        let cdp = u("http://127.0.0.1:9222/json/version");
        for source in [
            read_no_proxy_env(|v| (v == "no_proxy").then(|| "127.0.0.1".to_string())),
            read_no_proxy_env(|v| (v == "NO_PROXY").then(|| "127.0.0.1".to_string())),
        ] {
            assert!(matches_no_proxy(&cdp, source.as_deref()));
        }
        // And gateway-only loopback mode bypasses even without NO_PROXY.
        assert_eq!(
            resolve_proxy_route(&cdp, ProxyLoopbackMode::GatewayOnly, None),
            ProxyRoute::Direct
        );
    }
}
