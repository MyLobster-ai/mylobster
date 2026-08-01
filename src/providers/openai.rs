use super::openai_compat;
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;

// ============================================================================
// keychain:<service>:<account> OPENAI_API_KEY refs (v2026.5.2, issue #72120)
// ============================================================================

/// Parse a `keychain:<service>:<account>` secret ref. Returns
/// `(service, account)` when the ref is well-formed.
pub fn parse_keychain_ref(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let rest = trimmed.strip_prefix("keychain:")?;
    let (service, account) = rest.split_once(':')?;
    let service = service.trim();
    let account = account.trim();
    if service.is_empty() || account.is_empty() {
        return None;
    }
    Some((service.to_string(), account.to_string()))
}

/// Bounded cache for resolved Keychain lookups (upstream: "bounded cached
/// Keychain lookup" before Realtime sessions / voice bridges).
static KEYCHAIN_CACHE: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<String, String>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

const KEYCHAIN_CACHE_MAX_ENTRIES: usize = 32;

/// Resolve an `OPENAI_API_KEY` value that may be a
/// `keychain:<service>:<account>` ref (macOS Keychain via
/// `security find-generic-password`). Non-ref values pass through unchanged.
/// Lookups are cached (bounded) so repeated Realtime/voice-bridge session
/// creation does not re-prompt the Keychain.
pub fn resolve_openai_api_key_ref(raw: &str) -> Result<String> {
    let Some((service, account)) = parse_keychain_ref(raw) else {
        return Ok(raw.to_string());
    };
    let cache_key = format!("{}:{}", service, account);
    if let Some(cached) = KEYCHAIN_CACHE.lock().get(&cache_key).cloned() {
        return Ok(cached);
    }
    if !cfg!(target_os = "macos") {
        anyhow::bail!(
            "keychain: OPENAI_API_KEY refs require macOS Keychain (ref service {})",
            service
        );
    }
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("Keychain lookup failed to launch: {}", e))?;
    if !output.status.success() {
        anyhow::bail!(
            "Keychain lookup failed for service {} (status {})",
            service,
            output.status
        );
    }
    let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if secret.is_empty() {
        anyhow::bail!("Keychain returned an empty secret for service {}", service);
    }
    let mut cache = KEYCHAIN_CACHE.lock();
    if cache.len() >= KEYCHAIN_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(cache_key, secret.clone());
    Ok(secret)
}

// ============================================================================
// Transport selection (v2026.5.2: direct OpenAI Responses models default to
// SSE instead of WebSocket auto-selection unless WS is explicit)
// ============================================================================

/// Transport used for OpenAI streaming sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiTransport {
    Sse,
    Websocket,
    Auto,
}

/// Whether a model id belongs to the GPT-5 / Responses-API family that
/// carries the SSE-default transport policy.
pub fn is_gpt5_responses_model(model_id: &str) -> bool {
    let normalized = model_id.trim().to_ascii_lowercase();
    let normalized = normalized.strip_prefix("openai/").unwrap_or(&normalized);
    normalized.starts_with("gpt-5") || normalized.starts_with("o3") || normalized.starts_with("o4")
}

/// Resolve the effective transport for a direct OpenAI session.
///
/// GPT-5-family Responses sessions default to the SSE Responses transport;
/// WebSocket is only used when explicitly configured. Other models keep the
/// caller-provided (or auto) transport.
pub fn resolve_openai_transport(
    model_id: &str,
    explicit: Option<OpenAiTransport>,
) -> OpenAiTransport {
    match explicit {
        Some(OpenAiTransport::Websocket) => OpenAiTransport::Websocket,
        Some(OpenAiTransport::Sse) => OpenAiTransport::Sse,
        Some(OpenAiTransport::Auto) | None => {
            if is_gpt5_responses_model(model_id) {
                OpenAiTransport::Sse
            } else {
                explicit.unwrap_or(OpenAiTransport::Auto)
            }
        }
    }
}

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

    // ------------------------------------------------------------------
    // keychain:<service>:<account> refs (v2026.5.2)
    // ------------------------------------------------------------------

    #[test]
    fn parse_keychain_ref_well_formed() {
        assert_eq!(
            parse_keychain_ref("keychain:MySvc:me@example.com"),
            Some(("MySvc".to_string(), "me@example.com".to_string()))
        );
        assert_eq!(
            parse_keychain_ref("  keychain:svc:acct  "),
            Some(("svc".to_string(), "acct".to_string()))
        );
    }

    #[test]
    fn parse_keychain_ref_rejects_malformed() {
        assert!(parse_keychain_ref("sk-plain-key").is_none());
        assert!(parse_keychain_ref("keychain:only-service").is_none());
        assert!(parse_keychain_ref("keychain::acct").is_none());
        assert!(parse_keychain_ref("keychain:svc:").is_none());
        assert!(parse_keychain_ref("").is_none());
    }

    #[test]
    fn resolve_api_key_ref_passes_plain_keys_through() {
        assert_eq!(
            resolve_openai_api_key_ref("sk-plain").unwrap(),
            "sk-plain".to_string()
        );
    }

    // ------------------------------------------------------------------
    // Transport selection (v2026.5.2)
    // ------------------------------------------------------------------

    #[test]
    fn gpt5_family_detection() {
        assert!(is_gpt5_responses_model("gpt-5.5"));
        assert!(is_gpt5_responses_model("openai/gpt-5.4-mini"));
        assert!(is_gpt5_responses_model("o3-pro"));
        assert!(!is_gpt5_responses_model("gpt-4o"));
        assert!(!is_gpt5_responses_model("claude-sonnet-4-6"));
    }

    #[test]
    fn gpt5_defaults_to_sse_transport() {
        assert_eq!(resolve_openai_transport("gpt-5.5", None), OpenAiTransport::Sse);
        assert_eq!(
            resolve_openai_transport("gpt-5.4", Some(OpenAiTransport::Auto)),
            OpenAiTransport::Sse
        );
    }

    #[test]
    fn explicit_websocket_wins_for_gpt5() {
        assert_eq!(
            resolve_openai_transport("gpt-5.5", Some(OpenAiTransport::Websocket)),
            OpenAiTransport::Websocket
        );
    }

    #[test]
    fn non_gpt5_keeps_requested_transport() {
        assert_eq!(
            resolve_openai_transport("gpt-4o", None),
            OpenAiTransport::Auto
        );
        assert_eq!(
            resolve_openai_transport("gpt-4o", Some(OpenAiTransport::Auto)),
            OpenAiTransport::Auto
        );
        assert_eq!(
            resolve_openai_transport("gpt-4o", Some(OpenAiTransport::Sse)),
            OpenAiTransport::Sse
        );
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
