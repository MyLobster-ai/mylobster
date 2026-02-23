use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{Config, EmbeddingProvider as EmbeddingProviderKind};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A provider that turns text into dense vector embeddings.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Compute embeddings for a batch of texts.
    ///
    /// Returns one vector per input text, each of length [`Self::dimensions`].
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>>;

    /// The model identifier used by this provider (e.g. `text-embedding-3-small`).
    fn model_name(&self) -> String;

    /// Dimensionality of the vectors produced by [`Self::embed`].
    fn dimensions(&self) -> usize;
}

/// Type-erased wrapper so we can store any provider behind a single type.
pub type EmbeddingProviderBox = Box<dyn EmbeddingProvider>;

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create an [`EmbeddingProviderBox`] from the application configuration.
///
/// Returns `None` when the required credentials are missing or the configured
/// provider is not yet supported.
pub fn create_provider(config: &Config) -> Option<EmbeddingProviderBox> {
    let kind = config
        .memory
        .qmd
        .as_ref()
        .and_then(|_| None::<EmbeddingProviderKind>)
        .unwrap_or(EmbeddingProviderKind::Openai);

    match kind {
        EmbeddingProviderKind::Openai => {
            let api_key = config
                .models
                .providers
                .get("openai")
                .and_then(|p| p.api_key.clone())?;
            Some(Box::new(OpenAiEmbeddingProvider::new(api_key, None)))
        }
        EmbeddingProviderKind::Gemini => {
            let api_key = config
                .models
                .providers
                .get("google")
                .and_then(|p| p.api_key.clone())?;
            Some(Box::new(GeminiEmbeddingProvider::new(api_key, None)))
        }
        EmbeddingProviderKind::Mistral => {
            let api_key = config
                .models
                .providers
                .get("mistral")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("MISTRAL_API_KEY").ok())?;
            Some(Box::new(MistralEmbeddingProvider::new(api_key, None)))
        }
        EmbeddingProviderKind::Voyage => {
            let api_key = config
                .models
                .providers
                .get("voyage")
                .and_then(|p| p.api_key.clone())?;
            Some(Box::new(VoyageEmbeddingProvider::new(api_key, None)))
        }
        EmbeddingProviderKind::Local => Some(Box::new(LocalEmbeddingProvider::new(None))),
    }
}

// ---------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------

/// Calls the OpenAI `/v1/embeddings` endpoint.
pub struct OpenAiEmbeddingProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiEmbeddingProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        Self {
            api_key,
            model: model.unwrap_or_else(|| "text-embedding-3-small".to_string()),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct OpenAiEmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f64>,
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = OpenAiEmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let resp = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<OpenAiEmbeddingResponse>()
            .await?;

        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }

    fn dimensions(&self) -> usize {
        1536
    }
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

/// Calls the Google Generative AI embedding endpoint.
pub struct GeminiEmbeddingProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl GeminiEmbeddingProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        Self {
            api_key,
            model: model.unwrap_or_else(|| "text-embedding-004".to_string()),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct GeminiEmbeddingRequest {
    model: String,
    content: GeminiContent,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Deserialize)]
struct GeminiEmbeddingResponse {
    embedding: GeminiEmbeddingValues,
}

#[derive(Deserialize)]
struct GeminiEmbeddingValues {
    values: Vec<f64>,
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            let body = GeminiEmbeddingRequest {
                model: format!("models/{}", self.model),
                content: GeminiContent {
                    parts: vec![GeminiPart { text: text.clone() }],
                },
            };

            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:embedContent?key={}",
                self.model, self.api_key
            );

            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json::<GeminiEmbeddingResponse>()
                .await?;

            results.push(resp.embedding.values);
        }
        Ok(results)
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }

    fn dimensions(&self) -> usize {
        768
    }
}

// ---------------------------------------------------------------------------
// Mistral
// ---------------------------------------------------------------------------

/// Calls the Mistral `/v1/embeddings` endpoint.
pub struct MistralEmbeddingProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl MistralEmbeddingProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        Self {
            api_key,
            model: model.unwrap_or_else(|| "mistral-embed".to_string()),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct MistralEmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct MistralEmbeddingResponse {
    data: Vec<MistralEmbeddingData>,
}

#[derive(Deserialize)]
struct MistralEmbeddingData {
    embedding: Vec<f64>,
}

#[async_trait]
impl EmbeddingProvider for MistralEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = MistralEmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let resp = self
            .client
            .post("https://api.mistral.ai/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<MistralEmbeddingResponse>()
            .await?;

        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }

    fn dimensions(&self) -> usize {
        1024
    }
}

// ---------------------------------------------------------------------------
// Voyage
// ---------------------------------------------------------------------------

/// Calls the Voyage AI embedding endpoint.
pub struct VoyageEmbeddingProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl VoyageEmbeddingProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        Self {
            api_key,
            model: model.unwrap_or_else(|| "voyage-3".to_string()),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct VoyageEmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct VoyageEmbeddingResponse {
    data: Vec<VoyageEmbeddingData>,
}

#[derive(Deserialize)]
struct VoyageEmbeddingData {
    embedding: Vec<f64>,
}

#[async_trait]
impl EmbeddingProvider for VoyageEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = VoyageEmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let resp = self
            .client
            .post("https://api.voyageai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<VoyageEmbeddingResponse>()
            .await?;

        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }

    fn dimensions(&self) -> usize {
        1024
    }
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

/// A local embedding provider that produces zero vectors.
///
/// This is a placeholder implementation. A real local provider would load an
/// ONNX model or similar and run inference on-device.
pub struct LocalEmbeddingProvider {
    dimensions: usize,
}

impl LocalEmbeddingProvider {
    pub fn new(dimensions: Option<usize>) -> Self {
        Self {
            dimensions: dimensions.unwrap_or(384),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        // Placeholder: return zero vectors.
        Ok(texts.iter().map(|_| vec![0.0; self.dimensions]).collect())
    }

    fn model_name(&self) -> String {
        "local-placeholder".to_string()
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
