use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;
use tracing::{debug, warn};
use url::Url;

/// Web fetch tool with SSRF protection.
pub struct WebFetchTool;

#[async_trait::async_trait]
impl AgentTool for WebFetchTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "web_fetch".to_string(),
            description: "Fetch content from a URL with SSRF protection".to_string(),
            category: "web".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "method": { "type": "string", "enum": ["GET", "POST"], "default": "GET" },
                    "headers": { "type": "object" },
                    "body": { "type": "string" },
                    "maxChars": { "type": "integer" }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let url_str = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing url parameter"))?;

        let method = params
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET");

        let max_chars = params
            .get("maxChars")
            .and_then(|v| v.as_u64())
            .unwrap_or(200_000) as usize;

        // SSRF protection — first the static URL/hostname check. The policy
        // carries the v2026.7.1 opt-in escapes (RFC 2544 / IPv6 ULA) for
        // trusted fake-IP proxy stacks.
        let ssrf_policy = SsrfPolicy::from_config(&context.config);
        let url = Url::parse(url_str)?;
        if is_ssrf_target_with_policy(&url, &ssrf_policy) {
            return Ok(ToolResult::error(
                "URL targets a private/internal address (SSRF protection)",
            ));
        }

        // v2026.7.1: `tools.web.fetch.useTrustedEnvProxy` routes the fetch
        // through a trusted HTTP(S) env proxy which then resolves DNS itself.
        // The guarded static SSRF check above always runs BEFORE any
        // proxy/DNS work (guarded fetch before managed-proxy DNS); the local
        // DNS re-check is skipped only when the proxy actually applies to
        // this target (NO_PROXY-exempt targets keep the local re-check).
        let use_trusted_env_proxy = context
            .config
            .tools
            .web
            .fetch
            .as_ref()
            .and_then(|f| f.use_trusted_env_proxy)
            .unwrap_or(false);
        let env_proxy_url = if use_trusted_env_proxy {
            crate::infra::proxy::resolve_proxy_url_from_env(|var| std::env::var(var).ok())
        } else {
            None
        };
        let no_proxy = crate::infra::proxy::read_no_proxy_env(|var| std::env::var(var).ok());
        let proxied = env_proxy_url.is_some()
            && !crate::infra::proxy::matches_no_proxy(&url, no_proxy.as_deref());

        // Dynamic DNS-resolution check: if the hostname resolves to any
        // private/internal IP, block the request. Defends against DNS rebinding
        // and against hostnames that resolve to RFC1918 / link-local space.
        // Skipped when the trusted env proxy owns DNS resolution for this
        // target.
        if !proxied {
            if let Some(host) = url.host_str() {
                // Skip when the host is already an IP literal — the static check
                // above already validated it. Only re-resolve names.
                if host.parse::<std::net::IpAddr>().is_err()
                    && hostname_resolves_to_private_ip_with_policy(host, &ssrf_policy).await
                {
                    return Ok(ToolResult::error(
                        "URL hostname resolves to a private/internal address (SSRF protection)",
                    ));
                }
            }
        }

        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(3))
            .timeout(std::time::Duration::from_secs(10));
        if let Some(proxy_url) = env_proxy_url.as_deref().filter(|_| proxied) {
            client_builder = client_builder.proxy(reqwest::Proxy::all(proxy_url)?);
        }
        let client = client_builder.build()?;

        let mut request = match method.to_uppercase().as_str() {
            "POST" => client.post(url_str),
            _ => client.get(url_str),
        };

        // Apply custom headers
        if let Some(headers) = params.get("headers").and_then(|v| v.as_object()) {
            let mut header_map = HeaderMap::new();
            for (key, value) in headers {
                if let Some(val_str) = value.as_str() {
                    if let (Ok(name), Ok(val)) =
                        (HeaderName::from_str(key), HeaderValue::from_str(val_str))
                    {
                        header_map.insert(name, val);
                    }
                }
            }
            request = request.headers(header_map);
        }

        // Apply body
        if let Some(body) = params.get("body").and_then(|v| v.as_str()) {
            request = request.body(body.to_string());
        }

        let response = request.send().await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain")
            .to_string();

        // Log Cloudflare markdown token count if present
        if let Some(md_tokens) = response
            .headers()
            .get("x-markdown-tokens")
            .and_then(|v| v.to_str().ok())
        {
            debug!("Cloudflare x-markdown-tokens: {}", md_tokens);
        }

        // v2026.4.1: configurable maxResponseBytes truncation limit
        let max_bytes = context
            .config
            .tools
            .web
            .fetch
            .as_ref()
            .and_then(|f| f.max_response_bytes)
            .unwrap_or(1_048_576); // 1MB default

        // v2026.7.1: bounded body reads — stream the body and stop pulling
        // chunks once the cap is reached instead of buffering an unbounded
        // response before truncating.
        let (raw_bytes, body_truncated) = read_body_bounded(response, max_bytes as usize).await?;
        if body_truncated {
            warn!(
                "Response body truncated at {} bytes (maxResponseBytes)",
                max_bytes
            );
        }
        let body = String::from_utf8_lossy(&raw_bytes).into_owned();

        // Process content based on content-type
        let (text, extract_mode) = if content_type.contains("text/markdown") {
            // Cloudflare Markdown for Agents — already pre-rendered markdown
            (body, "markdown")
        } else if content_type.contains("application/json") {
            // Pretty-print JSON for readability
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(parsed) => {
                    let pretty =
                        serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| body.clone());
                    (pretty, "json")
                }
                Err(_) => (body, "raw"),
            }
        } else {
            (body, "raw")
        };

        // Truncate if needed. v2026.7.1: the full text is spilled to a file
        // so the model (or operator) can read the untruncated content instead
        // of losing it.
        let mut spilled_path: Option<String> = None;
        let text = if text.len() > max_chars {
            spilled_path = spill_full_content_to_file(&text).await;
            let cut = truncate_on_char_boundary(&text, max_chars);
            format!("{}... (truncated, {} chars total)", cut, text.len())
        } else {
            text
        };

        let mut result = serde_json::json!({
            "status": status.as_u16(),
            "contentType": content_type,
            "extractMode": extract_mode,
            "text": text
        });
        if let Some(path) = spilled_path {
            result["truncated"] = serde_json::json!(true);
            result["fullContentPath"] = serde_json::json!(path);
        }
        Ok(ToolResult::json(result))
    }
}

