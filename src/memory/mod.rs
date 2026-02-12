mod chunking;
mod embeddings;
mod hybrid;
mod manager;
mod schema;
mod search;

pub use manager::MemoryIndexManager;
pub use search::MemorySearchResult;

use crate::config::Config;
use anyhow::Result;

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
    _config: &Config,
    _query: &str,
    _max_results: u32,
    _min_score: f64,
    _session_key: Option<&str>,
) -> Result<Vec<MemorySearchResult>> {
    // TODO: initialise MemoryIndexManager and delegate to its search method.
    Ok(Vec::new())
}
