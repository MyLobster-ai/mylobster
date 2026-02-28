use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

/// v2026.2.26: Risk warning for Gemini CLI OAuth.
///
/// Gemini CLI OAuth grants broad Google account access. This constant
/// provides the warning text that should be shown to users before
/// initiating OAuth flows.
pub const GEMINI_OAUTH_RISK_WARNING: &str =
    "WARNING: Gemini CLI OAuth grants access to your Google account. \
     Only proceed if you trust this application and understand the \
     permissions being requested. This is NOT recommended for shared \
     or untrusted environments.";

/// Check if Gemini OAuth should require confirmation.
///
/// Returns `true` if the environment suggests this is a CLI or
/// unattended context where OAuth risks should be highlighted.
pub fn should_warn_oauth() -> bool {
    // Warn in non-interactive or shared environments
    std::env::var("GEMINI_SKIP_OAUTH_WARNING").is_err()
}

pub struct GeminiProvider {
    api_key: String,
    model: String,
    client: Client,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: Client::new(),
        }
    }
}

// ============================================================================
// Gemini API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
}

// ============================================================================
// Helper: Convert ProviderMessages to Gemini format
// ============================================================================

fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<GeminiContent> {
    messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "assistant" => "model".to_string(),
                other => other.to_string(),
            };

            let text = if let Some(s) = m.content.as_str() {
                s.to_string()
            } else {
                m.content.to_string()
            };

            GeminiContent {
                role,
                parts: vec![GeminiPart { text: Some(text) }],
            }
        })
        .collect()
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

#[async_trait]
impl ModelProvider for GeminiProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let contents = convert_messages(request.messages);

        let body = GeminiRequest {
            contents,
            generation_config: Some(GeminiGenerationConfig {
                max_output_tokens: request.max_tokens,
                temperature: request.temperature,
            }),
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({}): {}", status, text);
        }

        let api_resp: GeminiResponse = resp.json().await?;

        let mut content = Vec::new();
        let mut stop_reason = None;

        if let Some(candidates) = api_resp.candidates {
            if let Some(candidate) = candidates.into_iter().next() {
                stop_reason = candidate.finish_reason;
                if let Some(c) = candidate.content {
                    for part in c.parts {
                        if let Some(text) = part.text {
                            content.push(ContentBlock::Text(text));
                        }
                    }
                }
            }
        }

        let usage_meta = api_resp.usage_metadata.unwrap_or(GeminiUsageMetadata {
            prompt_token_count: None,
            candidates_token_count: None,
        });

        Ok(ProviderResponse {
            content,
            stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: usage_meta.prompt_token_count,
                output_tokens: usage_meta.candidates_token_count,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        // Gemini streaming uses a different endpoint; fall back to non-streaming
        // and emit the result as a single delta + done event.
        let (tx, rx) = mpsc::channel(256);

        let response = self.chat(request).await;

        tokio::spawn(async move {
            match response {
                Ok(resp) => {
                    let text = resp.content_text();
                    if !text.is_empty() {
                        let _ = tx.send(StreamEvent::Delta(text)).await;
                    }
                    let _ = tx.send(StreamEvent::Done(resp.usage)).await;
                }
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error(format!("Gemini error: {}", e)))
                        .await;
                }
            }
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        "google"
    }
}