/// Cut a string at (or just before) `max_bytes` without splitting a UTF-8
/// code point.
fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Stream a response body up to `max_bytes`; returns `(bytes, truncated)`.
/// Stops pulling network chunks once the cap is reached (v2026.7.1 bounded
/// body reads).
async fn read_body_bounded(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() >= max_bytes {
            let room = max_bytes - out.len();
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, true));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, false))
}

/// Write the full (pre-truncation) fetched text to a spill file and return
/// its path. Failures are non-fatal — the truncated inline text stands alone.
async fn spill_full_content_to_file(text: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("mylobster-webfetch");
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return None;
    }
    let name = format!(
        "webfetch-{}-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );
    let path = dir.join(name);
    match tokio::fs::write(&path, text).await {
        Ok(()) => Some(path.to_string_lossy().into_owned()),
        Err(_) => None,
    }
}

/// Check if a URL targets a private/internal address.
pub(crate) fn is_ssrf_target(url: &Url) -> bool {
    is_ssrf_target_with_policy(url, &SsrfPolicy::default())
}

/// SSRF policy escapes (v2026.7.1): opt-in allowances for trusted fake-IP
/// proxy stacks (sing-box, Clash, Surge) that resolve foreign domains into
/// reserved ranges. Mirrors upstream `tools.web.fetch.ssrfPolicy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SsrfPolicy {
    /// Allow the RFC 2544 benchmark range (198.18.0.0/15).
    pub allow_rfc2544_benchmark_range: bool,
    /// Allow IPv6 Unique Local Addresses (fc00::/7).
    pub allow_ipv6_unique_local_range: bool,
}

impl SsrfPolicy {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let policy = config
            .tools
            .web
            .fetch
            .as_ref()
            .and_then(|f| f.ssrf_policy.as_ref());
        Self {
            allow_rfc2544_benchmark_range: policy
                .and_then(|p| p.allow_rfc2544_benchmark_range)
                .unwrap_or(false),
            allow_ipv6_unique_local_range: policy
                .and_then(|p| p.allow_ipv6_unique_local_range)
                .unwrap_or(false),
        }
    }
}

