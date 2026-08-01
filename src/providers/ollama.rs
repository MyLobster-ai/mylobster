use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::debug;

pub struct OllamaProvider {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: Client,
}

impl OllamaProvider {
    pub fn new(base_url: String, model: String, api_key: Option<String>) -> Self {
        Self {
            base_url,
            model,
            api_key,
            client: Client::new(),
        }
    }

    /// Normalize the base URL: strip trailing `/v1` suffix since Ollama uses `/api/chat`.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{}/api/chat", base)
    }
}

// ============================================================================
// Ollama API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u64>,
    num_ctx: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    /// v2026.6.x: Ollama's native tool-call id is preserved when present
    /// instead of being resynthesized per index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    function: OllamaToolFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: Option<OllamaChatMessage>,
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<OllamaChatMessage> {
    messages
        .into_iter()
        .map(|m| {
            let content = match &m.content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            OllamaChatMessage {
                role: m.role,
                content,
                tool_calls: None,
            }
        })
        .collect()
}

fn build_request(request: &ProviderRequest, stream: bool) -> OllamaChatRequest {
    OllamaChatRequest {
        model: request.model.clone(),
        messages: convert_messages(request.messages.clone()),
        stream,
        tools: request.tools.clone(),
        options: Some(OllamaOptions {
            temperature: request.temperature,
            num_predict: request.max_tokens,
            num_ctx: 65536,
        }),
    }
}

fn parse_tool_calls(msg: &OllamaChatMessage) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if let Some(tool_calls) = &msg.tool_calls {
        for (i, tc) in tool_calls.iter().enumerate() {
            // v2026.6.x: preserve Ollama's native tool-call id when present.
            let id = tc
                .id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{}", i));
            blocks.push(ContentBlock::ToolUse {
                id,
                name: tc.function.name.clone(),
                input: tc.function.arguments.clone(),
            });
        }
    }
    blocks
}

// ============================================================================
// v2026.5.x–7.1 behavior helpers
// ============================================================================

/// Clamp `top_p` into the valid (0, 1] range (v2026.6.x normalization —
/// out-of-range values from proxies/config are normalized instead of being
/// rejected by the server).
pub fn normalize_top_p(top_p: f64) -> Option<f64> {
    if !top_p.is_finite() || top_p <= 0.0 {
        return None;
    }
    Some(top_p.min(1.0))
}

/// Hosts treated as local Ollama endpoints, including Docker/OrbStack
/// aliases (v2026.6.x): local hosts skip API-key requirements and are exempt
/// from remote-endpoint policies.
pub fn is_local_ollama_host(host: &str) -> bool {
    let lowered = host.trim().to_ascii_lowercase();
    let bare = lowered
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&lowered);
    bare == "localhost"
        || bare == "host.docker.internal"
        || bare == "host.orb.internal"
        || bare.ends_with(".orb.local")
        || bare.ends_with(".localhost")
        || bare == "::1"
        || bare == "0:0:0:0:0:0:0:1"
        || bare.starts_with("127.")
        || bare == "0.0.0.0"
}

/// Model-capability defaulting (v2026.6.x): discovery entries whose
/// `capabilities` list is missing/unknown default to tool-capable instead of
/// being filtered out of tool-using agents.
pub fn model_supports_tools(capabilities: Option<&[String]>) -> bool {
    match capabilities {
        None => true,
        Some(caps) if caps.is_empty() => true,
        Some(caps) => caps.iter().any(|c| c.eq_ignore_ascii_case("tools")),
    }
}

