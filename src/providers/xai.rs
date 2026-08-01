//! xAI Grok provider.
//!
//! Uses the OpenAI-compatible API format.

use super::openai_compat;
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;

pub struct XaiProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
}

impl XaiProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: Client::new(),
        }
    }
}

// ============================================================================
// v2026.5.x behavior helpers
// ============================================================================

/// SuperGrok / device-code OAuth constants (v2026.5.x). The interactive
/// device flow itself is CLI-owned; the provider recognizes OAuth bearer
/// tokens from these sources and reuses them for `web_search`.
pub const XAI_OAUTH_TOKEN_ENV_VAR: &str = "XAI_OAUTH_TOKEN";

/// Resolve the xAI credential: explicit key, then `XAI_API_KEY`, then an
/// OAuth bearer from `XAI_OAUTH_TOKEN` (reused for web_search too).
pub fn resolve_xai_credential(configured: Option<&str>) -> Option<String> {
    if let Some(key) = configured.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }
    for var in ["XAI_API_KEY", XAI_OAUTH_TOKEN_ENV_VAR] {
        if let Ok(value) = std::env::var(var) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

/// `reasoning_effort` is only accepted by Grok 4.3 (v2026.5.x); other Grok
/// models reject the parameter.
pub fn xai_supports_reasoning_effort(model_id: &str) -> bool {
    let normalized = model_id.trim().to_ascii_lowercase();
    let normalized = normalized.strip_prefix("xai/").unwrap_or(&normalized);
    normalized.starts_with("grok-4.3")
}

/// Image-quality values accepted by `grok-imagine-image-quality`.
pub const GROK_IMAGINE_IMAGE_QUALITIES: &[&str] = &["standard", "high"];

/// Terminal states for pending Grok video jobs (v2026.5.x polling).
pub fn grok_video_job_is_terminal(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "succeeded" | "completed" | "failed" | "cancelled" | "expired"
    )
}

// ============================================================================
// Malformed Responses parse hardening (v2026.5.2, issues #58063/#58733)
// ============================================================================

fn extract_url_citations(annotations: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(arr) = annotations.and_then(|a| a.as_array()) else {
        return;
    };
    for annotation in arr {
        if annotation.get("type").and_then(|t| t.as_str()) == Some("url_citation") {
            if let Some(url) = annotation.get("url").and_then(|u| u.as_str()) {
                if !out.iter().any(|existing| existing == url) {
                    out.push(url.to_string());
                }
            }
        }
    }
}

/// Leniently extract the answer text and annotation citations from an xAI
/// Responses payload, tolerating malformed shapes:
///
/// * non-object `output[]` entries are skipped
/// * `message` outputs with malformed `content` arrays are tolerated
/// * bare top-level `output_text` outputs are accepted
/// * falls back to the top-level `output_text` field
///
/// Returns `(text, annotation_citations)`; `text` is `None` when no textual
/// answer exists anywhere in the payload (the caller surfaces a structured
/// "malformed JSON response" error rather than aborting the tool call).
pub fn extract_xai_web_search_content(
    data: &serde_json::Value,
) -> (Option<String>, Vec<String>) {
    if let Some(outputs) = data.get("output").and_then(|o| o.as_array()) {
        for output in outputs {
            if !output.is_object() {
                continue;
            }
            match output.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    let content = output
                        .get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for block in &content {
                        if !block.is_object() {
                            continue;
                        }
                        if block.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                            if let Some(text) =
                                block.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty())
                            {
                                let mut citations = Vec::new();
                                extract_url_citations(block.get("annotations"), &mut citations);
                                return (Some(text.to_string()), citations);
                            }
                        }
                    }
                }
                Some("output_text") => {
                    if let Some(text) =
                        output.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty())
                    {
                        let mut citations = Vec::new();
                        extract_url_citations(output.get("annotations"), &mut citations);
                        return (Some(text.to_string()), citations);
                    }
                }
                _ => {}
            }
        }
    }
    (
        data.get("output_text")
            .and_then(|t| t.as_str())
            .map(|t| t.to_string()),
        Vec::new(),
    )
}

