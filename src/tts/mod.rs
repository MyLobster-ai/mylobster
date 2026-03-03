use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TTS voice descriptor
// ---------------------------------------------------------------------------

/// Metadata for a single TTS voice offered by a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsVoice {
    pub id: String,
    pub name: String,
    pub language: String,
    pub gender: Option<String>,
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

/// Trait implemented by every TTS backend.
#[async_trait::async_trait]
pub trait TtsProvider: Send + Sync {
    /// Human-readable provider name.
    fn name(&self) -> &str;

    /// Generate audio bytes for the given `text` using `voice`.
    async fn generate(&self, text: &str, voice: &str) -> Result<Vec<u8>>;

    /// List the voices this provider supports.
    fn available_voices(&self) -> Vec<TtsVoice>;
}

// ---------------------------------------------------------------------------
// ElevenLabs provider
// ---------------------------------------------------------------------------

/// TTS provider backed by the ElevenLabs streaming API.
pub struct ElevenLabsTtsProvider {
    api_key: String,
    client: reqwest::Client,
}

impl ElevenLabsTtsProvider {
    /// Create a new ElevenLabs provider with the supplied API key.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl TtsProvider for ElevenLabsTtsProvider {
    fn name(&self) -> &str {
        "elevenlabs"
    }

    async fn generate(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://api.elevenlabs.io/v1/text-to-speech/{}/stream",
            voice
        );

        let body = serde_json::json!({
            "text": text,
            "model_id": "eleven_monolingual_v1",
            "voice_settings": {
                "stability": 0.5,
                "similarity_boost": 0.75
            }
        });