/// Policy-aware variant of [`is_ssrf_target`].
pub(crate) fn is_ssrf_target_with_policy(url: &Url, policy: &SsrfPolicy) -> bool {
    // Block non-HTTP schemes
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }

    if let Some(host) = url.host_str() {
        // Block localhost variants
        if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" {
            return true;
        }

        // Block .localhost suffix (e.g. foo.localhost)
        let lower = host.to_lowercase();
        if lower.ends_with(".localhost") {
            return true;
        }

        // Block private IP ranges. host_str() returns IPv6 in bracketed form
        // (e.g. "[fc00::1]") per RFC 3986; strip the brackets before parsing.
        let ip_str = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
            return is_private_ip_with_policy(ip, policy);
        }

        // Block common internal hostnames
        if lower.ends_with(".internal")
            || lower.ends_with(".local")
            || lower.ends_with(".svc.cluster.local")
            || lower == "metadata.google.internal"
        {
            return true;
        }

        // Block cloud metadata endpoints
        if host == "169.254.169.254" || host == "metadata.google.internal" {
            return true;
        }
    }

    false
}

/// Extract an embedded IPv4 address from IPv6 transition mechanism addresses.
///
/// Supports: NAT64 (64:ff9b::/96 and 64:ff9b:1::/48), 6to4 (2002::/16),
/// Teredo (2001:0000::/32), and ISATAP (IID marker 0000:5efe).
fn extract_ipv6_embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let segments = v6.segments();
    let octets128 = v6.octets();

    // NAT64 well-known prefix (64:ff9b::/96) — IPv4 in last 32 bits
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0
    {
        return Some(std::net::Ipv4Addr::new(
            octets128[12],
            octets128[13],
            octets128[14],
            octets128[15],
        ));
    }

    // NAT64 local-use prefix (64:ff9b:1::/48) — IPv4 in last 32 bits
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001 {
        return Some(std::net::Ipv4Addr::new(
            octets128[12],
            octets128[13],
            octets128[14],
            octets128[15],
        ));
    }

    // 6to4 (2002::/16) — IPv4 embedded in bits 16–47 (segments[1] and segments[2])
    if segments[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            (segments[1] & 0xff) as u8,
            (segments[2] >> 8) as u8,
            (segments[2] & 0xff) as u8,
        ));
    }

    // Teredo (2001:0000::/32) — IPv4 server in segments[2..3], client in XOR of segments[6..7]
    if segments[0] == 0x2001 && segments[1] == 0x0000 {
        // Server address (segments 2-3)
        let server = std::net::Ipv4Addr::new(
            (segments[2] >> 8) as u8,
            (segments[2] & 0xff) as u8,
            (segments[3] >> 8) as u8,
            (segments[3] & 0xff) as u8,
        );
        // Client address — XOR of hextets 6-7 with 0xffff
        let client = std::net::Ipv4Addr::new(
            ((segments[6] ^ 0xffff) >> 8) as u8,
            ((segments[6] ^ 0xffff) & 0xff) as u8,
            ((segments[7] ^ 0xffff) >> 8) as u8,
            ((segments[7] ^ 0xffff) & 0xff) as u8,
        );
        // Check both: if either is private, return it for blocking
        if is_private_ipv4(&server) {
            return Some(server);
        }
        return Some(client);
    }

    // ISATAP — IID marker 0000:5efe in segments[5..6], IPv4 in last 32 bits
    if segments[5] == 0x0000 && segments[6] == 0x5efe {
        return Some(std::net::Ipv4Addr::new(
            octets128[12],
            octets128[13],
            octets128[14],
            octets128[15],
        ));
    }

    None
}

