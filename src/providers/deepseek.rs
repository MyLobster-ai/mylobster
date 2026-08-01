//! DeepSeek provider helpers (v2026.5.x–6.x).
//!
//! * **DSML leak suppression** — DeepSeek V4 occasionally emits DSML tool
//!   markup (`<｜DSML｜tool_call>` …) as visible text instead of structured
//!   tool calls. The streaming filter removes DSML blocks while buffering
//!   split tag prefixes across chunks (port of upstream
//!   `createDeepSeekTextFilter`).
//! * **Thinking levels** — DeepSeek V4 supports `xhigh`/`max` reasoning
//!   levels on top of the base set.

const DSML_KINDS: &[&str] = &["tool_use_error", "tool_calls", "tool_call", "function_calls"];
const DSML_BARS: &[&str] = &["|", "｜"];

fn dsml_open_tokens() -> Vec<String> {
    DSML_BARS
        .iter()
        .flat_map(|bar| DSML_KINDS.iter().map(move |kind| format!("<{bar}DSML{bar}{kind}>")))
        .collect()
}

fn dsml_close_tokens() -> Vec<String> {
    DSML_BARS
        .iter()
        .flat_map(|bar| DSML_KINDS.iter().map(move |kind| format!("</{bar}DSML{bar}{kind}>")))
        .collect()
}

fn find_earliest_token(haystack: &str, tokens: &[String]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for token in tokens {
        if let Some(index) = haystack.find(token.as_str()) {
            if best.map(|(b, _)| index < b).unwrap_or(true) {
                best = Some((index, token.len()));
            }
        }
    }
    best
}

/// Longest possible prefix of any token found at the end of the buffer —
/// kept back so a tag split across streamed chunks is still recognized.
fn split_tag_holdback(buffer: &str, tokens: &[String]) -> usize {
    let max_len = tokens.iter().map(|t| t.len()).max().unwrap_or(0);
    let holdback_window = max_len.saturating_sub(1);
    for keep in (1..=holdback_window.min(buffer.len())).rev() {
        // Byte-index safety: only consider char boundaries.
        let start = buffer.len() - keep;
        if !buffer.is_char_boundary(start) {
            continue;
        }
        let suffix = &buffer[start..];
        if tokens.iter().any(|t| t.starts_with(suffix)) {
            return keep;
        }
    }
    0
}

/// Incremental DeepSeek DSML text filter: push streamed chunks, receive safe
/// visible text; DSML tool-markup blocks are removed, and split tag prefixes
/// are buffered across chunks. `flush` drops any unterminated DSML block.
#[derive(Debug)]
pub struct DeepSeekTextFilter {
    buffer: String,
    inside_dsml: bool,
    open_tokens: Vec<String>,
    close_tokens: Vec<String>,
}

impl Default for DeepSeekTextFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepSeekTextFilter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            inside_dsml: false,
            open_tokens: dsml_open_tokens(),
            close_tokens: dsml_close_tokens(),
        }
    }

    fn consume(&mut self, r#final: bool) -> Vec<String> {
        let mut output = Vec::new();
        loop {
            if self.buffer.is_empty() {
                break;
            }
            if self.inside_dsml {
                if let Some((index, len)) = find_earliest_token(&self.buffer, &self.close_tokens) {
                    self.buffer = self.buffer[index + len..].to_string();
                    self.inside_dsml = false;
                    continue;
                }
                // Keep a suffix that could still become a closing tag; on
                // final flush drop the unterminated block entirely.
                if r#final {
                    self.buffer.clear();
                    self.inside_dsml = false;
                } else {
                    let max_close = self.close_tokens.iter().map(|t| t.len()).max().unwrap_or(0);
                    let keep = (max_close.saturating_sub(1)).min(self.buffer.len());
                    let mut start = self.buffer.len() - keep;
                    while !self.buffer.is_char_boundary(start) {
                        start += 1;
                    }
                    self.buffer = self.buffer[start..].to_string();
                }
                return output;
            }

            if let Some((index, len)) = find_earliest_token(&self.buffer, &self.open_tokens) {
                let visible = self.buffer[..index].to_string();
                if !visible.is_empty() {
                    output.push(visible);
                }
                self.buffer = self.buffer[index + len..].to_string();
                self.inside_dsml = true;
                continue;
            }

            // No complete open tag: emit everything except a possible split
            // tag prefix at the end (unless flushing).
            let holdback = if r#final {
                0
            } else {
                split_tag_holdback(&self.buffer, &self.open_tokens)
            };
            let emit_len = self.buffer.len() - holdback;
            if emit_len > 0 {
                let visible = self.buffer[..emit_len].to_string();
                if !visible.is_empty() {
                    output.push(visible);
                }
                self.buffer = self.buffer[emit_len..].to_string();
            }
            break;
        }
        output
    }

    /// Push one streamed text chunk; returns safe visible text segments.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        self.consume(false)
    }

    /// Flush buffered text at stream end, dropping unterminated DSML blocks.
    pub fn flush(&mut self) -> Vec<String> {
        self.consume(true)
    }
}