/// Incomplete-stream classification (v2026.7.1): a stream that ends without
/// a `done: true` frame is a retryable transport fallback, not a provider
/// refusal.
pub fn classify_incomplete_stream(saw_done: bool) -> Option<&'static str> {
    if saw_done {
        None
    } else {
        Some("incomplete-stream: no done frame received; retry/fallback eligible")
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let body = build_request(&request, false);
        let url = self.chat_url();

        let mut req = self.client
            .post(&url)
            .header("Content-Type", "application/json");

        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = req.json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({}): {}", status, text);
        }

        let api_resp: OllamaChatResponse = resp.json().await?;

        let mut content = Vec::new();
        if let Some(ref msg) = api_resp.message {
            if !msg.content.is_empty() {
                content.push(ContentBlock::Text(msg.content.clone()));
            }
            content.extend(parse_tool_calls(msg));
        }

        let stop_reason = if api_resp.done {
            Some("stop".to_string())
        } else {
            None
        };

        Ok(ProviderResponse {
            content,
            stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: api_resp.prompt_eval_count,
                output_tokens: api_resp.eval_count,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(256);

        let body = build_request(&request, true);
        let url = self.chat_url();
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            let mut req = client
                .post(&url)
                .header("Content-Type", "application/json");

            if let Some(ref key) = api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }

            let resp = match req.json(&body).send().await {
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
                        "Ollama API error ({}): {}",
                        status, text
                    )))
                    .await;
                return;
            }

            // Ollama streams NDJSON (one JSON object per line)
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

            let mut total_input = None;
            let mut total_output = None;

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<OllamaChatResponse>(line) {
                    Ok(chunk) => {
                        if let Some(ref msg) = chunk.message {
                            if !msg.content.is_empty() {
                                let _ = tx.send(StreamEvent::Delta(msg.content.clone())).await;
                            }
                            // Tool calls arrive in intermediate chunks
                            if let Some(ref tool_calls) = msg.tool_calls {
                                for tc in tool_calls {
                                    let _ = tx
                                        .send(StreamEvent::ToolCall(serde_json::json!({
                                            "function": {
                                                "name": tc.function.name,
                                                "arguments": tc.function.arguments.to_string()
                                            }
                                        })))
                                        .await;
                                }
                            }
                        }

                        if chunk.done {
                            total_input = chunk.prompt_eval_count;
                            total_output = chunk.eval_count;
                        }
                    }
                    Err(e) => {
                        debug!("Skipping unparseable Ollama NDJSON line: {}", e);
                    }
                }
            }

            let _ = tx
                .send(StreamEvent::Done(crate::gateway::TokenUsage {
                    input_tokens: total_input,
                    output_tokens: total_output,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }))
                .await;
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        "ollama"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
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

    async fn mock_chat(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mock_chat_ndjson(server: &MockServer, body: &str) {
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_string(body.to_string()),
            )
            .mount(server)
            .await;
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

    fn ok_response_json() -> serde_json::Value {
        json!({
            "message": {"role": "assistant", "content": "hello"},
            "done": true,
            "prompt_eval_count": 4,
            "eval_count": 2
        })
    }

    // ------------------------------------------------------------------------
    // chat_url normalization
    // ------------------------------------------------------------------------

    #[test]
    fn chat_url_appends_api_chat_to_plain_base() {
        let p = OllamaProvider::new(
            "http://localhost:11434".to_string(),
            "llama3".to_string(),
            None,
        );
        assert_eq!(p.chat_url(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn chat_url_strips_trailing_slash() {
        let p = OllamaProvider::new(
            "http://localhost:11434/".to_string(),
            "llama3".to_string(),
            None,
        );
        assert_eq!(p.chat_url(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn chat_url_strips_trailing_v1_for_openai_compat_envs() {
        // Ollama exposes BOTH /api/chat (native) and /v1/chat/completions (OpenAI
        // compat). When the user pastes the OpenAI-style base URL, we want the
        // native endpoint.
        let p = OllamaProvider::new(
            "http://localhost:11434/v1".to_string(),
            "llama3".to_string(),
            None,
        );
        assert_eq!(p.chat_url(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn chat_url_strips_trailing_v1_with_trailing_slash() {
        let p = OllamaProvider::new(
            "http://localhost:11434/v1/".to_string(),
            "llama3".to_string(),
            None,
        );
        assert_eq!(p.chat_url(), "http://localhost:11434/api/chat");
    }

    // ------------------------------------------------------------------------
    // Auth + headers
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_omits_authorization_when_no_api_key() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        p.chat(req("llama3")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert!(
            h.get("authorization").is_none(),
            "no api_key → no Authorization header"
        );
    }

    #[tokio::test]
    async fn chat_sets_bearer_authorization_when_api_key_present() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), Some("k".into()));
        p.chat(req("llama3")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("authorization").map(String::as_str),
            Some("Bearer k")
        );
    }

    // ------------------------------------------------------------------------
    // Request body
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sends_stream_false_for_non_streaming() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        p.chat(req("llama3")).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["stream"], false);
    }

    #[tokio::test]
    async fn chat_sends_options_with_num_ctx_and_passthrough() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let mut r = req("llama3");
        r.temperature = Some(0.2);
        r.max_tokens = Some(512);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["options"]["temperature"], 0.2);
        assert_eq!(b["options"]["num_predict"], 512);
        assert_eq!(b["options"]["num_ctx"], 65536);
    }

    #[tokio::test]
    async fn chat_omits_optional_options_fields_when_unset() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        p.chat(req("llama3")).await.unwrap();
        let (_, b) = captured(&server).await;
        let opts = &b["options"];
        assert!(opts.get("temperature").is_none());
        assert!(opts.get("num_predict").is_none());
        assert_eq!(opts["num_ctx"], 65536);
    }

    #[tokio::test]
    async fn chat_passes_through_tools() {
        let server = MockServer::start().await;
        mock_chat(&server, ok_response_json()).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let mut r = req("llama3");
        r.tools = Some(vec![json!({
            "type": "function",
            "function": {"name": "f", "parameters": {}}
        })]);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn convert_messages_uses_string_content_directly() {
        let converted = convert_messages(vec![user_msg("plain")]);
        assert_eq!(converted[0].content, "plain");
    }

    #[test]
    fn convert_messages_serializes_non_string_content_to_json() {
        let msg = ProviderMessage {
            role: "user".into(),
            content: json!({"a": 1}),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let converted = convert_messages(vec![msg]);
        assert!(converted[0].content.contains("\"a\""));
    }

    // ------------------------------------------------------------------------
    // Non-streaming response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_parses_text_response() {
        let server = MockServer::start().await;
        mock_chat(
            &server,
            json!({
                "message": {"role": "assistant", "content": "world"},
                "done": true,
                "prompt_eval_count": 7,
                "eval_count": 3
            }),
        )
        .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let r = p.chat(req("llama3")).await.unwrap();
        assert_eq!(r.content_text(), "world");
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert_eq!(r.usage.input_tokens, Some(7));
        assert_eq!(r.usage.output_tokens, Some(3));
    }

    #[tokio::test]
    async fn chat_parses_tool_calls() {
        let server = MockServer::start().await;
        mock_chat(
            &server,
            json!({
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "function": {"name": "weather", "arguments": {"city": "SF"}}
                    }]
                },
                "done": true
            }),
        )
        .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let r = p.chat(req("llama3")).await.unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert!(id.starts_with("call_"));
                assert_eq!(name, "weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn chat_skips_empty_text_block() {
        let server = MockServer::start().await;
        mock_chat(
            &server,
            json!({
                "message": {"role": "assistant", "content": ""},
                "done": true
            }),
        )
        .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let r = p.chat(req("llama3")).await.unwrap();
        assert!(
            r.content
                .iter()
                .all(|b| !matches!(b, ContentBlock::Text(t) if t.is_empty())),
            "empty content must not produce a Text block"
        );
    }

    #[tokio::test]
    async fn chat_omits_stop_reason_when_done_is_false() {
        let server = MockServer::start().await;
        mock_chat(
            &server,
            json!({
                "message": {"role": "assistant", "content": "partial"},
                "done": false
            }),
        )
        .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let r = p.chat(req("llama3")).await.unwrap();
        assert!(r.stop_reason.is_none());
    }

    #[tokio::test]
    async fn chat_returns_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
            .mount(&server)
            .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let err = p.chat(req("llama3")).await.unwrap_err().to_string();
        assert!(err.contains("404"));
        assert!(err.to_lowercase().contains("ollama"));
    }

    // ------------------------------------------------------------------------
    // NDJSON streaming
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn stream_sends_stream_true() {
        let server = MockServer::start().await;
        mock_chat_ndjson(
            &server,
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        )
        .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let _ = collect_stream(rx).await;
        let (_, b) = captured(&server).await;
        assert_eq!(b["stream"], true);
    }

    #[tokio::test]
    async fn stream_emits_text_deltas_in_ndjson_order() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"hel\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":3,\"eval_count\":2}\n",
        );
        let server = MockServer::start().await;
        mock_chat_ndjson(&server, body).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hel", "lo"]);
        match events.last() {
            Some(StreamEvent::Done(u)) => {
                assert_eq!(u.input_tokens, Some(3));
                assert_eq!(u.output_tokens, Some(2));
            }
            _ => panic!("last event must be Done"),
        }
    }

    #[tokio::test]
    async fn stream_skips_empty_content_chunks() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        );
        let server = MockServer::start().await;
        mock_chat_ndjson(&server, body).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["x"]);
    }

    #[tokio::test]
    async fn stream_emits_tool_call_chunks() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"f\",\"arguments\":{\"x\":1}}}]},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        );
        let server = MockServer::start().await;
        mock_chat_ndjson(&server, body).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        let tool_calls: Vec<&serde_json::Value> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["function"]["name"], "f");
    }

    #[tokio::test]
    async fn stream_skips_blank_lines_and_unparseable_chunks() {
        let body = concat!(
            "\n",
            "not-json\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"survived\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        );
        let server = MockServer::start().await;
        mock_chat_ndjson(&server, body).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["survived"]);
    }

    #[tokio::test]
    async fn stream_terminates_with_done_chunk_only_setting_usage() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false,\"prompt_eval_count\":99,\"eval_count\":99}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":4,\"eval_count\":1}\n",
        );
        let server = MockServer::start().await;
        mock_chat_ndjson(&server, body).await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Done(u)) => {
                // Per current impl, only the chunk with done=true is allowed
                // to set the final usage counters.
                assert_eq!(u.input_tokens, Some(4));
                assert_eq!(u.output_tokens, Some(1));
            }
            _ => panic!("last event must be Done"),
        }
    }

    #[tokio::test]
    async fn stream_emits_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(503).set_body_string("model loading"))
            .mount(&server)
            .await;
        let p = OllamaProvider::new(server.uri(), "llama3".into(), None);
        let rx = p.stream_chat(req("llama3")).await.unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Error(msg)) => {
                assert!(msg.contains("503"));
                assert!(msg.to_lowercase().contains("ollama"));
            }
            _ => panic!("expected Error last"),
        }
    }

    // ------------------------------------------------------------------------
    // Provider name
    // ------------------------------------------------------------------------

    #[test]
    fn name_is_ollama() {
        let p = OllamaProvider::new("http://x".into(), "m".into(), None);
        assert_eq!(p.name(), "ollama");
    }

    // ------------------------------------------------------------------
    // v2026.7.1 behavior helpers
    // ------------------------------------------------------------------

    #[test]
    fn top_p_normalization_clamps_range() {
        assert_eq!(normalize_top_p(0.9), Some(0.9));
        assert_eq!(normalize_top_p(1.7), Some(1.0));
        assert_eq!(normalize_top_p(0.0), None);
        assert_eq!(normalize_top_p(-0.3), None);
        assert_eq!(normalize_top_p(f64::NAN), None);
    }

    #[test]
    fn docker_and_orbstack_aliases_are_local() {
        for host in [
            "localhost",
            "127.0.0.1",
            "127.1.2.3",
            "host.docker.internal",
            "host.orb.internal",
            "ollama.orb.local",
            "[::1]",
        ] {
            assert!(is_local_ollama_host(host), "{} should be local", host);
        }
        assert!(!is_local_ollama_host("ollama.example.com"));
        assert!(!is_local_ollama_host("192.168.1.4"));
    }

    #[test]
    fn unknown_capabilities_default_tool_capable() {
        assert!(model_supports_tools(None));
        assert!(model_supports_tools(Some(&[][..])));
        assert!(model_supports_tools(Some(&["tools".to_string()][..])));
        assert!(!model_supports_tools(Some(&["vision".to_string()][..])));
    }

    #[test]
    fn native_tool_call_ids_preserved() {
        let msg = OllamaChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls: Some(vec![
                OllamaToolCall {
                    id: Some("native-1".to_string()),
                    function: OllamaToolFunction {
                        name: "f".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
                OllamaToolCall {
                    id: None,
                    function: OllamaToolFunction {
                        name: "g".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            ]),
        };
        let blocks = parse_tool_calls(&msg);
        match (&blocks[0], &blocks[1]) {
            (
                ContentBlock::ToolUse { id: id0, .. },
                ContentBlock::ToolUse { id: id1, .. },
            ) => {
                assert_eq!(id0, "native-1");
                assert_eq!(id1, "call_1");
            }
            _ => panic!("expected tool use blocks"),
        }
    }

    #[test]
    fn incomplete_stream_classified_retryable() {
        assert!(classify_incomplete_stream(true).is_none());
        let reason = classify_incomplete_stream(false).unwrap();
        assert!(reason.contains("incomplete-stream"));
    }
}
