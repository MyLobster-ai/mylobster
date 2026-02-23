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
            name: "web.fetch".to_string(),
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

        // SSRF protection
        let url = Url::parse(url_str)?;
        if is_ssrf_target(&url) {
            return Ok(ToolResult::error(
                "URL targets a private/internal address (SSRF protection)",
            ));
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(3))
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

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

        let body = response.text().await?;

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

        // Truncate if needed
        let text = if text.len() > max_chars {
            format!(
                "{}... (truncated, {} chars total)",
                &text[..max_chars],
                text.len()
            )
        } else {
            text
        };

        Ok(ToolResult::json(serde_json::json!({
            "status": status.as_u16(),
            "contentType": content_type,
            "extractMode": extract_mode,
            "text": text
        })))
    }
}

/// Check if a URL targets a private/internal address.
fn is_ssrf_target(url: &Url) -> bool {
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

        // Block private IP ranges
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            return is_private_ip(ip);
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

/// Check if an IP address is private/internal.
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_private_ipv4(&v4),
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
                return true;
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

            // TODO: Add DNS re-check — currently we only check the URL hostname,
            // not the resolved IP. A future enhancement should perform async DNS
            // resolution and re-validate the resolved address.

            false
        }
    }
}