        let response = self
            .client
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "audio/mpeg")
            .json(&body)
            .send()
            .await
            .context("ElevenLabs TTS request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "ElevenLabs API returned {}: {}",
                status,
                err_body
            ));
        }

        let audio_bytes = response
            .bytes()
            .await
            .context("Failed to read ElevenLabs audio stream")?;

        tracing::debug!(
            bytes = audio_bytes.len(),
            voice,
            "ElevenLabs TTS generated audio"
        );
        Ok(audio_bytes.to_vec())
    }

    fn available_voices(&self) -> Vec<TtsVoice> {
        // Commonly available ElevenLabs voices (pre-made).
        vec![
            TtsVoice {
                id: "21m00Tcm4TlvDq8ikWAM".into(),
                name: "Rachel".into(),
                language: "en".into(),
                gender: Some("female".into()),
            },
            TtsVoice {
                id: "AZnzlk1XvdvUeBnXmlld".into(),
                name: "Domi".into(),
                language: "en".into(),
                gender: Some("female".into()),
            },
            TtsVoice {
                id: "EXAVITQu4vr4xnSDxMaL".into(),
                name: "Bella".into(),
                language: "en".into(),
                gender: Some("female".into()),
            },
            TtsVoice {
                id: "ErXwobaYiN019PkySvjV".into(),
                name: "Antoni".into(),
                language: "en".into(),
                gender: Some("male".into()),
            },
            TtsVoice {
                id: "VR6AewLTigWG4xSOukaG".into(),
                name: "Arnold".into(),
                language: "en".into(),
                gender: Some("male".into()),
            },
            TtsVoice {
                id: "pNInz6obpgDQGcFmaJgB".into(),
                name: "Adam".into(),
                language: "en".into(),
                gender: Some("male".into()),
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// System TTS provider (macOS `say` / Linux `espeak`)
// ---------------------------------------------------------------------------

/// TTS provider that shells out to platform-native speech synthesis.
///
/// * macOS: uses the `say` command (outputs AIFF, returned as raw bytes).
/// * Linux: uses `espeak --stdout` (outputs WAV on stdout).
pub struct SystemTtsProvider;

impl SystemTtsProvider {
    pub fn new() -> Self {
        Self
    }

    /// Returns `true` when a system TTS binary is available on this platform.
    pub async fn is_available() -> bool {
        if cfg!(target_os = "macos") {
            which_exists("say").await
        } else if cfg!(target_os = "linux") {
            which_exists("espeak").await
        } else {
            false
        }
    }
}

#[async_trait::async_trait]
impl TtsProvider for SystemTtsProvider {
    fn name(&self) -> &str {
        "system"
    }

    async fn generate(&self, text: &str, _voice: &str) -> Result<Vec<u8>> {
        if cfg!(target_os = "macos") {
            generate_macos(text).await
        } else if cfg!(target_os = "linux") {
            generate_linux(text).await
        } else {
            Err(anyhow!("System TTS is not supported on this platform"))
        }
    }

    fn available_voices(&self) -> Vec<TtsVoice> {
        if cfg!(target_os = "macos") {
            vec![TtsVoice {
                id: "default".into(),
                name: "System Default".into(),
                language: "en".into(),
                gender: None,
            }]
        } else if cfg!(target_os = "linux") {
            vec![TtsVoice {
                id: "default".into(),
                name: "espeak Default".into(),
                language: "en".into(),
                gender: None,
            }]
        } else {
            vec![]
        }
    }
}

/// macOS: generate audio via `say`.
async fn generate_macos(text: &str) -> Result<Vec<u8>> {
    let tmp = tempfile::Builder::new()
        .suffix(".aiff")
        .tempfile()
        .context("Failed to create temp file for TTS output")?;
    let output_path = tmp.path().to_path_buf();

    let status = tokio::process::Command::new("say")
        .arg("-o")
        .arg(&output_path)
        .arg("--data-format=LEF32@22050")
        .arg(text)
        .status()
        .await
        .context("Failed to execute `say` command")?;

    if !status.success() {
        return Err(anyhow!("`say` exited with status {}", status));
    }

    let audio = tokio::fs::read(&output_path)
        .await
        .context("Failed to read TTS output file")?;

    tracing::debug!(bytes = audio.len(), "macOS TTS generated audio");
    Ok(audio)
}

/// Linux: generate audio via `espeak --stdout`.
async fn generate_linux(text: &str) -> Result<Vec<u8>> {
    let output = tokio::process::Command::new("espeak")
        .arg("--stdout")
        .arg(text)
        .output()
        .await
        .context("Failed to execute `espeak` command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("`espeak` failed: {}", stderr));
    }

    tracing::debug!(bytes = output.stdout.len(), "Linux TTS generated audio");
    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// TTS Manager
// ---------------------------------------------------------------------------

/// High-level manager that selects and delegates to a [`TtsProvider`].
pub struct TtsManager {
    provider: Box<dyn TtsProvider>,
}

impl TtsManager {
    /// Create a manager with an explicit provider.
    pub fn new(provider: Box<dyn TtsProvider>) -> Self {
        Self { provider }
    }

    /// Create a manager by inspecting the environment:
    ///
    /// 1. If `ELEVENLABS_API_KEY` is set, use ElevenLabs.
    /// 2. Otherwise, fall back to the system provider.
    pub async fn from_env() -> Result<Self> {
        if let Ok(key) = std::env::var("ELEVENLABS_API_KEY") {
            if !key.is_empty() {
                tracing::info!("TTS: using ElevenLabs provider");
                return Ok(Self::new(Box::new(ElevenLabsTtsProvider::new(key))));
            }
        }

        if SystemTtsProvider::is_available().await {
            tracing::info!("TTS: using system provider");
            return Ok(Self::new(Box::new(SystemTtsProvider::new())));
        }

        Err(anyhow!(
            "No TTS provider available. Set ELEVENLABS_API_KEY or install espeak/say."
        ))
    }

    /// Generate audio bytes using the active provider.
    pub async fn generate(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        self.provider.generate(text, voice).await
    }

    /// Generate audio and write it to `output_path`.
    pub async fn generate_to_file(
        &self,
        text: &str,
        voice: &str,
        output_path: &std::path::Path,
    ) -> Result<PathBuf> {
        let audio = self.generate(text, voice).await?;
        tokio::fs::write(output_path, &audio)
            .await
            .with_context(|| format!("Failed to write TTS output to {:?}", output_path))?;
        tracing::info!(path = ?output_path, bytes = audio.len(), "TTS audio saved");
        Ok(output_path.to_path_buf())
    }

    /// Name of the active provider.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// List available voices from the active provider.
    pub fn available_voices(&self) -> Vec<TtsVoice> {
        self.provider.available_voices()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a binary exists on `$PATH` using `which`.
async fn which_exists(binary: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(binary)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
