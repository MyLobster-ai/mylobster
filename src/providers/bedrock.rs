//! AWS Bedrock provider.
//!
//! Uses the Bedrock ConverseStream API with AWS SigV4 signing.
//! Supports Anthropic Claude models on AWS.

use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ============================================================================
// Bedrock Converse API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConverseRequest {
    model_id: String,
    messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guardrail_config: Option<GuardrailConfig>,
    /// Anthropic extended-thinking passthrough (v2026.4.29 Opus 4.7 parity).
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_model_request_fields: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConverseMessage {
    role: String,
    content: Vec<ConverseContentBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ConverseContentBlock {
    Text { text: String },
    Image { format: String, source: ImageSource },
    ToolUse { tool_use_id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: Vec<ToolResultContent> },
}

#[derive(Debug, Serialize, Deserialize)]
enum ImageSource {
    #[serde(rename = "bytes")]
    Bytes(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum ToolResultContent {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

/// Bedrock Guardrails configuration (v2026.4.1).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GuardrailConfig {
    guardrail_identifier: String,
    guardrail_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConverseResponse {
    output: Option<ConverseOutput>,
    usage: Option<ConverseUsage>,
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConverseOutput {
    message: Option<ConverseMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConverseUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

// ============================================================================
// Streaming types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamEvent_ {
    #[serde(default)]
    content_block_delta: Option<StreamDelta>,
    #[serde(default)]
    message_start: Option<serde_json::Value>,
    #[serde(default)]
    message_stop: Option<serde_json::Value>,
    #[serde(default)]
    metadata: Option<StreamMetadata>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    delta: Option<StreamDeltaContent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StreamDeltaContent {
    Text { text: String },
    ToolUse { input: String },
}

#[derive(Debug, Deserialize)]
struct StreamMetadata {
    usage: Option<ConverseUsage>,
}

// ============================================================================
// Thinking profiles (v2026.4.29 — Bedrock Opus 4.7 thinking parity)
// ============================================================================

/// Thinking levels + default for a Bedrock model ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockThinkingProfile {
    pub levels: Vec<&'static str>,
    pub default_level: &'static str,
}

const BASE_CLAUDE_THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];

/// Normalize a Bedrock model ref to its Claude family id:
/// strips `bedrock/` / `aws/` prefixes, geo prefixes (`us.` / `eu.` /
/// `apac.` / `global.`), the `anthropic.` vendor namespace, and `:N`
/// version suffixes; converts dots in the version to dashes.
pub fn normalize_bedrock_claude_model_id(model_ref: &str) -> String {
    let mut s = model_ref.trim().to_ascii_lowercase();
    for prefix in ["bedrock/", "aws/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    for geo in ["us.", "eu.", "apac.", "global."] {
        if let Some(rest) = s.strip_prefix(geo) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("anthropic.") {
        s = rest.to_string();
    }
    if let Some(colon) = s.find(':') {
        s.truncate(colon);
    }
    s.replace('.', "-")
}

/// Resolve the thinking profile for a Bedrock model ref (v2026.4.29 #74701):
///
/// * Claude Opus 4.7 (and 4.8) expose the full Anthropic-transport profile —
///   `xhigh`, `adaptive`, and `max` — matching `/think` menus and validation
///   on the direct Anthropic transport. Default stays `off`.
/// * Claude Opus/Sonnet 4.6 keep adaptive-by-default (base + `adaptive` +
///   `max`).
/// * Other Claude models get the base profile, default `off`.
/// * Non-Claude Bedrock models are off-only.
pub fn bedrock_thinking_profile(model_ref: &str) -> BedrockThinkingProfile {
    let id = normalize_bedrock_claude_model_id(model_ref);
    if !id.starts_with("claude") {
        return BedrockThinkingProfile {
            levels: vec!["off"],
            default_level: "off",
        };
    }
    if id.starts_with("claude-opus-4-7") || id.starts_with("claude-opus-4-8") {
        let mut levels = BASE_CLAUDE_THINKING_LEVELS.to_vec();
        levels.extend(["xhigh", "adaptive", "max"]);
        return BedrockThinkingProfile {
            levels,
            default_level: "off",
        };
    }
    if id.starts_with("claude-opus-4-6") || id.starts_with("claude-sonnet-4-6") {
        let mut levels = BASE_CLAUDE_THINKING_LEVELS.to_vec();
        levels.extend(["adaptive", "max"]);
        return BedrockThinkingProfile {
            levels,
            default_level: "adaptive",
        };
    }
    BedrockThinkingProfile {
        levels: BASE_CLAUDE_THINKING_LEVELS.to_vec(),
        default_level: "off",
    }
}

/// Build the `additionalModelRequestFields` thinking payload for a Converse
/// call, mirroring the Anthropic-transport wire shape.
fn build_thinking_fields(
    model_ref: &str,
    thinking: Option<&super::ThinkingConfig>,
) -> Option<serde_json::Value> {
    let thinking = thinking?;
    // Only Claude models accept the thinking field.
    if !normalize_bedrock_claude_model_id(model_ref).starts_with("claude") {
        return None;
    }
    Some(serde_json::json!({
        "thinking": {
            "type": "enabled",
            "budget_tokens": thinking.budget_tokens,
        }
    }))
}

// ============================================================================
// Service tier + inference-profile helpers (v2026.5.x–7.1)
// ============================================================================

/// Valid Bedrock `serviceTier` values (v2026.6.x param).
pub const BEDROCK_SERVICE_TIERS: &[&str] = &["default", "flex", "priority", "reserved"];

/// Normalize a configured Bedrock service tier; invalid values are rejected
/// so misconfigured tiers fail fast instead of at the API.
pub fn normalize_bedrock_service_tier(tier: &str) -> Option<&'static str> {
    let normalized = tier.trim().to_ascii_lowercase();
    BEDROCK_SERVICE_TIERS
        .iter()
        .find(|t| **t == normalized)
        .copied()
}

/// Strip a geo inference-profile prefix (`us.` / `eu.` / `apac.` /
/// `global.`) from a model id — embedding calls address the bare model id
/// (v2026.6.x fix).
pub fn strip_inference_profile_prefix(model_id: &str) -> &str {
    let trimmed = model_id.trim();
    for prefix in ["us.", "eu.", "apac.", "global."] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest;
        }
    }
    trimmed
}

// ============================================================================
// AWS SigV4 Signing (minimal implementation)
// ============================================================================

fn sign_request(
    method: &str,
    url: &str,
    body: &[u8],
    region: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
) -> Result<Vec<(String, String)>> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    let now = chrono::Utc::now();
    let date_stamp = now.format("%Y%m%d").to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

    let parsed = url::Url::parse(url)?;
    let host = parsed.host_str().unwrap_or("");
    let canonical_uri = parsed.path();
    let canonical_querystring = parsed.query().unwrap_or("");

    // Hash the payload
    let payload_hash = hex::encode(Sha256::digest(body));

    // Build canonical headers
    let mut canonical_headers = format!(
        "content-type:application/json\nhost:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, amz_date
    );
    let mut signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date".to_string();

    if let Some(token) = session_token {
        canonical_headers.push_str(&format!("x-amz-security-token:{}\n", token));
        signed_headers.push_str(";x-amz-security-token");
    }

    // Build canonical request
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_querystring, canonical_headers, signed_headers, payload_hash
    );

    let service = "bedrock";
    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);

    // Build string to sign
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    // Calculate signing key
    type HmacSha256 = Hmac<Sha256>;

    let k_date = {
        let mut mac = HmacSha256::new_from_slice(format!("AWS4{}", secret_key).as_bytes())?;
        mac.update(date_stamp.as_bytes());
        mac.finalize().into_bytes()
    };
    let k_region = {
        let mut mac = HmacSha256::new_from_slice(&k_date)?;
        mac.update(region.as_bytes());
        mac.finalize().into_bytes()
    };
    let k_service = {
        let mut mac = HmacSha256::new_from_slice(&k_region)?;
        mac.update(service.as_bytes());
        mac.finalize().into_bytes()
    };
    let k_signing = {
        let mut mac = HmacSha256::new_from_slice(&k_service)?;
        mac.update(b"aws4_request");
        mac.finalize().into_bytes()
    };

    // Calculate signature
    let signature = {
        let mut mac = HmacSha256::new_from_slice(&k_signing)?;
        mac.update(string_to_sign.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    };

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    );

    let mut headers = vec![
        ("Authorization".to_string(), authorization),
        ("x-amz-date".to_string(), amz_date),
        ("x-amz-content-sha256".to_string(), payload_hash),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];

    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }

    Ok(headers)
}

