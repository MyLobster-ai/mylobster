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
        }
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
        let messages = self.convert_messages(request.messages);

        let body = ConverseRequest {
            model_id: request.model,
            messages,
            inference_config: Some(InferenceConfig {
                max_tokens: request.max_tokens.or(Some(4096)),
                temperature: request.temperature,
            }),
            tool_config: None,
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
        let messages = self.convert_messages(request.messages);

        let body = ConverseRequest {
            model_id: request.model,
            messages,
            inference_config: Some(InferenceConfig {
                max_tokens: request.max_tokens.or(Some(4096)),
                temperature: request.temperature,
            }),
            tool_config: None,
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