/// Check if an IPv4 address is private/internal.
fn is_private_ipv4(v4: &std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        // Unspecified (0.0.0.0/8)
        || octets[0] == 0
        // Link-local / APIPA (169.254.0.0/16)
        || (octets[0] == 169 && octets[1] == 254)
        // Carrier-grade NAT (100.64.0.0/10)
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        // Broadcast (255.255.255.255)
        || (octets[0] == 255 && octets[1] == 255 && octets[2] == 255 && octets[3] == 255)
        // Multicast (224.0.0.0/4)
        || (octets[0] >= 224 && octets[0] <= 239)
        // Reserved (240.0.0.0/4, excluding 255.255.255.255 already covered)
        || (octets[0] >= 240)
        // Benchmarking (198.18.0.0/15)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        // TEST-NET-1 (192.0.2.0/24)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        // TEST-NET-2 (198.51.100.0/24)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        // TEST-NET-3 (203.0.113.0/24)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

/// Check if an IP address is private/internal or a blocked special-use address.
///
/// This covers both RFC 1918 private ranges and special-use addresses
/// (multicast, link-local, benchmarking, etc.) for SSRF protection.
/// Named for parity with OpenClaw's `isBlockedSpecialUseAddress`.
pub(crate) fn is_private_ip(ip: std::net::IpAddr) -> bool {
    is_private_ip_with_policy(ip, &SsrfPolicy::default())
}

/// Policy-aware SSRF IP check. `allow_ipv6_unique_local_range` exempts
/// fc00::/7 (RFC 4193) wholesale, matching upstream `allowUniqueLocalRange`
/// (#74351): other reserved IPv6 ranges stay blocked. The IPv4
/// `allow_rfc2544_benchmark_range` exempts 198.18.0.0/15.
pub(crate) fn is_private_ip_with_policy(ip: std::net::IpAddr, policy: &SsrfPolicy) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            if policy.allow_rfc2544_benchmark_range
                && (v4.octets()[0] == 198 && (v4.octets()[1] == 18 || v4.octets()[1] == 19))
            {
                return false;
            }
            is_private_ipv4(&v4)
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();

            // Loopback (::1)
            if v6.is_loopback() {
                return true;
            }

            // Unspecified (::)
            if v6.is_unspecified() {
                return true;
            }

            // Unique local addresses (fc00::/7 — segments[0] starts with 0xfc or 0xfd)
            if (segments[0] & 0xfe00) == 0xfc00 {
                // Opt-in for trusted fake-IP proxy stacks that resolve
                // foreign domains to ULA addresses (v2026.7.1).
                return !policy.allow_ipv6_unique_local_range;
            }

            // Link-local (fe80::/10)
            if (segments[0] & 0xffc0) == 0xfe80 {
                return true;
            }

            // Deprecated site-local (fec0::/10)
            if (segments[0] & 0xffc0) == 0xfec0 {
                return true;
            }

            // Multicast (ff00::/8)
            if (segments[0] & 0xff00) == 0xff00 {
                return true;
            }

            // AWS IMDSv2 IPv6 (fd00:ec2::254)
            if segments[0] == 0xfd00
                && segments[1] == 0x0ec2
                && segments[2..7] == [0, 0, 0, 0, 0]
                && segments[7] == 0x0254
            {
                return true;
            }

            // IPv4-mapped IPv6 (::ffff:x.x.x.x) — apply IPv4 rules
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_ip(std::net::IpAddr::V4(mapped));
            }

            // IPv6 transition mechanism embedded IPv4 addresses
            // (NAT64, 6to4, Teredo, ISATAP)
            if let Some(embedded) = extract_ipv6_embedded_ipv4(&v6) {
                return is_private_ipv4(&embedded);
            }

            false
        }
    }
}

/// Resolve the hostname and return true if ANY resolved address is private/
/// internal. Uses port 80 as a placeholder since we only need the address
/// list. On resolution failure, returns false — the request will then fail
/// at connection time, which is acceptable.
async fn hostname_resolves_to_private_ip(host: &str) -> bool {
    hostname_resolves_to_private_ip_with_policy(host, &SsrfPolicy::default()).await
}

