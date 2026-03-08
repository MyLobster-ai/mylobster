//! Image generation tool via DALL-E or compatible API.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

/// Generate images from text prompts.
pub struct ImageGenerateTool;

#[async_trait]
impl AgentTool for ImageGenerateTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "image_generate".to_string(),
            description: "Generate an image from a text prompt using DALL-E or compatible API"
                .to_string(),
            category: "media".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Text description of the image to generate"
                    },
                    "model": {
                        "type": "string",
                        "description": "Image model to use (e.g. dall-e-3)",
                        "default": "dall-e-3"
                    },
                    "size": {
                        "type": "string",
                        "description": "Image size",
                        "enum": ["1024x1024", "1792x1024", "1024x1792"],
                        "default": "1024x1024"
                    },
                    "quality": {
                        "type": "string",
                        "description": "Image quality",
                        "enum": ["standard", "hd"],
                        "default": "standard"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing prompt parameter"))?;

        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("dall-e-3");

        let size = params
            .get("size")
            .and_then(|v| v.as_str())
            .unwrap_or("1024x1024");

        let quality = params
            .get("quality")
            .and_then(|v| v.as_str())
            .unwrap_or("standard");

        // Resolve API key from config or environment
        let api_key = context
            .config
            .models
            .providers
            .get("openai")
            .and_then(|p| p.api_key.clone())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("No OpenAI API key configured for image generation")
            })?;

        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "n": 1,
            "size": size,
            "quality": quality,
            "response_format": "url"
        });

        let resp = client
            .post("https://api.openai.com/v1/images/generations")
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!(
                "Image generation API returned {}: {}",
                status, err_body
            )));
        }

        let json: serde_json::Value = resp.json().await?;

        let image_url = json["data"][0]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let revised_prompt = json["data"][0]["revised_prompt"]
            .as_str()
            .map(|s| s.to_string());

        if image_url.is_empty() {
            return Ok(ToolResult::error(
                "Image generation succeeded but no URL in response",
            ));
        }

        tracing::info!(
            model,
            size,
            quality,
            prompt_len = prompt.len(),
            "image generated"
        );

        Ok(ToolResult::json(serde_json::json!({
            "url": image_url,
            "model": model,
            "size": size,
            "quality": quality,
            "revised_prompt": revised_prompt
        })))
    }
}
