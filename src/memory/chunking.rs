/// A contiguous chunk of text extracted from a source file, ready for
/// embedding.
#[derive(Debug, Clone)]
pub struct TextChunk {
    /// The chunk text.
    pub text: String,
    /// 1-based start line in the original file (inclusive).
    pub start_line: u32,
    /// 1-based end line in the original file (inclusive).
    pub end_line: u32,
    /// Approximate token count (whitespace-split heuristic).
    pub token_count: u32,
}

/// Split `content` into chunks of approximately `max_tokens` tokens with
/// `overlap` tokens of overlap between consecutive chunks.
///
/// The tokenisation is a simple whitespace split (word count). For production
/// accuracy this should be replaced with a proper tokeniser (e.g. tiktoken),
/// but the whitespace heuristic is good enough for chunking decisions.
///
/// Line numbers are tracked so that search results can reference the original
/// file location.
pub fn chunk_text(content: &str, max_tokens: u32, overlap: u32) -> Vec<TextChunk> {
    if content.is_empty() {
        return Vec::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let max_tokens = max_tokens.max(1) as usize;
    let overlap = (overlap as usize).min(max_tokens.saturating_sub(1));

    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_words: Vec<&str> = Vec::new();
    let mut current_start_line: u32 = 1;
    let mut current_end_line: u32 = 1;

    for (line_idx, line) in lines.iter().enumerate() {
        let line_number = (line_idx as u32) + 1;
        let words: Vec<&str> = line.split_whitespace().collect();

        if words.is_empty() {
            // Blank lines still advance the end line but add no tokens.
            current_end_line = line_number;
            continue;
        }

        for word in &words {
            current_words.push(word);
            current_end_line = line_number;

            if current_words.len() >= max_tokens {
                let text = current_words.join(" ");
                chunks.push(TextChunk {
                    text,
                    start_line: current_start_line,
                    end_line: current_end_line,
                    token_count: current_words.len() as u32,
                });

                // Retain the last `overlap` words for the next chunk.
                let keep_from = current_words.len().saturating_sub(overlap);
                current_words = current_words[keep_from..].to_vec();
                current_start_line = current_end_line;
            }
        }
    }

    // Flush remaining words as the final chunk.
    if !current_words.is_empty() {
        let text = current_words.join(" ");
        chunks.push(TextChunk {
            text,
            start_line: current_start_line,
            end_line: current_end_line,
            token_count: current_words.len() as u32,
        });
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_content() {
        let chunks = chunk_text("", 256, 32);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_single_short_line() {
        let chunks = chunk_text("hello world", 256, 32);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
        assert_eq!(chunks[0].token_count, 2);
    }

    #[test]
    fn test_splits_at_max_tokens() {
        // 10 words, max 5 tokens, no overlap.
        let content = "a b c d e f g h i j";
        let chunks = chunk_text(content, 5, 0);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "a b c d e");
        assert_eq!(chunks[1].text, "f g h i j");
    }

    #[test]
    fn test_overlap() {
        // 10 words, max 5 tokens, overlap 2.
        let content = "a b c d e f g h i j";
        let chunks = chunk_text(content, 5, 2);
        assert!(chunks.len() >= 2);
        // Second chunk should start with the last 2 words of the first.
        assert!(chunks[1].text.starts_with("d e"));
    }

    #[test]
    fn test_multiline_tracking() {
        let content = "line one\nline two\nline three\nline four";
        let chunks = chunk_text(content, 4, 0);
        // "line one line two" = 4 words -> chunk 1
        assert_eq!(chunks[0].start_line, 1);
        assert!(chunks[0].end_line <= 2);
    }
}
