pub mod batch_embedding;
mod chunking;
mod embeddings;
mod hybrid;
pub mod lancedb;
mod manager;
pub mod postgres;
mod schema;
mod search;

pub use manager::MemoryIndexManager;
pub use search::MemorySearchResult;

use crate::config::Config;
use anyhow::Result;

/// Memory backend selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryBackend {
    /// SQLite with FTS5 + vector (default).
    Sqlite,
    /// PostgreSQL with pgvector + tsvector.
    Postgres,
    /// LanceDB for vector-only search.
    LanceDb,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::Sqlite
    }
}

impl MemoryBackend {
    /// Parse backend from config string.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Self::Postgres,
            "lancedb" | "lance" => Self::LanceDb,
            _ => Self::Sqlite,
        }
    }
}

/// Search memory for relevant content matching `query`.
///
/// This is the primary public entry point for the memory subsystem. It
/// initialises (or re-uses) a `MemoryIndexManager` for the active agent,
/// performs a hybrid BM25 + vector search, and returns de-duplicated,
/// score-sorted results.
///
/// # Arguments
///
/// * `config`      - Application configuration (determines embedding provider,
///                   DB path, chunking params, etc.)
/// * `query`       - Natural-language search query.
/// * `max_results` - Maximum number of results to return.
/// * `min_score`   - Minimum relevance score (0.0 .. 1.0) to include a result.
/// * `session_key` - Optional session key used to scope results.
pub async fn search(
    config: &Config,
    query: &str,
    max_results: u32,
    min_score: f64,
    session_key: Option<&str>,
) -> Result<Vec<MemorySearchResult>> {
    let agent_id = "default";

    let manager = match MemoryIndexManager::get(config, agent_id).await {
        Some(m) => m,
        None => {
            tracing::debug!("memory search unavailable (no embedding provider)");
            return Ok(Vec::new());
        }
    };

    let opts = search::MemorySearchOptions {
        max_results,
        min_score,
        session_key: session_key.map(|s| s.to_string()),
        mode: search::SearchMode::Hybrid,
    };

    Ok(manager.search(query, opts).await)
}
