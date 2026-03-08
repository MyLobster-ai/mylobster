//! Text-to-speech tool.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

/// Convert text to speech audio.
pub struct TtsSpeakTool;

#[async_trait]
impl AgentTool for TtsSpeakTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "tts_speak".to_string(),
            description: "Convert text to speech audio using ElevenLabs or system TTS".to_string(),
            category: "media".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to convert to speech"
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice ID or name (default: Rachel for ElevenLabs, system default otherwise)",
                        "default": "21m00Tcm4TlvDq8ikWAM"
                    },
                    "outputPath": {
                        "type": "string",
                        "description": "Optional file path to save the audio"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing text parameter"))?;

        let voice = params
            .get("voice")
            .and_then(|v| v.as_str())
            .unwrap_or("21m00Tcm4TlvDq8ikWAM"); // Rachel (ElevenLabs default)

        let output_path = params.get("outputPath").and_then(|v| v.as_str());

        // Create TTS manager from environment
        let tts = crate::tts::TtsManager::from_env().await.map_err(|e| {
            anyhow::anyhow!("TTS not available: {}", e)
        })?;

        let provider_name = tts.provider_name().to_string();

        // Generate audio
        let audio_bytes = tts.generate(text, voice).await?;

        // Save to file if output path specified, otherwise use temp file
        let save_path = if let Some(path) = output_path {
            let p = std::path::PathBuf::from(path);
            tokio::fs::write(&p, &audio_bytes).await?;
            p
        } else {
            let audio_dir = context.config.state_dir.join("audio");
            let _ = tokio::fs::create_dir_all(&audio_dir).await;
            let filename = format!("tts_{}.mp3", uuid::Uuid::new_v4());
            let p = audio_dir.join(&filename);
            tokio::fs::write(&p, &audio_bytes).await?;
            p
        };

        tracing::info!(
            provider = %provider_name,
            voice,
            bytes = audio_bytes.len(),
            path = %save_path.display(),
            "TTS audio generated"
        );

        Ok(ToolResult::json(serde_json::json!({
            "generated": true,
            "provider": provider_name,
            "voice": voice,
            "bytes": audio_bytes.len(),
            "path": save_path.display().to_string(),
            "text_chars": text.len()
        })))
    }
}
