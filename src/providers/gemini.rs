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

/// Default Generative Language API base URL (no trailing slash).
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: Client,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            client: Client::new(),
        }
    }

    /// Override the API base URL (e.g. for Vertex/regional endpoints or test mocks).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
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
            "{}/models/{}:generateContent?key={}",
            self.base_url, self.model, self.api_key
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderMessage;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ------------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------------

    fn user_msg(text: &str) -> ProviderMessage {
        ProviderMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn req(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![user_msg("hi")],
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
        }
    }

    fn ok_response_json() -> serde_json::Value {
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
        })
    }

    async fn mock_with_body(model: &str, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/models/{}:generateContent", model)))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    async fn captured(
        server: &MockServer,
    ) -> (
        std::collections::HashMap<String, String>,
        serde_json::Value,
    ) {
        let reqs = server.received_requests().await.expect("requests recorded");
        let r = reqs.first().expect("at least one request");
        let mut hdrs = std::collections::HashMap::new();
        for (name, val) in r.headers.iter() {
            hdrs.insert(
                name.as_str().to_ascii_lowercase(),
                val.to_str().unwrap_or("").to_string(),
            );
        }
        let body: serde_json::Value =
            serde_json::from_slice(&r.body).expect("body is valid JSON");
        (hdrs, body)
    }

    async fn collect_stream(
        mut rx: mpsc::Receiver<StreamEvent>,
    ) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, StreamEvent::Done(_) | StreamEvent::Error(_));
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    fn make_provider(server: &MockServer, model: &str) -> GeminiProvider {
        GeminiProvider::new("test-key".into(), model.into()).with_base_url(server.uri())
    }

    // ------------------------------------------------------------------------
    // OAuth warning constants
    // ------------------------------------------------------------------------

    #[test]
    fn oauth_warning_constant_mentions_google_account() {
        assert!(GEMINI_OAUTH_RISK_WARNING.contains("Google account"));
        assert!(GEMINI_OAUTH_RISK_WARNING.to_lowercase().contains("warning"));
    }

    #[test]
    fn should_warn_oauth_returns_true_by_default() {
        std::env::remove_var("GEMINI_SKIP_OAUTH_WARNING");
        assert!(should_warn_oauth());
    }

    #[test]
    fn should_warn_oauth_returns_false_when_env_set() {
        // SAFETY: tests share env vars. Set, assert, unset within the test.
        std::env::set_var("GEMINI_SKIP_OAUTH_WARNING", "1");
        let result = should_warn_oauth();
        std::env::remove_var("GEMINI_SKIP_OAUTH_WARNING");
        assert!(!result);
    }

    // ------------------------------------------------------------------------
    // Message conversion
    // ------------------------------------------------------------------------

    #[test]
    fn convert_messages_remaps_assistant_to_model() {
        let msgs = vec![ProviderMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::String("hi".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted[0].role, "model");
    }

    #[test]
    fn convert_messages_preserves_user_role() {
        let converted = convert_messages(vec![user_msg("q")]);
        assert_eq!(converted[0].role, "user");
    }

    #[test]
    fn convert_messages_extracts_text_from_string_content() {
        let converted = convert_messages(vec![user_msg("plain")]);
        assert_eq!(converted[0].parts[0].text.as_deref(), Some("plain"));
    }

    #[test]
    fn convert_messages_serializes_non_string_content_to_json_string() {
        let msg = ProviderMessage {
            role: "user".to_string(),
            content: json!({"complex": [1, 2, 3]}),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let converted = convert_messages(vec![msg]);
        let text = converted[0].parts[0].text.as_deref().unwrap();
        assert!(
            text.contains("complex") && text.contains('1'),
            "non-string content should fall back to JSON serialization, got: {}",
            text
        );
    }

    #[test]
    fn convert_messages_preserves_order() {
        let msgs = vec![
            user_msg("first"),
            ProviderMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String("middle".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("third"),
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].parts[0].text.as_deref(), Some("first"));
        assert_eq!(converted[1].parts[0].text.as_deref(), Some("middle"));
        assert_eq!(converted[2].parts[0].text.as_deref(), Some("third"));
    }

    // ------------------------------------------------------------------------
    // Endpoint + auth
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_targets_model_endpoint_with_api_key_query_param() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-1.5-pro:generateContent"))
            .and(query_param("key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let p = make_provider(&server, "gemini-1.5-pro");
        p.chat(req("gemini-1.5-pro")).await.unwrap();
    }

    #[tokio::test]
    async fn chat_sets_content_type_json() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn chat_honors_custom_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let p = GeminiProvider::new("k".into(), "m".into())
            .with_base_url(format!("{}/v1beta", server.uri()));
        p.chat(req("m")).await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Request body shaping
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sends_generation_config_with_max_output_tokens() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        let mut r = req("m");
        r.max_tokens = Some(2048);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[tokio::test]
    async fn chat_sends_generation_config_with_temperature() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        let mut r = req("m");
        r.temperature = Some(0.4);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["generationConfig"]["temperature"], 0.4);
    }

    #[tokio::test]
    async fn chat_omits_optional_generation_config_fields() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        let cfg = &b["generationConfig"];
        assert!(cfg.get("maxOutputTokens").is_none());
        assert!(cfg.get("temperature").is_none());
    }

    #[tokio::test]
    async fn chat_sends_contents_array_with_role_and_parts() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        assert!(b["contents"].is_array());
        assert_eq!(b["contents"][0]["role"], "user");
        assert_eq!(b["contents"][0]["parts"][0]["text"], "hi");
    }

    // ------------------------------------------------------------------------
    // Response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_parses_text_from_first_candidate() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": "world"}]},
                    "finishReason": "STOP"
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "world");
        assert_eq!(r.stop_reason.as_deref(), Some("STOP"));
    }

    #[tokio::test]
    async fn chat_concatenates_multiple_parts() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "hel"}, {"text": "lo"}]
                    }
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "hello");
    }

    #[tokio::test]
    async fn chat_skips_parts_without_text() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "kept"}, {}]
                    }
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "kept");
        assert_eq!(r.content.len(), 1);
    }

    #[tokio::test]
    async fn chat_handles_missing_candidates() {
        let server = mock_with_body("m", json!({})).await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert!(r.content.is_empty());
        assert!(r.stop_reason.is_none());
    }

    #[tokio::test]
    async fn chat_parses_usage_metadata() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}}],
                "usageMetadata": {"promptTokenCount": 33, "candidatesTokenCount": 11}
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.usage.input_tokens, Some(33));
        assert_eq!(r.usage.output_tokens, Some(11));
        assert!(r.usage.cache_read_tokens.is_none());
        assert!(r.usage.cache_write_tokens.is_none());
    }

    #[tokio::test]
    async fn chat_handles_missing_usage_metadata() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}}]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert!(r.usage.input_tokens.is_none());
        assert!(r.usage.output_tokens.is_none());
    }

    #[tokio::test]
    async fn chat_returns_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(403).set_body_string("permission denied"))
            .mount(&server)
            .await;
        let p = make_provider(&server, "m");
        let err = p.chat(req("m")).await.unwrap_err().to_string();
        assert!(err.contains("403"), "should mention status: {}", err);
        assert!(err.to_lowercase().contains("gemini"), "should mention provider: {}", err);
    }

    // ------------------------------------------------------------------------
    // Streaming (currently a non-streaming fallback)
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn stream_chat_emits_full_text_then_done() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "stream-text"}]}}],
                "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["stream-text"]);
        match events.last() {
            Some(StreamEvent::Done(u)) => {
                assert_eq!(u.input_tokens, Some(7));
                assert_eq!(u.output_tokens, Some(3));
            }
            _ => panic!("last event should be Done"),
        }
    }

    #[tokio::test]
    async fn stream_chat_skips_delta_when_text_empty() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}}]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let saw_delta = events.iter().any(|e| matches!(e, StreamEvent::Delta(_)));
        assert!(!saw_delta, "no delta should be emitted for empty text");
        assert!(matches!(events.last(), Some(StreamEvent::Done(_))));
    }

    #[tokio::test]
    async fn stream_chat_emits_error_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let p = make_provider(&server, "m");
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Error(msg)) => assert!(msg.to_lowercase().contains("gemini")),
            _ => panic!("expected Error last"),
        }
    }

    // ------------------------------------------------------------------------
    // Provider name
    // ------------------------------------------------------------------------

    #[test]
    fn name_is_google() {
        let p = GeminiProvider::new("k".into(), "m".into());
        assert_eq!(p.name(), "google");
    }

    #[test]
    fn default_base_url_points_to_generative_language_v1beta() {
        assert_eq!(
            DEFAULT_GEMINI_BASE_URL,
            "https://generativelanguage.googleapis.com/v1beta"
        );
    }
}
