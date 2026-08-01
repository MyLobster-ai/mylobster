//! Discord outbound chunking (v2026.7.1).
//!
//! Port of OpenClaw `extensions/discord/src/chunk.ts`: chunks outbound Discord
//! text by both character count and (soft) line count while keeping fenced
//! code blocks balanced across chunks — long replies with code fences stay
//! valid near the 2000-char Discord message limit.
//!
//! Bundled-native port; upstream ships this inside the Discord npm plugin.

/// Max characters per Discord message.
pub const DISCORD_DEFAULT_MAX_CHARS: usize = 2000;
/// Soft max line count per message (Discord clients clip very tall messages).
pub const DISCORD_DEFAULT_MAX_LINES: usize = 17;

const CJK_PUNCTUATION_BREAK_AFTER: &str = "、。，．！？；：）］｝〉》」』】〕〗〙";

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenFence {
    indent: String,
    marker_char: char,
    marker_len: usize,
    open_line: String,
}

fn parse_fence_line(line: &str) -> Option<OpenFence> {
    // ^( {0,3})(`{3,}|~{3,})(.*)$
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && idx < 3 && bytes[idx] == b' ' {
        idx += 1;
    }
    let indent = &line[..idx];
    let marker_char = match bytes.get(idx) {
        Some(b'`') => '`',
        Some(b'~') => '~',
        _ => return None,
    };
    let mut marker_len = 0;
    while bytes.get(idx + marker_len) == Some(&(marker_char as u8)) {
        marker_len += 1;
    }
    if marker_len < 3 {
        return None;
    }
    Some(OpenFence {
        indent: indent.to_string(),
        marker_char,
        marker_len,
        open_line: line.to_string(),
    })
}

fn close_fence_line(fence: &OpenFence) -> String {
    format!(
        "{}{}",
        fence.indent,
        fence.marker_char.to_string().repeat(fence.marker_len)
    )
}

