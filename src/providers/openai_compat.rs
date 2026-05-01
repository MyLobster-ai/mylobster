//! Shared types and functions for OpenAI-compatible APIs.
//!
//! Used by OpenAI, Groq, and other providers that implement the
//! OpenAI chat completions API format.

use super::{ContentBlock, ProviderMessage, ProviderRequest, ProviderResponse, StreamEvent};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ============================================================================
// OpenAI-Compatible API Types
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAiMessage {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiChoice {
    pub message: OpenAiMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamChunk {
    pub choices: Vec<OpenAiStreamChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamChoice {
    pub delta: OpenAiStreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamDelta {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

// ============================================================================
// Shared Functions
// ============================================================================

/// Convert ProviderMessages to OpenAI format.
pub(crate) fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<OpenAiMessage> {
    messages
        .into_iter()
        .map(|m| OpenAiMessage {
            role: m.role,
            content: m.content,
            name: m.name,
            tool_call_id: m.tool_call_id,
            tool_calls: m.tool_calls,
        })
        .collect()
}

/// Build an OpenAI-compatible request body.
pub(crate) fn build_request(request: ProviderRequest, stream: bool) -> OpenAiRequest {
    OpenAiRequest {
        model: request.model,
        messages: convert_messages(request.messages),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        stream: if stream { Some(true) } else { None },
        tools: request.tools,
        tool_choice: request.tool_choice,
    }
}

/// Parse an OpenAI-compatible response into our ProviderResponse.
pub(crate) fn parse_openai_response(api_resp: OpenAiResponse) -> Result<ProviderResponse> {
    let choice = api_resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let mut content = Vec::new();

    if let Some(text) = choice.message.content.as_str() {
        if !text.is_empty() {
            content.push(ContentBlock::Text(text.to_string()));
        }
    }

    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            if let (Some(id), Some(function)) =
                (tc.get("id").and_then(|v| v.as_str()), tc.get("function"))
            {
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let input: serde_json::Value = serde_json::from_str(arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                content.push(ContentBlock::ToolUse {
                    id: id.to_string(),
                    name,
                    input,
                });
            }
        }
    }

    let usage = api_resp.usage.unwrap_or(OpenAiUsage {
        prompt_tokens: None,
        completion_tokens: None,
    });

    Ok(ProviderResponse {
        content,
        stop_reason: choice.finish_reason,
        usage: crate::gateway::TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_tokens: None,
            cache_write_tokens: None,
        },
    })
}

/// Make a non-streaming chat request to an OpenAI-compatible endpoint.
pub(crate) async fn openai_compat_chat(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: ProviderRequest,
    provider_name: &str,
) -> Result<ProviderResponse> {
    let body = build_request(request, false);

    let resp = client
        .post(format!("{}/chat/completions", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("User-Agent", "MyLobster/2026.4.1")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{} API error ({}): {}", provider_name, status, text);
    }

    let api_resp: OpenAiResponse = resp.json().await?;
    parse_openai_response(api_resp)
}

/// Make a streaming chat request to an OpenAI-compatible endpoint.
pub(crate) async fn openai_compat_stream_chat(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: ProviderRequest,
    provider_name: &str,
) -> Result<mpsc::Receiver<StreamEvent>> {
    let (tx, rx) = mpsc::channel(256);

    let body = build_request(request, true);

    let client = client.clone();
    let base_url = base_url.to_string();
    let api_key = api_key.to_string();
    let provider_name = provider_name.to_string();

    tokio::spawn(async move {
        let resp = match client
            .post(format!("{}/chat/completions", base_url))
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", "MyLobster/2026.4.1")
            .json(&body)
            .send()
            .await
        {
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
                    "{} API error ({}): {}",
                    provider_name, status, text
                )))
                .await;
            return;
        }

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
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    break;
                }
                match serde_json::from_str::<OpenAiStreamChunk>(data) {
                    Ok(chunk) => {
                        if let Some(usage) = chunk.usage {
                            total_input = usage.prompt_tokens;
                            total_output = usage.completion_tokens;
                        }
                        for choice in chunk.choices {
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    let _ = tx.send(StreamEvent::Delta(content)).await;
                                }
                            }
                            if let Some(tool_calls) = choice.delta.tool_calls {
                                for tc in tool_calls {
                                    let _ = tx.send(StreamEvent::ToolCall(tc)).await;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Skip unparseable chunks
                    }
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

    fn ok_response_json() -> serde_json::Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
    }

    async fn mock_with_body(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
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

    fn sse_response(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body.to_string())
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

    // ------------------------------------------------------------------------
    // build_request
    // ------------------------------------------------------------------------

    #[test]
    fn build_request_omits_max_tokens_when_none() {
        let body = build_request(req("gpt-4o"), false);
        assert!(body.max_tokens.is_none());
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("max_tokens").is_none());
    }

    #[test]
    fn build_request_omits_temperature_when_none() {
        let body = build_request(req("gpt-4o"), false);
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("temperature").is_none());
    }

    #[test]
    fn build_request_omits_stream_when_not_streaming() {
        let body = build_request(req("gpt-4o"), false);
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("stream").is_none());
    }

    #[test]
    fn build_request_sets_stream_true_when_streaming() {
        let body = build_request(req("gpt-4o"), true);
        let serialized = serde_json::to_value(&body).unwrap();
        assert_eq!(serialized["stream"], true);
    }

    #[test]
    fn build_request_passes_through_tools_and_tool_choice() {
        let mut r = req("gpt-4o");
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})]);
        r.tool_choice = Some(json!("auto"));
        let serialized = serde_json::to_value(&build_request(r, false)).unwrap();
        assert_eq!(serialized["tools"][0]["function"]["name"], "f");
        assert_eq!(serialized["tool_choice"], "auto");
    }

    #[test]
    fn convert_messages_preserves_all_fields() {
        let msgs = vec![ProviderMessage {
            role: "tool".to_string(),
            content: json!("result"),
            name: Some("calc".to_string()),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: Some(vec![json!({"id": "call_1"})]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].name.as_deref(), Some("calc"));
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("call_1"));
        assert!(converted[0].tool_calls.is_some());
    }

    // ------------------------------------------------------------------------
    // Headers + URL
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sets_bearer_auth_and_required_headers() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "secret-key", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("authorization").map(String::as_str),
            Some("Bearer secret-key")
        );
        assert_eq!(
            h.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(h
            .get("user-agent")
            .map(|v| v.starts_with("MyLobster/"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn chat_endpoint_is_chat_completions() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/chat/completions");
    }

    #[tokio::test]
    async fn chat_respects_base_url_with_trailing_path() {
        // OPENAI_BASE_URL overrides should be honored verbatim.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let client = Client::new();
        openai_compat_chat(&client, &base, "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/v1/chat/completions");
    }

    // ------------------------------------------------------------------------
    // Request body shaping
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_passes_through_max_tokens() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        let mut r = req("gpt-4o");
        r.max_tokens = Some(2048);
        openai_compat_chat(&client, &server.uri(), "k", r, "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["max_tokens"], 2048);
    }

    #[tokio::test]
    async fn chat_passes_through_temperature() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        let mut r = req("gpt-4o");
        r.temperature = Some(0.3);
        openai_compat_chat(&client, &server.uri(), "k", r, "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["temperature"], 0.3);
    }

    #[tokio::test]
    async fn chat_does_not_send_stream_field() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert!(b.get("stream").is_none() || b["stream"].is_null());
    }

    #[tokio::test]
    async fn chat_omits_optional_fields_when_unset() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert!(b.get("max_tokens").is_none());
        assert!(b.get("temperature").is_none());
        assert!(b.get("tools").is_none());
        assert!(b.get("tool_choice").is_none());
    }

    // ------------------------------------------------------------------------
    // Response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn parses_text_response() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert_eq!(r.content.len(), 1);
        assert!(matches!(&r.content[0], ContentBlock::Text(t) if t == "world"));
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert_eq!(r.usage.input_tokens, Some(1));
        assert_eq!(r.usage.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn parses_tool_call_response() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_42",
                        "type": "function",
                        "function": {
                            "name": "weather",
                            "arguments": "{\"city\":\"SF\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_42");
                assert_eq!(name, "weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert_eq!(r.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn parses_tool_call_with_invalid_arguments_json_falls_back_to_empty_object() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_x",
                        "function": {"name": "f", "arguments": "not-json"}
                    }]
                }
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert!(input.is_object());
                assert_eq!(input.as_object().unwrap().len(), 0);
            }
            other => panic!("expected ToolUse with empty input, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn skips_empty_text_content() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "stop"
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert!(r.content.is_empty(), "empty content should not produce a Text block");
    }

    #[tokio::test]
    async fn errors_on_empty_choices() {
        let server = mock_with_body(json!({
            "choices": [],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0}
        }))
        .await;
        let client = Client::new();
        let err = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("no choices"));
    }

    #[tokio::test]
    async fn handles_missing_usage() {
        let server = mock_with_body(json!({
            "choices": [{"message": {"role": "assistant", "content": "x"}}]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert!(r.usage.input_tokens.is_none());
        assert!(r.usage.output_tokens.is_none());
        assert!(r.usage.cache_read_tokens.is_none());
        assert!(r.usage.cache_write_tokens.is_none());
    }

    #[tokio::test]
    async fn errors_include_provider_name_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"bad key"}}"#),
            )
            .mount(&server)
            .await;
        let client = Client::new();
        let err = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "Groq")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "should include status: {}", err);
        assert!(err.contains("Groq"), "should include provider name: {}", err);
    }

    // ------------------------------------------------------------------------
    // Streaming
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn stream_sends_stream_true() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let _ = collect_stream(rx).await;
        let (_, b) = captured(&server).await;
        assert_eq!(b["stream"], true);
    }

    #[tokio::test]
    async fn stream_emits_text_deltas_in_order() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hel", "lo"]);
        assert!(matches!(events.last(), Some(StreamEvent::Done(_))));
    }

    #[tokio::test]
    async fn stream_skips_empty_content_deltas() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
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
    async fn stream_emits_tool_call_deltas() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let tool_calls: Vec<&serde_json::Value> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["function"]["name"], "f");
    }

    #[tokio::test]
    async fn stream_captures_usage_from_chunk() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let usage = match events.last() {
            Some(StreamEvent::Done(u)) => u.clone(),
            other => panic!("expected Done last, got {:?}", other.is_some()),
        };
        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[tokio::test]
    async fn stream_skips_unparseable_chunks_and_comments() {
        let body = concat!(
            ": this is a comment line\n",
            "\n",
            "data: not-json\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"survived\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
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
    async fn stream_terminates_on_done_sentinel() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"AFTER\"}}]}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let saw_after = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Delta(t) if t == "AFTER"));
        assert!(!saw_after, "[DONE] sentinel must terminate the stream");
    }

    #[tokio::test]
    async fn stream_emits_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "Groq")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Error(msg)) => {
                assert!(msg.contains("429"), "should include status: {}", msg);
                assert!(msg.contains("Groq"), "should include provider: {}", msg);
            }
            other => panic!("expected Error last, got {:?}", other.is_some()),
        }
    }
}
