/// Default maximum image dimension in pixels (reduced from 2048 in v2026.2.17).
pub const DEFAULT_IMAGE_MAX_DIMENSION_PX: u32 = 1200;

/// Default maximum image file size in bytes (5 MB).
pub const DEFAULT_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Limits applied when sanitizing images before sending to AI providers.
pub struct ImageSanitizationLimits {
    pub max_dimension_px: u32,
    pub max_bytes: usize,
}

/// Resolve image sanitization limits from config, falling back to defaults.
pub fn resolve_limits(config_dim: Option<u32>) -> ImageSanitizationLimits {
    ImageSanitizationLimits {
        max_dimension_px: config_dim.unwrap_or(DEFAULT_IMAGE_MAX_DIMENSION_PX),
        max_bytes: DEFAULT_IMAGE_MAX_BYTES,
    }
}

// ---------------------------------------------------------------------------
// Image processing (delegates to ImageMagick CLI)
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Image manipulation via ImageMagick CLI (`convert` / `identify`).
///
/// All operations degrade gracefully: when ImageMagick is not installed the
/// input data is returned unchanged (for `resize`) or an error is returned.
pub struct ImageProcessor;

impl ImageProcessor {
    /// Resize an image so that neither dimension exceeds `max_dim`.
    ///
    /// Returns the original bytes when ImageMagick is unavailable.
    pub async fn resize(data: &[u8], max_dim: u32) -> Result<Vec<u8>> {
        if !binary_exists("convert").await {
            tracing::warn!("ImageMagick not found -- returning original image data");
            return Ok(data.to_vec());
        }

        let input = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .context("create temp input")?;
        tokio::fs::write(input.path(), data).await?;

        let output = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .context("create temp output")?;

        let geometry = format!("{}x{}>", max_dim, max_dim);
        let status = tokio::process::Command::new("convert")
            .arg(input.path())
            .arg("-resize")
            .arg(&geometry)
            .arg(output.path())
            .status()
            .await
            .context("convert resize")?;

        if !status.success() {
            return Err(anyhow!("convert resize exited with {}", status));
        }

        let out = tokio::fs::read(output.path()).await?;
        tracing::debug!(
            original = data.len(),
            resized = out.len(),
            max_dim,
            "image resized"
        );
        Ok(out)
    }

    /// Convert an image between formats (e.g. "png" -> "jpeg").
    pub async fn convert_format(data: &[u8], from: &str, to: &str) -> Result<Vec<u8>> {
        if !binary_exists("convert").await {
            return Err(anyhow!("ImageMagick `convert` not found"));
        }

        let in_suffix = format!(".{}", from);
        let out_suffix = format!(".{}", to);

        let input = tempfile::Builder::new()
            .suffix(&in_suffix)
            .tempfile()
            .context("create temp input")?;
        tokio::fs::write(input.path(), data).await?;

        let output = tempfile::Builder::new()
            .suffix(&out_suffix)
            .tempfile()
            .context("create temp output")?;

        let status = tokio::process::Command::new("convert")
            .arg(input.path())
            .arg(output.path())
            .status()
            .await
            .context("convert format")?;

        if !status.success() {
            return Err(anyhow!("convert format exited with {}", status));
        }

        let out = tokio::fs::read(output.path()).await?;
        tracing::debug!(from, to, bytes = out.len(), "image format converted");
        Ok(out)
    }

    /// Return (width, height) of an image using ImageMagick `identify`.
    pub async fn get_dimensions(data: &[u8]) -> Result<(u32, u32)> {
        if !binary_exists("identify").await {
            return Err(anyhow!("ImageMagick `identify` not found"));
        }

        let input = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .context("create temp input")?;
        tokio::fs::write(input.path(), data).await?;

        let output = tokio::process::Command::new("identify")
            .arg("-format")
            .arg("%w %h")
            .arg(input.path())
            .output()
            .await
            .context("identify dimensions")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("identify failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = stdout.trim().split_whitespace().collect();
        if parts.len() < 2 {
            return Err(anyhow!("unexpected identify output: {}", stdout));
        }

        let w: u32 = parts[0].parse().context("parse width")?;
        let h: u32 = parts[1].parse().context("parse height")?;
        Ok((w, h))
    }
}

// ---------------------------------------------------------------------------
// Audio processing (Whisper + ffmpeg)
// ---------------------------------------------------------------------------

/// Audio transcription and format conversion.
pub struct AudioProcessor;

impl AudioProcessor {
    /// Transcribe an audio file to text.
    ///
    /// If `api_key` is provided, calls the OpenAI Whisper API.
    /// Otherwise falls back to the local `whisper` CLI.
    pub async fn transcribe(audio_path: &Path, api_key: Option<&str>) -> Result<String> {
        if let Some(key) = api_key {
            Self::transcribe_openai(audio_path, key).await
        } else {
            Self::transcribe_local(audio_path).await
        }
    }

    /// Transcribe via OpenAI Whisper API.
    async fn transcribe_openai(audio_path: &Path, api_key: &str) -> Result<String> {
        let file_bytes = tokio::fs::read(audio_path).await.context("read audio file")?;
        let file_name = audio_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name)
            .mime_str("audio/mpeg")?;

