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

/// Check if an IP address is private/internal.
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                // Link-local / APIPA (169.254.0.0/16)
                || (octets[0] == 169 && octets[1] == 254)
                // Carrier-grade NAT (100.64.0.0/10)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
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

            false
        }
    }
}