/// Resolve `(content, citations)` from an xAI Responses payload. Top-level
/// `citations` win when non-empty; otherwise annotation citations are used.
/// Content falls back to `"No response"` when the payload carried no text.
pub fn resolve_xai_response_text_and_citations(
    data: &serde_json::Value,
) -> (String, Vec<String>) {
    let (text, annotation_citations) = extract_xai_web_search_content(data);
    let top_level: Vec<String> = data
        .get("citations")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let citations = if !top_level.is_empty() {
        top_level
    } else {
        annotation_citations
    };
    (text.unwrap_or_else(|| "No response".to_string()), citations)
}

#[async_trait]
impl ModelProvider for XaiProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "xAI",
        )
        .await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "xAI",
        )
        .await
    }

    fn name(&self) -> &str {
        "xai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_message_output_text_with_citations() {
        let data = json!({"output": [{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "answer",
                "annotations": [
                    {"type": "url_citation", "url": "https://a.example"},
                    {"type": "url_citation", "url": "https://a.example"},
                    {"type": "other", "url": "https://ignored.example"}
                ]
            }]
        }]});
        let (text, citations) = extract_xai_web_search_content(&data);
        assert_eq!(text.as_deref(), Some("answer"));
        assert_eq!(citations, vec!["https://a.example"]);
    }

    #[test]
    fn accepts_bare_top_level_output_text_entry() {
        let data = json!({"output": [
            {"type": "output_text", "text": "bare", "annotations": []}
        ]});
        let (text, citations) = extract_xai_web_search_content(&data);
        assert_eq!(text.as_deref(), Some("bare"));
        assert!(citations.is_empty());
    }

    #[test]
    fn skips_malformed_output_entries() {
        let data = json!({"output": [
            null,
            42,
            {"type": "message", "content": "not-an-array"},
            {"type": "message", "content": [null, {"type": "output_text", "text": ""}]},
            {"type": "output_text", "text": "survivor"}
        ]});
        let (text, _) = extract_xai_web_search_content(&data);
        assert_eq!(text.as_deref(), Some("survivor"));
    }

    #[test]
    fn falls_back_to_top_level_output_text_field() {
        let data = json!({"output": [], "output_text": "fallback"});
        let (text, _) = extract_xai_web_search_content(&data);
        assert_eq!(text.as_deref(), Some("fallback"));
    }

    #[test]
    fn returns_none_for_fully_malformed_payload() {
        let (text, citations) = extract_xai_web_search_content(&json!({"output": "junk"}));
        assert!(text.is_none());
        assert!(citations.is_empty());
        let (text, _) = extract_xai_web_search_content(&json!({}));
        assert!(text.is_none());
    }

    #[test]
    fn top_level_citations_win_over_annotations() {
        let data = json!({
            "output": [{"type": "output_text", "text": "t",
                "annotations": [{"type": "url_citation", "url": "https://ann.example"}]}],
            "citations": ["https://top.example"]
        });
        let (content, citations) = resolve_xai_response_text_and_citations(&data);
        assert_eq!(content, "t");
        assert_eq!(citations, vec!["https://top.example"]);
    }

    #[test]
    fn resolve_defaults_to_no_response() {
        let (content, citations) = resolve_xai_response_text_and_citations(&json!({}));
        assert_eq!(content, "No response");
        assert!(citations.is_empty());
    }

    // ------------------------------------------------------------------
    // v2026.5.x helpers
    // ------------------------------------------------------------------

    #[test]
    fn reasoning_effort_only_for_grok_4_3() {
        assert!(xai_supports_reasoning_effort("grok-4.3"));
        assert!(xai_supports_reasoning_effort("xai/grok-4.3"));
        assert!(!xai_supports_reasoning_effort("grok-4"));
        assert!(!xai_supports_reasoning_effort("grok-build-0.1"));
    }

    #[test]
    fn video_job_terminal_states() {
        assert!(grok_video_job_is_terminal("succeeded"));
        assert!(grok_video_job_is_terminal("FAILED"));
        assert!(!grok_video_job_is_terminal("pending"));
        assert!(!grok_video_job_is_terminal("running"));
    }

    #[test]
    fn imagine_qualities_listed() {
        assert!(GROK_IMAGINE_IMAGE_QUALITIES.contains(&"high"));
    }
}