        let form = reqwest::multipart::Form::new()
            .text("model", "whisper-1")
            .part("file", part);

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .header("Authorization", format!("Bearer {}", api_key))
            .multipart(form)
            .send()
            .await
            .context("Whisper API request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Whisper API returned {}: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await.context("parse Whisper response")?;
        let text = json["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        tracing::info!(chars = text.len(), "Whisper API transcription complete");
        Ok(text)
    }

    /// Transcribe via local `whisper` CLI.
    async fn transcribe_local(audio_path: &Path) -> Result<String> {
        if !binary_exists("whisper").await {
            return Err(anyhow!(
                "No transcription backend available. Set OPENAI_API_KEY or install `whisper` CLI."
            ));
        }

        let output = tokio::process::Command::new("whisper")
            .arg(audio_path)
            .arg("--model")
            .arg("base")
            .arg("--output_format")
            .arg("txt")
            .arg("--output_dir")
            .arg(
                audio_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .output()
            .await
            .context("whisper CLI")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("whisper CLI failed: {}", stderr));
        }

        // whisper writes <stem>.txt alongside the input file
        let txt_path = audio_path.with_extension("txt");
        let text = tokio::fs::read_to_string(&txt_path)
            .await
            .with_context(|| format!("read whisper output {:?}", txt_path))?;

        tracing::info!(chars = text.len(), "Local whisper transcription complete");
        Ok(text.trim().to_string())
    }

    /// Convert audio between formats via `ffmpeg`.
    pub async fn convert_audio(input: &Path, output: &Path, format: &str) -> Result<()> {
        if !binary_exists("ffmpeg").await {
            return Err(anyhow!("`ffmpeg` not found"));
        }

        let status = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-f")
            .arg(format)
            .arg(output)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("ffmpeg convert")?;

        if !status.success() {
            return Err(anyhow!("ffmpeg convert exited with {}", status));
        }

        tracing::debug!(?input, ?output, format, "audio converted");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Video processing (ffmpeg / ffprobe)
// ---------------------------------------------------------------------------

/// Video frame extraction and metadata via `ffmpeg` / `ffprobe`.
pub struct VideoProcessor;

impl VideoProcessor {
    /// Extract frames from a video at the given interval (seconds).
    ///
    /// Writes PNG images to `output_dir` and returns their paths.
    pub async fn extract_frames(
        video_path: &Path,
        interval_secs: f64,
        output_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        if !binary_exists("ffmpeg").await {
            return Err(anyhow!("`ffmpeg` not found"));
        }

        tokio::fs::create_dir_all(output_dir)
            .await
            .context("create output dir")?;

        let fps_filter = format!("fps=1/{}", interval_secs);
        let output_pattern = output_dir.join("frame_%04d.png");

        let status = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(video_path)
            .arg("-vf")
            .arg(&fps_filter)
            .arg(&output_pattern)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("ffmpeg extract frames")?;

        if !status.success() {
            return Err(anyhow!("ffmpeg extract_frames exited with {}", status));
        }

        // Collect generated frame files.
        let mut frames = Vec::new();
        let mut entries = tokio::fs::read_dir(output_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("png") {
                frames.push(path);
            }
        }
        frames.sort();

        tracing::info!(count = frames.len(), ?video_path, "extracted frames");
        Ok(frames)
    }

    /// Get the duration of a video in seconds via `ffprobe`.
    pub async fn get_duration(video_path: &Path) -> Result<f64> {
        if !binary_exists("ffprobe").await {
            return Err(anyhow!("`ffprobe` not found"));
        }

        let output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(video_path)
            .output()
            .await
            .context("ffprobe duration")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("ffprobe failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let duration: f64 = stdout.trim().parse().context("parse duration")?;
        Ok(duration)
    }
}

// ---------------------------------------------------------------------------
// File uploader
// ---------------------------------------------------------------------------

/// Uploads files to the MyLobster fileserver.
pub struct FileUploader;

impl FileUploader {
    /// Upload a local file to the fileserver via multipart POST.
    ///
    /// Returns the download URL on success.
    pub async fn upload(file_path: &Path, fileserver_url: &str) -> Result<String> {
        let file_bytes = tokio::fs::read(file_path)
            .await
            .with_context(|| format!("read file {:?}", file_path))?;

        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str("application/octet-stream")?;

        let form = reqwest::multipart::Form::new().part("file", part);

        let upload_url = format!("{}/api/internal/upload", fileserver_url.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let resp = client
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("upload to {}", upload_url))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Fileserver returned {}: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await.context("parse upload response")?;
        let download_url = json["url"]
            .as_str()
            .or_else(|| json["download_url"].as_str())
            .unwrap_or_default()
            .to_string();

        if download_url.is_empty() {
            return Err(anyhow!(
                "Fileserver response did not contain a download URL"
            ));
        }

        tracing::info!(file = %file_name, url = %download_url, "file uploaded");
        Ok(download_url)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Check if a binary exists on `$PATH`.
async fn binary_exists(name: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_IMAGE_MAX_DIMENSION_PX, 1200);
        assert_eq!(DEFAULT_IMAGE_MAX_BYTES, 5 * 1024 * 1024);
    }

    #[test]
    fn test_resolve_limits_with_config() {
        let limits = resolve_limits(Some(800));
        assert_eq!(limits.max_dimension_px, 800);
        assert_eq!(limits.max_bytes, DEFAULT_IMAGE_MAX_BYTES);
    }

    #[test]
    fn test_resolve_limits_without_config() {
        let limits = resolve_limits(None);
        assert_eq!(limits.max_dimension_px, DEFAULT_IMAGE_MAX_DIMENSION_PX);
        assert_eq!(limits.max_bytes, DEFAULT_IMAGE_MAX_BYTES);
    }
}
