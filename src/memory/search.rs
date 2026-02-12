use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// A single search result returned by the memory subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    /// The matched text snippet.
    pub text: String,
    /// Source file path (relative to the workspace or index root).
    pub path: String,
    /// Relevance score in the range `[0.0, 1.0]`.
    pub score: f64,
    /// First line number of the matched region (1-based, inclusive).
    pub start_line: u32,
    /// Last line number of the matched region (1-based, inclusive).
    pub end_line: u32,
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Options that control how a memory search is executed.
#[derive(Debug, Clone)]
pub struct MemorySearchOptions {
    /// Maximum number of results to return.
    pub max_results: u32,
    /// Minimum relevance score to include a result.
    pub min_score: f64,
    /// Optional session key for scoping results.
    pub session_key: Option<String>,
    /// Search mode to use.
    pub mode: SearchMode,
}

impl Default for MemorySearchOptions {
    fn default() -> Self {
        Self {
            max_results: 10,
            min_score: 0.0,
            session_key: None,
            mode: SearchMode::Hybrid,
        }
    }
}

/// The strategy used to execute a memory search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// BM25 full-text search only.
    Fts,
    /// Vector similarity search only.
    Vector,
    /// Hybrid: merge BM25 and vector results via reciprocal rank fusion.
    Hybrid,
}

// ---------------------------------------------------------------------------
// Low-level query helpers (stubs)
// ---------------------------------------------------------------------------

/// Perform a BM25 full-text search against the `chunks_fts` table.
///
/// Returns `(chunk_id, bm25_score)` pairs in descending score order.
pub fn fts_search(_conn: &rusqlite::Connection, _query: &str, _limit: u32) -> Vec<(i64, f64)> {
    // TODO: execute FTS5 MATCH query and return ranked results.
    Vec::new()
}

/// Perform a brute-force vector similarity search over `chunks.embedding`.
///
/// Returns `(chunk_id, cosine_similarity)` pairs in descending order.
pub fn vector_search(
    _conn: &rusqlite::Connection,
    _query_embedding: &[f64],
    _limit: u32,
) -> Vec<(i64, f64)> {
    // TODO: iterate over stored embeddings and compute cosine similarity.
    Vec::new()
}

/// Load full chunk details for a set of chunk IDs and build
/// [`MemorySearchResult`] values.
pub fn load_results(
    _conn: &rusqlite::Connection,
    _scored_ids: &[(i64, f64)],
) -> Vec<MemorySearchResult> {
    // TODO: JOIN chunks + files to populate MemorySearchResult fields.
    Vec::new()
}
