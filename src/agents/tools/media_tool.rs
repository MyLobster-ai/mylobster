//! Media processing tool.
//!
//! Supports audio transcription (Whisper API), video frame extraction,
//! and image upload/analysis.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;

pub struct MediaTool;

#[async_trait]
impl AgentTool for MediaTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "media".to_string(),
            description: "Process media: transcribe audio, extract video frames, analyze images".to_string(),
            category: "media".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["transcribe", "extract_frames", "analyze_image", "convert"],
                        "description": "Media action to perform"
                    },
                    "path": { "type": "string", "description": "Path to the media file" },
                    "url": { "type": "string", "description": "URL of the media file" },
                    "language": { "type": "string", "description": "Language code for transcription" },
                    "frameInterval": {
                        "type": "number",
                        "description": "Seconds between frame extractions",
                        "default": 1.0
                    },
                    "maxFrames": {
                        "type": "integer",
                        "description": "Maximum frames to extract",
                        "default": 10
                    },
                    "outputFormat": {
                        "type": "string",
                        "description": "Output format for conversion",
                        "enum": ["mp3", "wav", "png", "jpg", "webp"]
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Prompt for image analysis"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing action parameter"))?;

        match action {
            "transcribe" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

                let language = params
                    .get("language")
                    .and_then(|v| v.as_str());

                // Try Whisper API (OpenAI)
                let api_key = context
                    .config
                    .models
                    .providers
                    .get("openai")
                    .and_then(|p| p.api_key.clone())
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok());

                if let Some(api_key) = api_key {
                    return transcribe_whisper(path, language, &api_key).await;
                }

                // Fallback: try whisper CLI
                transcribe_cli(path, language).await
            }
            "extract_frames" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

                let interval = params
                    .get("frameInterval")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0);

                let max_frames = params
                    .get("maxFrames")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as usize;

                extract_video_frames(path, interval, max_frames).await
            }
            "analyze_image" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

                let prompt = params
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Describe this image in detail.");

                // Read image and encode as base64
                let data = tokio::fs::read(path).await?;
                let base64_data = base64::engine::general_purpose::STANDARD.encode(&data);

                let mime = if path.ends_with(".png") {
                    "image/png"
                } else if path.ends_with(".webp") {
                    "image/webp"
                } else if path.ends_with(".gif") {
                    "image/gif"
                } else {
                    "image/jpeg"
                };

                Ok(ToolResult::json(serde_json::json!({
                    "action": "analyze_image",
                    "path": path,
                    "mimeType": mime,
                    "size": data.len(),
                    "base64Length": base64_data.len(),
                    "prompt": prompt,
                    "note": "Image data encoded. Use a vision model for analysis."
                })))
            }
            "convert" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

                let output_format = params
                    .get("outputFormat")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing outputFormat parameter"))?;

                let output_path = format!(
                    "{}.{}",
                    path.rsplit_once('.').map(|(base, _)| base).unwrap_or(path),
                    output_format
                );

                // Use ffmpeg for audio/video conversion
                let output = tokio::process::Command::new("ffmpeg")
                    .args(["-i", path, "-y", &output_path])
                    .output()
                    .await;

                match output {
                    Ok(out) if out.status.success() => {
                        Ok(ToolResult::json(serde_json::json!({
                            "action": "convert",
                            "input": path,
                            "output": output_path,
                            "format": output_format,
                            "success": true
                        })))
                    }
                    Ok(out) => Ok(ToolResult::error(format!(
                        "ffmpeg conversion failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ))),
                    Err(e) => Ok(ToolResult::error(format!(
                        "ffmpeg not available: {}",
                        e
                    ))),
                }
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown media action: {}",
                action
            ))),
        }
    }
}

async fn transcribe_whisper(
    path: &str,
    language: Option<&str>,
    api_key: &str,
) -> Result<ToolResult> {
    let client = reqwest::Client::new();
    let file_data = tokio::fs::read(path).await?;
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.mp3")
        .to_string();

    let file_part = reqwest::multipart::Part::bytes(file_data)
        .file_name(file_name)
        .mime_str("audio/mpeg")?;

    let mut form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", file_part);

    if let Some(lang) = language {
        form = form.text("language", lang.to_string());
    }

    let resp = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Ok(ToolResult::error(format!(
            "Whisper API error ({}): {}",
            status, text
        )));
    }

    let result: serde_json::Value = resp.json().await?;
    Ok(ToolResult::json(result))
}

async fn transcribe_cli(path: &str, language: Option<&str>) -> Result<ToolResult> {
    let mut cmd = tokio::process::Command::new("whisper");
    cmd.arg(path).arg("--output_format").arg("txt");

    if let Some(lang) = language {
        cmd.arg("--language").arg(lang);
    }

    let output = cmd.output().await.map_err(|e| {
        anyhow::anyhow!("Whisper CLI not available and no OpenAI API key configured: {}", e)
    })?;

    if !output.status.success() {
        return Ok(ToolResult::error(format!(
            "Whisper CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(ToolResult::json(serde_json::json!({
        "text": String::from_utf8_lossy(&output.stdout).to_string(),
        "method": "whisper-cli"
    })))
}

async fn extract_video_frames(
    path: &str,
    interval: f64,
    max_frames: usize,
) -> Result<ToolResult> {
    let temp_dir = tempfile::tempdir()?;
    let output_pattern = temp_dir.path().join("frame_%04d.png");

    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-i",
            path,
            "-vf",
            &format!("fps=1/{}", interval),
            "-frames:v",
            &max_frames.to_string(),
            output_pattern.to_str().unwrap(),
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            // Count extracted frames
            let mut frames = Vec::new();
            let mut entries = tokio::fs::read_dir(temp_dir.path()).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("png") {
                    frames.push(path.display().to_string());
                }
            }
            frames.sort();

            Ok(ToolResult::json(serde_json::json!({
                "action": "extract_frames",
                "input": path,
                "frameCount": frames.len(),
                "interval": interval,
                "frames": frames,
                "tempDir": temp_dir.path().display().to_string()
            })))
        }
        Ok(out) => Ok(ToolResult::error(format!(
            "Frame extraction failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
        Err(e) => Ok(ToolResult::error(format!(
            "ffmpeg not available: {}",
            e
        ))),
    }
}

