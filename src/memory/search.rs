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
// Low-level query helpers
// ---------------------------------------------------------------------------

/// Perform a BM25 full-text search against the `chunks_fts` table.
///
/// Returns `(chunk_id, bm25_score)` pairs in descending score order.
pub fn fts_search(conn: &rusqlite::Connection, query: &str, limit: u32) -> Vec<(i64, f64)> {
    // FTS5 MATCH query with BM25 ranking
    let sql = "SELECT rowid, rank FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY rank LIMIT ?2";

    let mut results = Vec::new();
    if let Ok(mut stmt) = conn.prepare(sql) {
        let rows = stmt.query_map(rusqlite::params![query, limit], |row| {
            let id: i64 = row.get(0)?;
            let rank: f64 = row.get(1)?;
            // FTS5 rank is negative (lower = better match), invert for score
            Ok((id, -rank))
        });

        if let Ok(rows) = rows {
            for row in rows.flatten() {
                results.push(row);
            }
        }
    }

    // Sort by descending score (highest relevance first)
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Perform a brute-force vector similarity search over `chunks.embedding`.
///
/// Returns `(chunk_id, cosine_similarity)` pairs in descending order.
pub fn vector_search(
    conn: &rusqlite::Connection,
    query_embedding: &[f64],
    limit: u32,
) -> Vec<(i64, f64)> {
    if query_embedding.is_empty() {
        return Vec::new();
    }

    let sql = "SELECT id, embedding FROM chunks WHERE embedding IS NOT NULL";

    let mut results = Vec::new();
    if let Ok(mut stmt) = conn.prepare(sql) {
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        });

        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (id, blob) = row;
                if let Some(embedding) = deserialize_embedding(&blob) {
                    let sim = cosine_similarity(query_embedding, &embedding);
                    if sim > 0.0 {
                        results.push((id, sim));
                    }
                }
            }
        }
    }

    // Sort by descending similarity
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit as usize);
    results
}

/// Load full chunk details for a set of chunk IDs and build
/// [`MemorySearchResult`] values.
pub fn load_results(
    conn: &rusqlite::Connection,
    scored_ids: &[(i64, f64)],
) -> Vec<MemorySearchResult> {
    if scored_ids.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();

    for &(chunk_id, score) in scored_ids {
        let sql = "SELECT c.text, c.start_line, c.end_line, f.path \
                   FROM chunks c JOIN files f ON c.file_id = f.id \
                   WHERE c.id = ?1";

        if let Ok(mut stmt) = conn.prepare(sql) {
            let row = stmt.query_row(rusqlite::params![chunk_id], |row| {
                let text: String = row.get(0)?;
                let start_line: u32 = row.get(1)?;
                let end_line: u32 = row.get(2)?;
                let path: String = row.get(3)?;
                Ok(MemorySearchResult {
                    text,
                    path,
                    score,
                    start_line,
                    end_line,
                })
            });

            if let Ok(result) = row {
                results.push(result);
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f64::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

/// Deserialize a blob of f64 values stored as little-endian bytes.
fn deserialize_embedding(blob: &[u8]) -> Option<Vec<f64>> {
    if blob.len() % 8 != 0 {
        return None;
    }

    let count = blob.len() / 8;
    let mut embedding = Vec::with_capacity(count);
    for i in 0..count {
        let bytes: [u8; 8] = blob[i * 8..(i + 1) * 8].try_into().ok()?;
        embedding.push(f64::from_le_bytes(bytes));
    }
    Some(embedding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn test_deserialize_embedding() {
        let val: f64 = 1.5;
        let bytes = val.to_le_bytes();
        let result = deserialize_embedding(&bytes).unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0] - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_deserialize_embedding_invalid() {
        assert!(deserialize_embedding(&[1, 2, 3]).is_none()); // not divisible by 8
    }
}
