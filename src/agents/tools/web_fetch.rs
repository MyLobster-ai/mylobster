use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
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

        let body = response.text().await?;

        // Truncate if needed
        let text = if body.len() > max_chars {
            format!(
                "{}... (truncated, {} chars total)",
                &body[..max_chars],
                body.len()
            )
        } else {
            body
        };

        Ok(ToolResult::json(serde_json::json!({
            "status": status.as_u16(),
            "contentType": content_type,
            "text": text
        })))
    }
}

/// Check if a URL targets a private/internal address.
fn is_ssrf_target(url: &Url) -> bool {
    if let Some(host) = url.host_str() {
        // Block localhost
        if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" {
            return true;
        }

        // Block private IP ranges
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            return match ip {
                std::net::IpAddr::V4(v4) => {
                    v4.is_private()
                        || v4.is_loopback()
                        || v4.is_link_local()
                        || v4.octets()[0] == 169 && v4.octets()[1] == 254
                }
                std::net::IpAddr::V6(v6) => v6.is_loopback(),
            };
        }

        // Block common internal hostnames
        let lower = host.to_lowercase();
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

    // Block non-HTTP schemes
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }

    false
}