/// Policy-aware DNS re-check (v2026.7.1).
pub(crate) async fn hostname_resolves_to_private_ip_with_policy(
    host: &str,
    policy: &SsrfPolicy,
) -> bool {
    let target = format!("{}:80", host);
    match tokio::net::lookup_host(target).await {
        Ok(addrs) => {
            for addr in addrs {
                if is_private_ip_with_policy(addr.ip(), policy) {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// True when the hostname resolves and EVERY resolved address is private.
///
/// Used by self-hosted endpoint detection (Brave/SearXNG/Firecrawl base URL
/// validation): a host that resolves only to private space is treated as an
/// intentionally self-hosted deployment. Resolution failure returns false.
pub(crate) async fn hostname_resolves_only_to_private_ips(host: &str) -> bool {
    // IP literals short-circuit without DNS.
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return is_private_ip(ip);
    }
    let target = format!("{}:80", host);
    match tokio::net::lookup_host(target).await {
        Ok(addrs) => {
            let mut any = false;
            for addr in addrs {
                any = true;
                if !is_private_ip(addr.ip()) {
                    return false;
                }
            }
            any
        }
        Err(_) => false,
    }
}

/// True for hostnames that are blocked outright by static SSRF rules
/// (localhost variants, `.internal`/`.local` TLDs, cloud metadata hosts).
pub(crate) fn is_blocked_hostname(host: &str) -> bool {
    let lower = host.to_lowercase();
    lower == "localhost"
        || lower == "127.0.0.1"
        || lower == "::1"
        || lower == "[::1]"
        || lower.ends_with(".localhost")
        || lower.ends_with(".internal")
        || lower.ends_with(".local")
        || lower.ends_with(".svc.cluster.local")
        || lower == "metadata.google.internal"
        || lower == "169.254.169.254"
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn ssrf_blocks_localhost_literal() {
        assert!(is_ssrf_target(&url("http://localhost/x")));
        assert!(is_ssrf_target(&url("https://localhost:8080/x")));
    }

    #[test]
    fn ssrf_blocks_127_0_0_1() {
        assert!(is_ssrf_target(&url("http://127.0.0.1/")));
    }

    #[test]
    fn ssrf_blocks_ipv6_loopback() {
        assert!(is_ssrf_target(&url("http://[::1]/")));
    }

    #[test]
    fn ssrf_blocks_aws_imdsv2_endpoint() {
        assert!(is_ssrf_target(&url("http://169.254.169.254/latest/meta-data/")));
    }

    #[test]
    fn ssrf_blocks_gcp_metadata_endpoint() {
        assert!(is_ssrf_target(&url(
            "http://metadata.google.internal/computeMetadata/v1/"
        )));
    }

    #[test]
    fn ssrf_blocks_internal_tld() {
        assert!(is_ssrf_target(&url("http://service.internal/")));
    }

    #[test]
    fn ssrf_blocks_local_tld() {
        assert!(is_ssrf_target(&url("http://printer.local/")));
    }

    #[test]
    fn ssrf_blocks_kubernetes_svc_dns() {
        assert!(is_ssrf_target(&url("http://api.default.svc.cluster.local/")));
    }

    #[test]
    fn ssrf_blocks_dotted_localhost_subdomain() {
        assert!(is_ssrf_target(&url("http://foo.localhost/")));
    }

    #[test]
    fn ssrf_blocks_non_http_schemes() {
        assert!(is_ssrf_target(&url("ftp://example.com/file")));
        assert!(is_ssrf_target(&url("file:///etc/passwd")));
        assert!(is_ssrf_target(&url("gopher://example.com/")));
    }

    #[test]
    fn ssrf_allows_public_https_url() {
        // Public domains pass the static check; the runtime DNS check is the
        // second layer (covered separately).
        assert!(!is_ssrf_target(&url("https://example.com/path")));
    }

    #[test]
    fn ssrf_blocks_rfc1918_literals() {
        assert!(is_ssrf_target(&url("http://10.0.0.1/")));
        assert!(is_ssrf_target(&url("http://172.16.0.1/")));
        assert!(is_ssrf_target(&url("http://192.168.1.1/")));
    }

    #[test]
    fn ssrf_blocks_link_local_169_254() {
        assert!(is_ssrf_target(&url("http://169.254.0.50/")));
    }

    #[test]
    fn ssrf_blocks_ipv4_mapped_ipv6_loopback() {
        // ::ffff:127.0.0.1 must apply IPv4 rules and resolve to loopback
        assert!(is_ssrf_target(&url("http://[::ffff:127.0.0.1]/")));
    }

    #[test]
    fn ssrf_blocks_ipv6_unique_local() {
        // fc00::/7 — unique local addresses
        assert!(is_ssrf_target(&url("http://[fc00::1]/")));
        assert!(is_ssrf_target(&url("http://[fd00::1]/")));
    }

    #[test]
    fn ssrf_blocks_ipv6_link_local() {
        // fe80::/10
        assert!(is_ssrf_target(&url("http://[fe80::1]/")));
    }

    // ---- SSRF policy escapes (v2026.7.1) -----------------------------------

    #[test]
    fn ula_opt_in_allows_fc00_range_only() {
        let policy = SsrfPolicy {
            allow_ipv6_unique_local_range: true,
            ..Default::default()
        };
        assert!(!is_ssrf_target_with_policy(&url("http://[fc00::1]/"), &policy));
        assert!(!is_ssrf_target_with_policy(&url("http://[fd12::8]/"), &policy));
        // Loopback, link-local, and site-local IPv6 stay blocked.
        assert!(is_ssrf_target_with_policy(&url("http://[::1]/"), &policy));
        assert!(is_ssrf_target_with_policy(&url("http://[fe80::1]/"), &policy));
        assert!(is_ssrf_target_with_policy(&url("http://[fec0::1]/"), &policy));
        // IPv4 private space is unaffected by the IPv6 opt-in.
        assert!(is_ssrf_target_with_policy(&url("http://10.0.0.1/"), &policy));
    }

    #[test]
    fn ula_blocked_without_opt_in() {
        let policy = SsrfPolicy::default();
        assert!(is_ssrf_target_with_policy(&url("http://[fc00::1]/"), &policy));
        assert!(is_ssrf_target_with_policy(&url("http://[fd00::1]/"), &policy));
    }

    #[test]
    fn rfc2544_opt_in_allows_benchmark_range_only() {
        let policy = SsrfPolicy {
            allow_rfc2544_benchmark_range: true,
            ..Default::default()
        };
        assert!(!is_ssrf_target_with_policy(&url("http://198.18.0.1/"), &policy));
        assert!(!is_ssrf_target_with_policy(&url("http://198.19.255.1/"), &policy));
        // Neighboring ranges stay blocked.
        assert!(is_ssrf_target_with_policy(&url("http://192.168.1.1/"), &policy));
        assert!(is_ssrf_target_with_policy(&url("http://[fc00::1]/"), &policy));
    }

    #[test]
    fn blocked_hostname_static_list() {
        assert!(is_blocked_hostname("localhost"));
        assert!(is_blocked_hostname("foo.localhost"));
        assert!(is_blocked_hostname("metadata.google.internal"));
        assert!(is_blocked_hostname("169.254.169.254"));
        assert!(!is_blocked_hostname("example.com"));
    }

    #[tokio::test]
    async fn resolves_only_to_private_handles_ip_literals() {
        assert!(hostname_resolves_only_to_private_ips("127.0.0.1").await);
        assert!(hostname_resolves_only_to_private_ips("[fc00::1]").await);
        assert!(!hostname_resolves_only_to_private_ips("1.1.1.1").await);
    }

    // ---- Bounded reads + truncation spill (v2026.7.1) ----------------------

    #[test]
    fn char_boundary_truncation_never_splits_code_points() {
        // "héllo" — 'é' is 2 bytes starting at index 1.
        let s = "h\u{e9}llo";
        assert_eq!(truncate_on_char_boundary(s, 2), "h");
        assert_eq!(truncate_on_char_boundary(s, 3), "h\u{e9}");
        assert_eq!(truncate_on_char_boundary(s, 100), s);
        assert_eq!(truncate_on_char_boundary(s, 0), "");
    }

    #[tokio::test]
    async fn bounded_read_stops_at_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(10_000)))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/big", server.uri())).await.unwrap();
        let (bytes, truncated) = read_body_bounded(response, 1_000).await.unwrap();
        assert_eq!(bytes.len(), 1_000);
        assert!(truncated);

        let response = reqwest::get(format!("{}/big", server.uri())).await.unwrap();
        let (bytes, truncated) = read_body_bounded(response, 100_000).await.unwrap();
        assert_eq!(bytes.len(), 10_000);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn spill_writes_full_content_and_returns_path() {
        let path = spill_full_content_to_file("full fetched content")
            .await
            .expect("spill path");
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(written, "full fetched content");
        let _ = tokio::fs::remove_file(&path).await;
    }

    // ---- DNS re-check ------------------------------------------------------

    #[tokio::test]
    async fn dns_recheck_blocks_localhost_hostname() {
        // "localhost" resolves to 127.0.0.1 / ::1 on essentially all systems.
        assert!(hostname_resolves_to_private_ip("localhost").await);
    }

    #[tokio::test]
    async fn dns_recheck_returns_false_on_resolution_failure() {
        // .invalid is reserved (RFC 2606) and must never resolve.
        assert!(!hostname_resolves_to_private_ip("nonexistent-host.invalid").await);
    }
}
