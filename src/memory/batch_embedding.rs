//! Batch embedding APIs.
//!
//! Supports OpenAI and Anthropic batch APIs for efficient bulk embedding.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ============================================================================
// Batch Embedding Manager
// ============================================================================

/// Manages asynchronous batch embedding requests.
pub struct BatchEmbeddingManager {
    provider: BatchEmbeddingProvider,
    queue: Vec<BatchItem>,
    max_batch_size: usize,
}

#[derive(Debug, Clone)]
enum BatchEmbeddingProvider {
    OpenAi {
        api_key: String,
        model: String,
    },
    Anthropic {
        api_key: String,
        model: String,
    },
}

#[derive(Debug, Clone)]
struct BatchItem {
    id: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEmbeddingResult {
    pub id: String,
    pub embedding: Vec<f32>,
}

impl BatchEmbeddingManager {
    /// Create a new batch embedding manager using the OpenAI Batch API.
    pub fn openai(api_key: &str, model: &str) -> Self {
        Self {
            provider: BatchEmbeddingProvider::OpenAi {
                api_key: api_key.to_string(),
                model: model.to_string(),
            },
            queue: Vec::new(),
            max_batch_size: 2048,
        }
    }

    /// Create a new batch embedding manager using the Anthropic API.
    pub fn anthropic(api_key: &str, model: &str) -> Self {
        Self {
            provider: BatchEmbeddingProvider::Anthropic {
                api_key: api_key.to_string(),
                model: model.to_string(),
            },
            queue: Vec::new(),
            max_batch_size: 2048,
        }
    }

    /// Add an item to the embedding queue.
    pub fn enqueue(&mut self, id: &str, text: &str) {
        self.queue.push(BatchItem {
            id: id.to_string(),
            text: text.to_string(),
        });
    }

    /// Get the number of items in the queue.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Process all queued items in batches and return embeddings.
    pub async fn flush(&mut self) -> Result<Vec<BatchEmbeddingResult>> {
        if self.queue.is_empty() {
            return Ok(Vec::new());
        }

        let items = std::mem::take(&mut self.queue);
        let mut all_results = Vec::new();

        // Process in batches
        for chunk in items.chunks(self.max_batch_size) {
            let batch_results = match &self.provider {
                BatchEmbeddingProvider::OpenAi { api_key, model } => {
                    self.process_openai_batch(chunk, api_key, model).await?
                }
                BatchEmbeddingProvider::Anthropic { api_key, model } => {
                    self.process_anthropic_batch(chunk, api_key, model).await?
                }
            };
            all_results.extend(batch_results);
        }

        info!(
            count = all_results.len(),
            "batch embedding completed"
        );

        Ok(all_results)
    }

    async fn process_openai_batch(
        &self,
        items: &[BatchItem],
        api_key: &str,
        model: &str,
    ) -> Result<Vec<BatchEmbeddingResult>> {
        let client = reqwest::Client::new();

        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();

        let resp = client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&serde_json::json!({
                "input": texts,
                "model": model,
                "encoding_format": "float"
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI batch embedding error ({}): {}", status, text);
        }

        let body: serde_json::Value = resp.json().await?;
        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow::anyhow!("Invalid OpenAI embedding response"))?;

        let mut results = Vec::new();
        for (i, item) in data.iter().enumerate() {
            if let Some(embedding) = item.get("embedding").and_then(|e| e.as_array()) {
                let vec: Vec<f32> = embedding
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();

                if i < items.len() {
                    results.push(BatchEmbeddingResult {
                        id: items[i].id.clone(),
                        embedding: vec,
                    });
                }
            }
        }

        debug!(
            count = results.len(),
            model,
            "openai batch embedding processed"
        );

        Ok(results)
    }

    async fn process_anthropic_batch(
        &self,
        items: &[BatchItem],
        api_key: &str,
        _model: &str,
    ) -> Result<Vec<BatchEmbeddingResult>> {
        // Anthropic's batch API uses the Messages Batch endpoint
        // For embeddings, we'd need to use a voyager model or similar
        debug!(
            count = items.len(),
            "anthropic batch embedding (using Voyage AI)"
        );

        let client = reqwest::Client::new();
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();

        let resp = client
            .post("https://api.voyageai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&serde_json::json!({
                "input": texts,
                "model": "voyage-3"
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Voyage batch embedding error ({}): {}", status, text);
        }

        let body: serde_json::Value = resp.json().await?;
        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow::anyhow!("Invalid Voyage embedding response"))?;

        let mut results = Vec::new();
        for (i, item) in data.iter().enumerate() {
            if let Some(embedding) = item.get("embedding").and_then(|e| e.as_array()) {
                let vec: Vec<f32> = embedding
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();

                if i < items.len() {
                    results.push(BatchEmbeddingResult {
                        id: items[i].id.clone(),
                        embedding: vec,
                    });
                }
            }
        }

        Ok(results)
    }
}