/// Strip DSML blocks from a complete (non-streamed) text.
pub fn strip_dsml_markup(text: &str) -> String {
    let mut filter = DeepSeekTextFilter::new();
    let mut out: Vec<String> = filter.push(text);
    out.extend(filter.flush());
    out.join("")
}

/// Thinking levels supported by DeepSeek V4 (v2026.6.x: `xhigh` and `max`
/// join the base set).
pub const DEEPSEEK_V4_THINKING_LEVELS: &[&str] =
    &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(strip_dsml_markup("hello world"), "hello world");
    }

    #[test]
    fn strips_complete_dsml_block() {
        let input = "before <|DSML|tool_call>{\"tool\":\"x\"}</|DSML|tool_call> after";
        assert_eq!(strip_dsml_markup(input), "before  after");
    }

    #[test]
    fn strips_fullwidth_bar_variant() {
        let input = "a<｜DSML｜tool_calls>hidden</｜DSML｜tool_calls>b";
        assert_eq!(strip_dsml_markup(input), "ab");
    }

    #[test]
    fn drops_unterminated_block_on_flush() {
        let input = "visible <|DSML|function_calls>never closed";
        assert_eq!(strip_dsml_markup(input), "visible ");
    }

    #[test]
    fn buffers_split_open_tag_across_chunks() {
        let mut filter = DeepSeekTextFilter::new();
        let mut out = Vec::new();
        out.extend(filter.push("hello <|DSM"));
        out.extend(filter.push("L|tool_call>secret</|DSML|tool_call> world"));
        out.extend(filter.flush());
        assert_eq!(out.join(""), "hello  world");
    }

    #[test]
    fn buffers_split_close_tag_across_chunks() {
        let mut filter = DeepSeekTextFilter::new();
        let mut out = Vec::new();
        out.extend(filter.push("x<|DSML|tool_call>secret</|DSML|too"));
        out.extend(filter.push("l_call>y"));
        out.extend(filter.flush());
        assert_eq!(out.join(""), "xy");
    }

    #[test]
    fn angle_brackets_in_plain_text_survive() {
        let mut filter = DeepSeekTextFilter::new();
        let mut out = Vec::new();
        out.extend(filter.push("a < b and <tag> stays"));
        out.extend(filter.flush());
        assert_eq!(out.join(""), "a < b and <tag> stays");
    }

    #[test]
    fn multiple_blocks_stripped() {
        let input = "1<|DSML|tool_call>a</|DSML|tool_call>2<|DSML|tool_calls>b</|DSML|tool_calls>3";
        assert_eq!(strip_dsml_markup(input), "123");
    }

    #[test]
    fn v4_thinking_levels_include_xhigh_and_max() {
        assert!(DEEPSEEK_V4_THINKING_LEVELS.contains(&"xhigh"));
        assert!(DEEPSEEK_V4_THINKING_LEVELS.contains(&"max"));
    }
}
