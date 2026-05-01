use super::openai_compat;
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        openai_compat::openai_compat_chat(&self.client, &self.base_url, &self.api_key, request, "OpenAI").await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        openai_compat::openai_compat_stream_chat(&self.client, &self.base_url, &self.api_key, request, "OpenAI").await
    }

    fn name(&self) -> &str {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderMessage;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![ProviderMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hi".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
        }
    }

    #[test]
    fn name_is_openai() {
        let p = OpenAiProvider::new("k".into(), "http://x".into(), "gpt-4o".into());
        assert_eq!(p.name(), "openai");
    }

    #[tokio::test]
    async fn chat_delegates_to_openai_compat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }]
            })))
            .mount(&server)
            .await;
        let p = OpenAiProvider::new("k".into(), server.uri(), "gpt-4o".into());
        let r = p.chat(req("gpt-4o")).await.unwrap();
        assert_eq!(r.content_text(), "ok");
    }

    #[tokio::test]
    async fn chat_error_uses_openai_provider_label() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let p = OpenAiProvider::new("k".into(), server.uri(), "gpt-4o".into());
        let err = p.chat(req("gpt-4o")).await.unwrap_err().to_string();
        assert!(err.contains("OpenAI"), "error should be labelled OpenAI: {}", err);
    }
}