fn close_fence_if_needed(text: &str, open_fence: Option<&OpenFence>) -> String {
    let Some(fence) = open_fence else {
        return text.to_string();
    };
    let close_line = close_fence_line(fence);
    if text.is_empty() {
        return close_line;
    }
    if !text.ends_with('\n') {
        return format!("{}\n{}", text, close_line);
    }
    format!("{}{}", text, close_line)
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Find the last whitespace char index in the window (separator stays with
/// the next segment).
fn find_whitespace_break(window: &[char]) -> Option<usize> {
    window.iter().rposition(|c| c.is_whitespace())
}

/// Find the last CJK punctuation break, returning the exclusive end so the
/// punctuation stays with the current segment. Never breaks at index 0.
fn find_cjk_punctuation_break(window: &[char]) -> Option<usize> {
    for end in (1..=window.len()).rev() {
        let ch = window[end - 1];
        if end - 1 > 0 && CJK_PUNCTUATION_BREAK_AFTER.contains(ch) {
            return Some(end);
        }
    }
    None
}

fn split_long_line(line: &str, max_chars: usize, preserve_whitespace: bool) -> Vec<String> {
    let limit = max_chars.max(1);
    if char_len(line) <= limit {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut remaining: Vec<char> = line.chars().collect();
    while remaining.len() > limit {
        if preserve_whitespace {
            let (head, tail) = remaining.split_at(limit);
            out.push(head.iter().collect::<String>());
            remaining = tail.to_vec();
            continue;
        }
        let window = &remaining[..limit];
        let break_idx = match find_whitespace_break(window) {
            Some(idx) if idx > 0 => idx,
            _ => match find_cjk_punctuation_break(window) {
                Some(idx) if idx > 0 => idx,
                _ => limit,
            },
        };
        let (head, tail) = remaining.split_at(break_idx);
        out.push(head.iter().collect::<String>());
        // Keep the separator with the next segment so words don't get glued.
        remaining = tail.to_vec();
    }
    if !remaining.is_empty() {
        out.push(remaining.iter().collect::<String>());
    }
    out
}

/// Chunk outbound Discord text by character count and soft line count while
/// keeping fenced code blocks balanced across chunks: each chunk that splits a
/// fence gets a synthetic closing fence, and the next chunk reopens the fence.
pub fn chunk_discord_text(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let max_lines = max_lines.max(1);
    if text.is_empty() {
        return Vec::new();
    }
    if char_len(text) <= max_chars && count_lines(text) <= max_lines {
        return vec![text.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_lines = 0usize;
    let mut open_fence: Option<OpenFence> = None;

    macro_rules! flush {
        () => {
            if !current.is_empty() {
                let payload = close_fence_if_needed(&current, open_fence.as_ref());
                if !payload.trim().is_empty() {
                    chunks.push(payload);
                }
                current = String::new();
                current_lines = 0;
                if let Some(fence) = open_fence.as_ref() {
                    current = fence.open_line.clone();
                    current_lines = 1;
                }
            }
        };
    }

    for original_line in text.split('\n') {
        let fence_info = parse_fence_line(original_line);
        let was_inside_fence = open_fence.is_some();
        let mut next_open_fence = open_fence.clone();
        if let Some(info) = fence_info {
            match open_fence.as_ref() {
                None => next_open_fence = Some(info),
                Some(open) => {
                    if open.marker_char == info.marker_char && info.marker_len >= open.marker_len {
                        next_open_fence = None;
                    }
                }
            }
        }

        // A flush can fire mid-line, before `open_fence` advances, so reserve
        // room for the still-open fence's closing line.
        let fence_to_reserve = next_open_fence.as_ref().or(open_fence.as_ref());
        let reserve_chars = fence_to_reserve
            .map(|fence| char_len(&close_fence_line(fence)) + 1)
            .unwrap_or(0);
        let reserve_lines = if fence_to_reserve.is_some() { 1 } else { 0 };
        let effective_max_chars = max_chars.saturating_sub(reserve_chars);
        let effective_max_lines = max_lines.saturating_sub(reserve_lines);
        let char_limit = if effective_max_chars > 0 {
            effective_max_chars
        } else {
            max_chars
        };
        let line_limit = if effective_max_lines > 0 {
            effective_max_lines
        } else {
            max_lines
        };
        let prefix_len = if !current.is_empty() {
            char_len(&current) + 1
        } else {
            0
        };
        let segment_limit = char_limit.saturating_sub(prefix_len).max(1);
        let segments = split_long_line(original_line, segment_limit, was_inside_fence);

        for (seg_index, segment) in segments.iter().enumerate() {
            let is_line_continuation = seg_index > 0;
            let delimiter = if is_line_continuation {
                ""
            } else if !current.is_empty() {
                "\n"
            } else {
                ""
            };
            let addition_len = char_len(delimiter) + char_len(segment);
            let next_len = char_len(&current) + addition_len;
            let next_lines = current_lines + if is_line_continuation { 0 } else { 1 };

            let would_exceed_chars = next_len > char_limit;
            let would_exceed_lines = next_lines > line_limit;

            if (would_exceed_chars || would_exceed_lines) && !current.is_empty() {
                flush!();
            }

            if !current.is_empty() {
                current.push_str(delimiter);
                current.push_str(segment);
                if !is_line_continuation {
                    current_lines += 1;
                }
            } else {
                current = segment.clone();
                current_lines = 1;
            }
        }

        open_fence = next_open_fence;
    }

    if !current.is_empty() {
        let payload = close_fence_if_needed(&current, open_fence.as_ref());
        if !payload.trim().is_empty() {
            chunks.push(payload);
        }
    }

    rebalance_reasoning_italics(text, chunks)
}

/// Chunk with the Discord defaults (2000 chars / 17 lines).
pub fn chunk_discord_text_default(text: &str) -> Vec<String> {
    chunk_discord_text(text, DISCORD_DEFAULT_MAX_CHARS, DISCORD_DEFAULT_MAX_LINES)
}

/// Keep italics intact for reasoning payloads wrapped once with `_…_`: close
/// italics at the end of each chunk and reopen at the start of the next.
fn rebalance_reasoning_italics(source: &str, chunks: Vec<String>) -> Vec<String> {
    if chunks.len() <= 1 {
        return chunks;
    }
    let opens_with_reasoning = {
        let starts = source.starts_with("Reasoning:") || source.starts_with("Thinking");
        let mut has_italic_open = false;
        if starts {
            if let Some(idx) = source.find('\n') {
                let after = source[idx..].trim_start_matches('\n');
                has_italic_open = after.starts_with('_');
            }
        }
        starts && has_italic_open && source.trim_end().ends_with('_')
    };
    if !opens_with_reasoning {
        return chunks;
    }
    let mut adjusted = chunks;
    let last = adjusted.len() - 1;
    for i in 0..adjusted.len() {
        if !adjusted[i].trim_end().ends_with('_') {
            adjusted[i].push('_');
        }
        if i == last {
            break;
        }
        let next = &adjusted[i + 1];
        let trimmed_start = next.trim_start();
        if !trimmed_start.starts_with('_') {
            let leading_len = next.len() - trimmed_start.len();
            let (leading, body) = next.split_at(leading_len);
            adjusted[i + 1] = format!("{}_{}", leading, body);
        }
    }
    adjusted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_single_chunk() {
        assert_eq!(chunk_discord_text_default("hello"), vec!["hello"]);
        assert!(chunk_discord_text_default("").is_empty());
    }

    #[test]
    fn splits_near_char_limit() {
        let text = "word ".repeat(600); // 3000 chars
        let chunks = chunk_discord_text(&text, 2000, 1000);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 2000);
        }
    }

    #[test]
    fn splits_on_soft_line_limit() {
        let text = (0..40).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let chunks = chunk_discord_text(&text, 2000, 17);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.split('\n').count() <= 17);
        }
    }

    #[test]
    fn keeps_code_fences_balanced_across_chunks() {
        let mut body = String::from("```rust\n");
        for i in 0..80 {
            body.push_str(&format!("let x{} = {};\n", i, i));
        }
        body.push_str("```");
        let chunks = chunk_discord_text(&body, 400, 100);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            // Every chunk must contain an even number of fence lines
            // (open + close) so Discord renders each chunk as valid code.
            let fence_lines = chunk
                .split('\n')
                .filter(|line| parse_fence_line(line).is_some())
                .count();
            assert_eq!(fence_lines % 2, 0, "unbalanced fences in chunk: {chunk}");
            assert!(chunk.chars().count() <= 400);
        }
        // Continuation chunks reopen the fence with the original info string.
        assert!(chunks[1].starts_with("```rust"));
    }

    #[test]
    fn fence_reservation_prevents_overflow() {
        let mut body = String::from("```\n");
        body.push_str(&"x".repeat(500));
        body.push('\n');
        body.push_str("```");
        let chunks = chunk_discord_text(&body, 120, 50);
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= 120,
                "chunk exceeded limit: {}",
                chunk.chars().count()
            );
        }
    }

    #[test]
    fn long_line_breaks_at_whitespace() {
        let line = format!("{} tail", "a".repeat(30));
        let segments = split_long_line(&line, 33, false);
        assert_eq!(segments[0], "a".repeat(30));
        assert_eq!(segments[1], " tail");
    }

    #[test]
    fn long_line_breaks_after_cjk_punctuation() {
        let line = format!("{}。{}", "あ".repeat(10), "い".repeat(10));
        let segments = split_long_line(&line, 12, false);
        assert_eq!(segments[0], format!("{}。", "あ".repeat(10)));
    }

    #[test]
    fn preserve_whitespace_inside_fences() {
        let line = format!("{}   {}", "a".repeat(5), "b".repeat(10));
        let segments = split_long_line(&line, 6, true);
        // Hard breaks at the limit — no whitespace-seeking inside fences.
        assert_eq!(segments[0].chars().count(), 6);
    }

    #[test]
    fn reasoning_italics_rebalanced() {
        let source = format!("Reasoning:\n_{}_", "thought ".repeat(400));
        let chunks = chunk_discord_text(&source, 500, 100);
        assert!(chunks.len() >= 2);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(chunk.trim_end().ends_with('_'), "chunk {} missing closing italic", i);
            if i > 0 {
                assert!(chunk.trim_start().starts_with('_'), "chunk {} missing reopening italic", i);
            }
        }
    }
}
