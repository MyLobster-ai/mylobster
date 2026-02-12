use std::collections::HashMap;

/// Weight applied to BM25 (FTS) scores during reciprocal rank fusion.
const FTS_WEIGHT: f64 = 1.0;

/// Weight applied to vector similarity scores during reciprocal rank fusion.
const VECTOR_WEIGHT: f64 = 1.0;

/// Constant `k` in the RRF formula: `1 / (k + rank)`.
const RRF_K: f64 = 60.0;

/// Merge BM25 and vector search result lists using Reciprocal Rank Fusion
/// (RRF).
///
/// Each input is a list of `(chunk_id, score)` pairs **already sorted by
/// descending score**. The function fuses them into a single ranked list,
/// de-duplicates by `chunk_id`, and returns the top `limit` results.
///
/// # Algorithm
///
/// For each result list the rank-based RRF score is computed as:
///
/// ```text
/// rrf_score = weight / (k + rank)
/// ```
///
/// where `rank` is the 1-based position in the sorted list. Scores from
/// both lists are summed per `chunk_id` and the merged list is sorted by
/// total RRF score in descending order.
pub fn merge_results(
    fts_results: &[(i64, f64)],
    vector_results: &[(i64, f64)],
    limit: u32,
) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();

    for (rank, &(id, _score)) in fts_results.iter().enumerate() {
        let rrf = FTS_WEIGHT / (RRF_K + (rank as f64 + 1.0));
        *scores.entry(id).or_default() += rrf;
    }

    for (rank, &(id, _score)) in vector_results.iter().enumerate() {
        let rrf = VECTOR_WEIGHT / (RRF_K + (rank as f64 + 1.0));
        *scores.entry(id).or_default() += rrf;
    }

    let mut merged: Vec<(i64, f64)> = scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(limit as usize);
    merged
}

/// Normalise a list of raw scores to the `[0.0, 1.0]` range using min-max
/// scaling.
///
/// If all scores are identical, every entry is mapped to `1.0`.
pub fn normalise_scores(results: &mut [(i64, f64)]) {
    if results.is_empty() {
        return;
    }

    let min = results.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let max = results
        .iter()
        .map(|r| r.1)
        .fold(f64::NEG_INFINITY, f64::max);

    let range = max - min;
    if range.abs() < f64::EPSILON {
        for r in results.iter_mut() {
            r.1 = 1.0;
        }
    } else {
        for r in results.iter_mut() {
            r.1 = (r.1 - min) / range;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_empty() {
        let merged = merge_results(&[], &[], 10);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_merge_single_list() {
        let fts = vec![(1, 5.0), (2, 3.0)];
        let merged = merge_results(&fts, &[], 10);
        assert_eq!(merged.len(), 2);
        // First item should have higher RRF score (lower rank).
        assert!(merged[0].1 >= merged[1].1);
    }

    #[test]
    fn test_merge_overlapping() {
        let fts = vec![(1, 5.0), (2, 3.0), (3, 1.0)];
        let vec_results = vec![(2, 0.95), (4, 0.80), (1, 0.70)];
        let merged = merge_results(&fts, &vec_results, 10);
        // Chunk 2 appears in both — should have the highest combined score.
        assert_eq!(merged[0].0, 2);
    }

    #[test]
    fn test_normalise_scores() {
        let mut data = vec![(1, 2.0), (2, 4.0), (3, 6.0)];
        normalise_scores(&mut data);
        assert!((data[0].1 - 0.0).abs() < f64::EPSILON);
        assert!((data[1].1 - 0.5).abs() < f64::EPSILON);
        assert!((data[2].1 - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_normalise_uniform() {
        let mut data = vec![(1, 3.0), (2, 3.0)];
        normalise_scores(&mut data);
        assert!((data[0].1 - 1.0).abs() < f64::EPSILON);
        assert!((data[1].1 - 1.0).abs() < f64::EPSILON);
    }
}