// ============================================================================
// Provider
// ============================================================================

pub struct BedrockProvider {
    region: String,
    model: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    client: Client,
    /// Optional guardrail config: (guardrail_identifier, guardrail_version, trace).
    guardrail_config: Option<(String, String, Option<String>)>,
}

impl BedrockProvider {
    pub fn new(region: String, model: String) -> Self {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();

        Self {
            region,
            model,
            access_key,
            secret_key,
            session_token,
            client: Client::new(),
            guardrail_config: None,
        }
    }

    /// Builder method to attach Bedrock Guardrails configuration (v2026.4.1).
    pub fn with_guardrails(
        mut self,
        guardrail_identifier: String,
        guardrail_version: String,
        trace: Option<String>,
    ) -> Self {
        self.guardrail_config = Some((guardrail_identifier, guardrail_version, trace));
        self
    }

    fn build_guardrail_config(&self) -> Option<GuardrailConfig> {
        self.guardrail_config.as_ref().map(|(id, version, trace)| GuardrailConfig {
            guardrail_identifier: id.clone(),
            guardrail_version: version.clone(),
            trace: trace.clone(),
        })
    }

    fn endpoint_url(&self, stream: bool) -> String {
        let action = if stream { "converse-stream" } else { "converse" };
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/{}",
            self.region, self.model, action
        )
    }

    fn convert_messages(&self, messages: Vec<ProviderMessage>) -> Vec<ConverseMessage> {
        messages
            .into_iter()
            .map(|m| {
                let content = if let Some(text) = m.content.as_str() {
                    vec![ConverseContentBlock::Text {
                        text: text.to_string(),
                    }]
                } else if let Some(arr) = m.content.as_array() {
                    arr.iter()
                        .filter_map(|item| {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                Some(ConverseContentBlock::Text {
                                    text: text.to_string(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    vec![ConverseContentBlock::Text {
                        text: m.content.to_string(),
                    }]
                };

                ConverseMessage {
                    role: if m.role == "assistant" {
                        "assistant".to_string()
                    } else {
                        "user".to_string()
                    },
                    content,
                }
            })
            .collect()
    }
}

#[async_trait]
impl ModelProvider for BedrockProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        if self.access_key.is_empty() || self.secret_key.is_empty() {
            anyhow::bail!("AWS credentials not configured (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)");
        }

        let url = self.endpoint_url(false);
        let thinking_fields = build_thinking_fields(&request.model, request.thinking.as_ref());
        let messages = self.convert_messages(request.messages);

        let body = ConverseRequest {
            model_id: request.model,
            messages,
            inference_config: Some(InferenceConfig {
                max_tokens: request.max_tokens.or(Some(4096)),
                temperature: request.temperature,
            }),
            tool_config: None,
            guardrail_config: self.build_guardrail_config(),
            additional_model_request_fields: thinking_fields,
        };

        let body_bytes = serde_json::to_vec(&body)?;
        let headers = sign_request(
            "POST",
            &url,
            &body_bytes,
            &self.region,
            &self.access_key,
            &self.secret_key,
            self.session_token.as_deref(),
        )?;

        let mut req = self.client.post(&url);
        for (key, value) in headers {
            req = req.header(&key, &value);
        }
        req = req.body(body_bytes);

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Bedrock API error ({}): {}", status, text);
        }

        let api_resp: ConverseResponse = resp.json().await?;

        let mut content = Vec::new();
        if let Some(output) = api_resp.output {
            if let Some(message) = output.message {
                for block in message.content {
                    match block {
                        ConverseContentBlock::Text { text } => {
                            content.push(ContentBlock::Text(text));
                        }
                        ConverseContentBlock::ToolUse {
                            tool_use_id,
                            name,
                            input,
                        } => {
                            content.push(ContentBlock::ToolUse {
                                id: tool_use_id,
                                name,
                                input,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        let usage = api_resp.usage.unwrap_or(ConverseUsage {
            input_tokens: None,
            output_tokens: None,
        });

        Ok(ProviderResponse {
            content,
            stop_reason: api_resp.stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        if self.access_key.is_empty() || self.secret_key.is_empty() {
            anyhow::bail!("AWS credentials not configured (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)");
        }

        let (tx, rx) = mpsc::channel(256);

        let url = self.endpoint_url(true);
        let thinking_fields = build_thinking_fields(&request.model, request.thinking.as_ref());
        let messages = self.convert_messages(request.messages);

        let body = ConverseRequest {
            model_id: request.model,
            messages,
            inference_config: Some(InferenceConfig {
                max_tokens: request.max_tokens.or(Some(4096)),
                temperature: request.temperature,
            }),
            tool_config: None,
            guardrail_config: self.build_guardrail_config(),
            additional_model_request_fields: thinking_fields,
        };

        let body_bytes = serde_json::to_vec(&body)?;
        let headers = sign_request(
            "POST",
            &url,
            &body_bytes,
            &self.region,
            &self.access_key,
            &self.secret_key,
            self.session_token.as_deref(),
        )?;

        let client = self.client.clone();

        tokio::spawn(async move {
            let mut req = client.post(&url);
            for (key, value) in headers {
                req = req.header(&key, &value);
            }
            req = req.body(body_bytes);

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error(format!("Request failed: {}", e)))
                        .await;
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(StreamEvent::Error(format!(
                        "Bedrock API error ({}): {}",
                        status, text
                    )))
                    .await;
                return;
            }

            // Bedrock ConverseStream returns event-stream format
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "Failed to read response: {}",
                            e
                        )))
                        .await;
                    return;
                }
            };

            let mut total_usage = crate::gateway::TokenUsage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            };

            // Parse event stream (newline-delimited JSON events)
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                if let Ok(event) = serde_json::from_str::<StreamEvent_>(line) {
                    if let Some(delta) = event.content_block_delta {
                        if let Some(content) = delta.delta {
                            match content {
                                StreamDeltaContent::Text { text } => {
                                    let _ = tx.send(StreamEvent::Delta(text)).await;
                                }
                                StreamDeltaContent::ToolUse { input } => {
                                    let _ = tx
                                        .send(StreamEvent::ToolCall(
                                            serde_json::json!({ "partial_json": input }),
                                        ))
                                        .await;
                                }
                            }
                        }
                    }

                    if let Some(metadata) = event.metadata {
                        if let Some(usage) = metadata.usage {
                            total_usage.input_tokens = usage.input_tokens;
                            total_usage.output_tokens = usage.output_tokens;
                        }
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done(total_usage)).await;
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        "bedrock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Model ref normalization
    // ------------------------------------------------------------------

    #[test]
    fn normalizes_geo_and_vendor_prefixes() {
        assert_eq!(
            normalize_bedrock_claude_model_id("us.anthropic.claude-opus-4-7-20260115-v1:0"),
            "claude-opus-4-7-20260115-v1"
        );
        assert_eq!(
            normalize_bedrock_claude_model_id("bedrock/anthropic.claude-sonnet-4-6-v1:0"),
            "claude-sonnet-4-6-v1"
        );
        assert_eq!(
            normalize_bedrock_claude_model_id("eu.anthropic.claude-opus-4.7"),
            "claude-opus-4-7"
        );
    }

    // ------------------------------------------------------------------
    // Thinking profiles (v2026.4.29 Opus 4.7 parity, #74701)
    // ------------------------------------------------------------------

    #[test]
    fn opus_4_7_exposes_full_thinking_profile() {
        let profile =
            bedrock_thinking_profile("us.anthropic.claude-opus-4-7-20260115-v1:0");
        assert!(profile.levels.contains(&"xhigh"));
        assert!(profile.levels.contains(&"adaptive"));
        assert!(profile.levels.contains(&"max"));
        assert_eq!(profile.default_level, "off");
    }

    #[test]
    fn opus_and_sonnet_4_6_stay_adaptive_by_default() {
        for model in [
            "anthropic.claude-opus-4-6-v1:0",
            "us.anthropic.claude-sonnet-4-6-20251101-v1:0",
        ] {
            let profile = bedrock_thinking_profile(model);
            assert!(profile.levels.contains(&"adaptive"), "{}", model);
            assert!(!profile.levels.contains(&"xhigh"), "{}", model);
            assert_eq!(profile.default_level, "adaptive", "{}", model);
        }
    }

    #[test]
    fn older_claude_gets_base_profile() {
        let profile = bedrock_thinking_profile("anthropic.claude-3-5-sonnet-20241022-v2:0");
        assert_eq!(profile.levels, BASE_CLAUDE_THINKING_LEVELS.to_vec());
        assert_eq!(profile.default_level, "off");
    }

    #[test]
    fn non_claude_models_are_off_only() {
        let profile = bedrock_thinking_profile("amazon.titan-text-express-v1");
        assert_eq!(profile.levels, vec!["off"]);
    }

    // ------------------------------------------------------------------
    // Thinking wire fields
    // ------------------------------------------------------------------

    #[test]
    fn thinking_fields_built_for_claude_models() {
        let thinking = crate::providers::ThinkingConfig { budget_tokens: 2048 };
        let fields = build_thinking_fields(
            "us.anthropic.claude-opus-4-7-20260115-v1:0",
            Some(&thinking),
        )
        .unwrap();
        assert_eq!(fields["thinking"]["type"], "enabled");
        assert_eq!(fields["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn thinking_fields_skipped_without_config_or_for_non_claude() {
        assert!(build_thinking_fields("anthropic.claude-opus-4-7", None).is_none());
        let thinking = crate::providers::ThinkingConfig { budget_tokens: 1024 };
        assert!(build_thinking_fields("amazon.titan-text-express-v1", Some(&thinking)).is_none());
    }

    #[test]
    fn converse_request_serializes_thinking_passthrough() {
        let body = ConverseRequest {
            model_id: "us.anthropic.claude-opus-4-7-v1:0".to_string(),
            messages: vec![],
            inference_config: None,
            tool_config: None,
            guardrail_config: None,
            additional_model_request_fields: build_thinking_fields(
                "us.anthropic.claude-opus-4-7-v1:0",
                Some(&crate::providers::ThinkingConfig { budget_tokens: 512 }),
            ),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            512
        );
    }

    #[test]
    fn converse_request_omits_thinking_when_absent() {
        let body = ConverseRequest {
            model_id: "m".to_string(),
            messages: vec![],
            inference_config: None,
            tool_config: None,
            guardrail_config: None,
            additional_model_request_fields: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("additionalModelRequestFields").is_none());
    }

    // ------------------------------------------------------------------
    // v2026.5.x–7.1: service tier + inference profile prefix
    // ------------------------------------------------------------------

    #[test]
    fn service_tier_normalization() {
        assert_eq!(normalize_bedrock_service_tier("default"), Some("default"));
        assert_eq!(normalize_bedrock_service_tier(" FLEX "), Some("flex"));
        assert_eq!(normalize_bedrock_service_tier("priority"), Some("priority"));
        assert_eq!(normalize_bedrock_service_tier("reserved"), Some("reserved"));
        assert_eq!(normalize_bedrock_service_tier("turbo"), None);
    }

    #[test]
    fn inference_profile_prefix_stripped_for_embeddings() {
        assert_eq!(
            strip_inference_profile_prefix("us.amazon.titan-embed-text-v2:0"),
            "amazon.titan-embed-text-v2:0"
        );
        assert_eq!(
            strip_inference_profile_prefix("global.anthropic.claude-opus-4-8-v1:0"),
            "anthropic.claude-opus-4-8-v1:0"
        );
        assert_eq!(
            strip_inference_profile_prefix("amazon.titan-embed-text-v2:0"),
            "amazon.titan-embed-text-v2:0"
        );
    }
}
