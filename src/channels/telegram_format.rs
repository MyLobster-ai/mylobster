//! Telegram outbound formatting: markdown → Telegram-safe HTML conversion and
//! HTML-aware chunk splitting (port of OpenClaw `extensions/telegram/src/format.ts`,
//! `caption.ts`, and `rich-plain-fallback.ts` at v2026.7.1).
//!
//! Lengths are measured in UTF-16 code units to match both the upstream JS
//! implementation (`String.prototype.length`) and the Telegram Bot API limits.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

/// Telegram message text limit used for outbound chunking (upstream
/// `TELEGRAM_TEXT_CHUNK_LIMIT` in `outbound-adapter.ts`; safe margin under the
/// Bot API 4096 limit).
pub const TELEGRAM_TEXT_CHUNK_LIMIT: usize = 4000;

/// Telegram media caption limit (upstream `TELEGRAM_MAX_CAPTION_LENGTH`).
pub const TELEGRAM_MAX_CAPTION_LENGTH: usize = 1024;

// ============================================================================
// UTF-16 length helpers (JS `.length` parity)
// ============================================================================

/// Number of UTF-16 code units in `s` (JS `String.prototype.length` parity).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Byte index in `s` after consuming at most `max_units` UTF-16 code units,
/// never splitting a code point (upstream `clampToSurrogateBoundary` parity:
/// an astral char straddling the limit is dropped whole; if the very first
/// char exceeds the budget it is kept whole so chunking still advances).
fn byte_index_for_utf16_budget(s: &str, max_units: usize) -> usize {
    let mut units = 0usize;
    let mut byte_idx = 0usize;
    let mut first = true;
    for ch in s.chars() {
        let w = ch.len_utf16();
        if units + w > max_units {
            if first {
                // Keep the whole first char (upstream clamps forward past the pair).
                return ch.len_utf8();
            }
            return byte_idx;
        }
        units += w;
        byte_idx += ch.len_utf8();
        first = false;
    }
    byte_idx
}

// ============================================================================
// HTML escaping / plain-text fallback
// ============================================================================

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn escape_html_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

static TELEGRAM_HTML_TAG_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").unwrap());

/// Strips tags and unescapes entities so a failed HTML send can fall back to
/// plain text (upstream `telegramHtmlToPlainTextFallback`).
pub fn telegram_html_to_plain_text(html: &str) -> String {
    let stripped = TELEGRAM_HTML_TAG_PATTERN.replace_all(html, "");
    stripped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

// ============================================================================
// Markdown → Telegram HTML conversion
// ============================================================================
//
// The upstream converts through a full markdown IR (`markdownToIR` +
// `renderTelegramHtml`). This port covers the Telegram-supported entity set the
// upstream renderer emits: <b>, <i>, <s>, <code>, <pre><code class="language-x">,
// <a href>, <tg-spoiler>, <blockquote> — with heading style "none" (headings as
// bold lines) and linkified markdown links.

fn convert_inline_markdown(text: &str) -> String {
    // Protect inline code spans first, then apply remaining inline entities.
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let (before, after_tick) = rest.split_at(start);
        out.push_str(&convert_inline_no_code(before));
        let after = &after_tick[1..];
        if let Some(end) = after.find('`') {
            out.push_str("<code>");
            out.push_str(&escape_html(&after[..end]));
            out.push_str("</code>");
            rest = &after[end + 1..];
        } else {
            out.push_str(&escape_html("`"));
            rest = after;
        }
    }
    out.push_str(&convert_inline_no_code(rest));
    out
}

static LINK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").unwrap());
static BOLD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*([^*]+)\*\*").unwrap());
static ITALIC_STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
static ITALIC_UNDERSCORE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b_([^_\n]+)_\b").unwrap());
static STRIKE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~~([^~]+)~~").unwrap());
static SPOILER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\|\|([^|]+)\|\|").unwrap());

fn convert_inline_no_code(text: &str) -> String {
    // Extract links before escaping so URLs stay intact, using placeholders.
    let mut links: Vec<(String, String)> = Vec::new();
    let replaced = LINK_RE
        .replace_all(text, |caps: &regex::Captures| {
            let idx = links.len();
            links.push((caps[1].to_string(), caps[2].to_string()));
            format!("\u{0000}L{idx}\u{0000}")
        })
        .into_owned();

    let mut html = escape_html(&replaced);
    html = BOLD_RE.replace_all(&html, "<b>$1</b>").into_owned();
    html = STRIKE_RE.replace_all(&html, "<s>$1</s>").into_owned();
    html = SPOILER_RE
        .replace_all(&html, "<tg-spoiler>$1</tg-spoiler>")
        .into_owned();
    html = ITALIC_STAR_RE.replace_all(&html, "<i>$1</i>").into_owned();
    html = ITALIC_UNDERSCORE_RE
        .replace_all(&html, "<i>$1</i>")
        .into_owned();

    for (idx, (label, url)) in links.iter().enumerate() {
        let placeholder = format!("\u{0000}L{idx}\u{0000}");
        let anchor = format!(
            "<a href=\"{}\">{}</a>",
            escape_html_attr(url),
            escape_html(label)
        );
        html = html.replace(&placeholder, &anchor);
    }
    html
}

/// Blockquotes longer than this many lines render as Telegram expandable
/// blockquotes (`<blockquote expandable>`).
pub const TELEGRAM_EXPANDABLE_BLOCKQUOTE_MIN_LINES: usize = 4;

/// Whether `lines[index]` starts a markdown table (a `|`-delimited header row
/// followed by a `|---|` separator row).
fn is_table_start(lines: &[&str], index: usize) -> bool {
    let header = lines[index].trim();
    if !header.starts_with('|') || !header.contains('|') {
        return false;
    }
    let Some(next) = lines.get(index + 1) else {
        return false;
    };
    let sep = next.trim();
    sep.starts_with('|')
        && sep
            .trim_matches('|')
            .split('|')
            .all(|cell| {
                let cell = cell.trim();
                !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':' | ' '))
            })
}

/// Converts markdown to Telegram-safe HTML (Telegram-supported entity subset).
pub fn markdown_to_telegram_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    let mut blockquote_buf: Vec<String> = Vec::new();

    let flush_blockquote = |out: &mut String, buf: &mut Vec<String>| {
        if !buf.is_empty() {
            // Long quotes collapse behind Telegram's expandable control
            // (upstream expandable blockquote support, v2026.6.x).
            if buf.len() >= TELEGRAM_EXPANDABLE_BLOCKQUOTE_MIN_LINES {
                out.push_str("<blockquote expandable>");
            } else {
                out.push_str("<blockquote>");
            }
            out.push_str(&buf.join("\n"));
            out.push_str("</blockquote>\n");
            buf.clear();
        }
    };

    let lines: Vec<&str> = markdown.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        // Markdown tables normalize to monospace blocks BEFORE entity
        // escaping (Telegram HTML has no table support; upstream renders
        // tables as <pre><code> blocks).
        if !in_code_block && is_table_start(&lines, index) {
            flush_blockquote(&mut out, &mut blockquote_buf);
            let mut table_lines: Vec<&str> = Vec::new();
            while index < lines.len() && lines[index].trim().starts_with('|') {
                table_lines.push(lines[index].trim());
                index += 1;
            }
            out.push_str(&format!(
                "<pre><code>{}</code></pre>\n",
                escape_html(&table_lines.join("\n"))
            ));
            continue;
        }
        index += 1;
        if trimmed.starts_with("```") {
            if in_code_block {
                // Close code block.
                if code_buf.ends_with('\n') {
                    code_buf.pop();
                }
                match &code_lang {
                    Some(lang) if !lang.is_empty() => {
                        out.push_str(&format!(
                            "<pre><code class=\"language-{}\">{}</code></pre>\n",
                            escape_html_attr(lang),
                            escape_html(&code_buf)
                        ));
                    }
                    _ => {
                        out.push_str(&format!(
                            "<pre><code>{}</code></pre>\n",
                            escape_html(&code_buf)
                        ));
                    }
                }
                code_buf.clear();
                code_lang = None;
                in_code_block = false;
            } else {
                flush_blockquote(&mut out, &mut blockquote_buf);
                in_code_block = true;
                let lang = trimmed.trim_start_matches('`').trim();
                code_lang = if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                };
            }
            continue;
        }
        if in_code_block {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }
        if let Some(quoted) = trimmed.strip_prefix("> ").or_else(|| {
            if trimmed == ">" {
                Some("")
            } else {
                None
            }
        }) {
            blockquote_buf.push(convert_inline_markdown(quoted));
            continue;
        }
        flush_blockquote(&mut out, &mut blockquote_buf);
        // Headings render as bold lines (upstream headingStyle: "none").
        if let Some(rest) = trimmed
            .strip_prefix("###### ")
            .or_else(|| trimmed.strip_prefix("##### "))
            .or_else(|| trimmed.strip_prefix("#### "))
            .or_else(|| trimmed.strip_prefix("### "))
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            out.push_str(&format!("<b>{}</b>\n", convert_inline_markdown(rest)));
            continue;
        }
        out.push_str(&convert_inline_markdown(line));
        out.push('\n');
    }
    if in_code_block {
        // Unterminated fence — flush what we have.
        if code_buf.ends_with('\n') {
            code_buf.pop();
        }
        out.push_str(&format!(
            "<pre><code>{}</code></pre>\n",
            escape_html(&code_buf)
        ));
    }
    flush_blockquote(&mut out, &mut blockquote_buf);
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ============================================================================
// HTML-aware chunk splitting (faithful port of `splitTelegramHtmlChunks`)
// ============================================================================

static HTML_TAG_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(</?)([a-zA-Z][a-zA-Z0-9-]*)\b[^>]*?>").unwrap());

static TELEGRAM_SELF_CLOSING_HTML_TAGS: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["br", "hr", "img", "input", "tg-map"].into_iter().collect());

static TELEGRAM_RICH_BLOCK_HTML_TAGS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "aside",
        "audio",
        "blockquote",
        "details",
        "figure",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "img",
        "li",
        "ol",
        "p",
        "pre",
        "table",
        "tg-collage",
        "tg-map",
        "tg-math-block",
        "tg-slideshow",
        "tr",
        "ul",
        "video",
    ]
    .into_iter()
    .collect()
});

static TELEGRAM_RICH_MEDIA_HTML_TAGS: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["audio", "img", "video"].into_iter().collect());

static ANCHOR_NAME_ATTR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)\sname="[^"]+""#).unwrap());

#[derive(Debug, Clone)]
struct TelegramHtmlTag {
    name: String,
    open_tag: String,
    close_tag: String,
    rich_block: bool,
    rich_media: bool,
}

fn is_telegram_rich_block_html_tag(raw_tag: &str, tag_name: &str) -> bool {
    TELEGRAM_RICH_BLOCK_HTML_TAGS.contains(tag_name)
        || (tag_name == "a" && ANCHOR_NAME_ATTR_RE.is_match(raw_tag))
}

fn build_open_prefix(tags: &[TelegramHtmlTag]) -> String {
    tags.iter().map(|t| t.open_tag.as_str()).collect()
}

fn build_close_suffix(tags: &[TelegramHtmlTag]) -> String {
    tags.iter().rev().map(|t| t.close_tag.as_str()).collect()
}

fn close_suffix_len16(tags: &[TelegramHtmlTag]) -> usize {
    tags.iter().map(|t| utf16_len(&t.close_tag)).sum()
}

/// Finds the end byte index (position of `;`) of an HTML entity starting at
/// byte `start` (which must be `&`), or `None` (upstream
/// `findTelegramHtmlEntityEnd`).
fn find_html_entity_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'&') {
        return None;
    }
    let mut i = start + 1;
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'#' {
        i += 1;
        if i >= bytes.len() {
            return None;
        }
        let is_hex = bytes[i] == b'x' || bytes[i] == b'X';
        if is_hex {
            i += 1;
            let hex_start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i == hex_start {
                return None;
            }
        } else {
            let digit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == digit_start {
                return None;
            }
        }
    } else {
        let name_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i == name_start {
            return None;
        }
    }
    if bytes.get(i) == Some(&b';') {
        Some(i)
    } else {
        None
    }
}

/// Byte index at which to split `text` so at most `max_units` UTF-16 units are
/// taken, without splitting an HTML entity or a code point (upstream
/// `findTelegramHtmlSafeSplitIndex`).
fn find_html_safe_split_index(text: &str, max_units: usize) -> usize {
    if utf16_len(text) <= max_units {
        return text.len();
    }
    let normalized_max = max_units.max(1);
    let limit_byte = byte_index_for_utf16_budget(text, normalized_max);
    // Entity safety: if the last `&` before the split has no matching `;`
    // before the split but forms a valid entity crossing it, back up to the `&`.
    let head = &text[..limit_byte];
    let last_amp = head.rfind('&');
    let Some(amp_idx) = last_amp else {
        return limit_byte;
    };
    if let Some(semi_idx) = head.rfind(';') {
        if amp_idx < semi_idx {
            return limit_byte;
        }
    }
    match find_html_entity_end(text, amp_idx) {
        Some(entity_end) if entity_end >= limit_byte => amp_idx,
        _ => limit_byte,
    }
}

fn pop_html_tag(tags: &mut Vec<TelegramHtmlTag>, name: &str) {
    if let Some(pos) = tags.iter().rposition(|t| t.name == name) {
        tags.remove(pos);
    }
}

/// Error raised when the tag overhead alone exceeds the chunk limit.
#[derive(Debug, thiserror::Error)]
pub enum TelegramChunkError {
    #[error("Telegram HTML chunk limit exceeded by tag overhead (limit={0})")]
    TagOverhead(usize),
    #[error("Telegram HTML chunk limit exceeded by leading entity (limit={0})")]
    LeadingEntity(usize),
}

/// Splits Telegram HTML into chunks of at most `limit` UTF-16 units, closing
/// open tags at chunk boundaries and reopening them in the next chunk.
/// Faithful port of upstream `splitTelegramHtmlChunks` (v2026.7.1).
pub fn split_telegram_html_chunks(
    html: &str,
    limit: usize,
) -> Result<Vec<String>, TelegramChunkError> {
    split_telegram_html_chunks_with_limits(html, limit, None, None)
}

pub fn split_telegram_html_chunks_with_limits(
    html: &str,
    limit: usize,
    block_limit: Option<usize>,
    media_limit: Option<usize>,
) -> Result<Vec<String>, TelegramChunkError> {
    if html.is_empty() {
        return Ok(Vec::new());
    }
    let normalized_limit = limit.max(1);
    let block_limit = block_limit.map(|v| v.max(1));
    let media_limit = media_limit.map(|v| v.max(1));
    if utf16_len(html) <= normalized_limit && block_limit.is_none() && media_limit.is_none() {
        return Ok(vec![html.to_string()]);
    }

    struct ChunkState {
        chunks: Vec<String>,
        open_tags: Vec<TelegramHtmlTag>,
        current: String,
        current_len16: usize,
        current_block_count: usize,
        current_media_count: usize,
        chunk_has_payload: bool,
    }

    impl ChunkState {
        fn reset_current(&mut self) {
            self.current = build_open_prefix(&self.open_tags);
            self.current_len16 = utf16_len(&self.current);
            self.current_block_count = self.open_tags.iter().filter(|t| t.rich_block).count();
            self.current_media_count = self.open_tags.iter().filter(|t| t.rich_media).count();
            self.chunk_has_payload = false;
        }

        fn flush_current(&mut self) {
            if !self.chunk_has_payload {
                return;
            }
            let mut chunk = std::mem::take(&mut self.current);
            chunk.push_str(&build_close_suffix(&self.open_tags));
            self.chunks.push(chunk);
            self.reset_current();
        }

        fn append_text(
            &mut self,
            segment: &str,
            normalized_limit: usize,
        ) -> Result<(), TelegramChunkError> {
            let mut remaining = segment;
            while !remaining.is_empty() {
                let overhead = self.current_len16 + close_suffix_len16(&self.open_tags);
                let available = normalized_limit.saturating_sub(overhead);
                if available == 0 {
                    if !self.chunk_has_payload {
                        return Err(TelegramChunkError::TagOverhead(normalized_limit));
                    }
                    self.flush_current();
                    continue;
                }
                let remaining_len16 = utf16_len(remaining);
                if remaining_len16 <= available {
                    self.current.push_str(remaining);
                    self.current_len16 += remaining_len16;
                    self.chunk_has_payload = true;
                    break;
                }
                let split_at = find_html_safe_split_index(remaining, available);
                if split_at == 0 {
                    if !self.chunk_has_payload {
                        return Err(TelegramChunkError::LeadingEntity(normalized_limit));
                    }
                    self.flush_current();
                    continue;
                }
                let (head, tail) = remaining.split_at(split_at);
                self.current.push_str(head);
                self.current_len16 += utf16_len(head);
                self.chunk_has_payload = true;
                remaining = tail;
                self.flush_current();
            }
            Ok(())
        }
    }

    let mut state = ChunkState {
        chunks: Vec::new(),
        open_tags: Vec::new(),
        current: String::new(),
        current_len16: 0,
        current_block_count: 0,
        current_media_count: 0,
        chunk_has_payload: false,
    };
    state.reset_current();

    let mut last_index = 0usize;
    for caps in HTML_TAG_PATTERN.captures_iter(html) {
        let m = caps.get(0).unwrap();
        let tag_start = m.start();
        let tag_end = m.end();
        state.append_text(&html[last_index..tag_start], normalized_limit)?;

        let raw_tag = m.as_str();
        let is_closing = &caps[1] == "</";
        let tag_name = caps[2].to_lowercase();
        let is_self_closing = !is_closing
            && (TELEGRAM_SELF_CLOSING_HTML_TAGS.contains(tag_name.as_str())
                || raw_tag.trim_end().ends_with("/>"));
        let is_rich_block = !is_closing && is_telegram_rich_block_html_tag(raw_tag, &tag_name);
        let is_rich_media = !is_closing
            && (tag_name == "figure"
                || (TELEGRAM_RICH_MEDIA_HTML_TAGS.contains(tag_name.as_str())
                    && !state.open_tags.iter().any(|t| t.name == "figure")));

        if !is_closing {
            let next_close_len = if is_self_closing {
                0
            } else {
                utf16_len(&format!("</{tag_name}>"))
            };
            let over_block = block_limit
                .map(|bl| is_rich_block && state.current_block_count >= bl)
                .unwrap_or(false);
            let over_media = media_limit
                .map(|ml| is_rich_media && state.current_media_count >= ml)
                .unwrap_or(false);
            let over_length = state.current_len16
                + utf16_len(raw_tag)
                + close_suffix_len16(&state.open_tags)
                + next_close_len
                > normalized_limit;
            if state.chunk_has_payload && (over_block || over_media || over_length) {
                state.flush_current();
            }
        }

        state.current.push_str(raw_tag);
        state.current_len16 += utf16_len(raw_tag);
        if is_self_closing {
            state.chunk_has_payload = true;
        }
        if is_rich_block {
            state.current_block_count += 1;
        }
        if is_rich_media {
            state.current_media_count += 1;
        }
        if is_closing {
            pop_html_tag(&mut state.open_tags, &tag_name);
        } else if !is_self_closing {
            state.open_tags.push(TelegramHtmlTag {
                close_tag: format!("</{tag_name}>"),
                open_tag: raw_tag.to_string(),
                name: tag_name,
                rich_block: is_rich_block,
                rich_media: is_rich_media,
            });
        }
        last_index = tag_end;
    }

    state.append_text(&html[last_index..], normalized_limit)?;
    state.flush_current();
    if state.chunks.is_empty() {
        Ok(vec![html.to_string()])
    } else {
        Ok(state.chunks)
    }
}

/// Converts markdown to Telegram HTML chunks within `limit` UTF-16 units
/// (upstream `markdownToTelegramHtmlChunks`).
pub fn markdown_to_telegram_html_chunks(
    markdown: &str,
    limit: usize,
) -> Result<Vec<String>, TelegramChunkError> {
    split_telegram_html_chunks(&markdown_to_telegram_html(markdown), limit)
}

// ============================================================================
// Plain-text chunking (port of `rich-plain-fallback.ts`)
// ============================================================================

/// Fixed-size plain-text chunking at code-point boundaries (upstream
/// `splitTelegramPlainTextChunks`).
pub fn split_plain_text_chunks(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized_limit = limit.max(1);
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = byte_index_for_utf16_budget(rest, normalized_limit);
        let (head, tail) = rest.split_at(end.max(1).min(rest.len()));
        if head.is_empty() {
            break;
        }
        chunks.push(head.to_string());
        rest = tail;
    }
    chunks
}

/// Distributes plain text over exactly `chunk_count` chunks when the HTML plan
/// produced more chunks than the fixed split (upstream
/// `splitTelegramPlainTextFallback`).
pub fn split_plain_text_fallback(text: &str, chunk_count: usize, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized_limit = limit.max(1);
    let fixed = split_plain_text_chunks(text, normalized_limit);
    if chunk_count <= 1 || fixed.len() >= chunk_count {
        return fixed;
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    let total_units = utf16_len(text);
    let mut consumed_units = 0usize;
    for index in 0..chunk_count {
        let remaining_units = total_units - consumed_units;
        let remaining_chunks = chunk_count - index;
        let next_len = if remaining_chunks == 1 {
            remaining_units
        } else {
            normalized_limit.min(remaining_units.div_ceil(remaining_chunks))
        };
        let end = byte_index_for_utf16_budget(rest, next_len.max(1));
        let (head, tail) = rest.split_at(end.min(rest.len()));
        consumed_units += utf16_len(head);
        chunks.push(head.to_string());
        rest = tail;
    }
    chunks
}

// ============================================================================
// Chunked text plan (port of send.ts `buildChunkedTextPlan`)
// ============================================================================

/// One outbound text chunk: HTML body with a plain-text fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramTextChunk {
    pub plain_text: String,
    pub html_text: Option<String>,
}

/// Builds the HTML+plain chunk plan for a markdown message (upstream
/// `buildChunkedTextPlan`): HTML chunks with paired plain fallbacks; falls back
/// to plain-only chunks when HTML planning fails or plain needs more chunks.
pub fn build_chunked_text_plan(raw_text: &str, limit: usize) -> Vec<TelegramTextChunk> {
    let html_text = markdown_to_telegram_html(raw_text);
    let fallback_text = raw_text;
    let html_chunks = match split_telegram_html_chunks(&html_text, limit) {
        Ok(chunks) => chunks,
        Err(err) => {
            tracing::debug!(
                "telegram send failed HTML chunk planning, retrying as plain text: {err}"
            );
            return split_plain_text_chunks(fallback_text, limit)
                .into_iter()
                .map(|plain_text| TelegramTextChunk {
                    plain_text,
                    html_text: None,
                })
                .collect();
        }
    };
    let fixed_plain_chunks = split_plain_text_chunks(fallback_text, limit);
    if fixed_plain_chunks.len() > html_chunks.len() {
        tracing::debug!(
            "telegram send plain-text fallback needs more chunks than HTML; sending plain text"
        );
        return fixed_plain_chunks
            .into_iter()
            .map(|plain_text| TelegramTextChunk {
                plain_text,
                html_text: None,
            })
            .collect();
    }
    let plain_chunks = split_plain_text_fallback(fallback_text, html_chunks.len(), limit);
    html_chunks
        .into_iter()
        .enumerate()
        .map(|(index, html)| TelegramTextChunk {
            plain_text: plain_chunks.get(index).cloned().unwrap_or_else(|| html.clone()),
            html_text: Some(html),
        })
        .collect()
}

// ============================================================================
// Rich message rendering (port of rich-message.ts constants + chunk planning)
// ============================================================================

/// Rich-message per-chunk text limit (upstream `TELEGRAM_RICH_TEXT_LIMIT`).
pub const TELEGRAM_RICH_TEXT_LIMIT: usize = 32_768;
/// Rich blocks per chunk (upstream `TELEGRAM_RICH_BLOCK_LIMIT`).
pub const TELEGRAM_RICH_BLOCK_LIMIT: usize = 500;
/// Rich media elements per chunk (upstream `TELEGRAM_RICH_MEDIA_LIMIT`).
pub const TELEGRAM_RICH_MEDIA_LIMIT: usize = 50;

/// Renders markdown into rich HTML chunks: up to 32 768 UTF-16 units per
/// chunk with the upstream block (500) and media (50) caps, tags reopened
/// across boundaries. Used when `channels.telegram.richMessages` is true;
/// callers fall back to the plain chunk plan on errors (rich→plain fallback).
pub fn render_rich_message_chunks(
    markdown: &str,
    text_limit: usize,
) -> Result<Vec<String>, TelegramChunkError> {
    let limit = text_limit.clamp(1, TELEGRAM_RICH_TEXT_LIMIT);
    split_telegram_html_chunks_with_limits(
        &markdown_to_telegram_html(markdown),
        limit,
        Some(TELEGRAM_RICH_BLOCK_LIMIT),
        Some(TELEGRAM_RICH_MEDIA_LIMIT),
    )
}

// ============================================================================
// Sticker paths (outbound sticker references)
// ============================================================================

/// Parses an outbound sticker reference (`sticker:<file_id>`). Messages of
/// this shape deliver as native stickers instead of text, preserving the
/// upstream sticker media path.
pub fn parse_sticker_reference(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("sticker:")?;
    let file_id = rest.trim();
    (!file_id.is_empty()
        && file_id.len() >= 10
        && file_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some(file_id)
}

// ============================================================================
// Caption splitting for media follow-ups (port of caption.ts)
// ============================================================================

/// Result of splitting message text against the Telegram caption limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TelegramCaptionSplit {
    /// Caption to attach to the media message (≤ 1024 UTF-16 units).
    pub caption: Option<String>,
    /// Text too long for a caption — send as a separate follow-up message.
    pub follow_up_text: Option<String>,
}

/// Splits text into media caption vs. follow-up message (upstream
/// `splitTelegramCaption`).
pub fn split_telegram_caption(text: Option<&str>) -> TelegramCaptionSplit {
    let trimmed = text.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return TelegramCaptionSplit::default();
    }
    if utf16_len(trimmed) > TELEGRAM_MAX_CAPTION_LENGTH {
        return TelegramCaptionSplit {
            caption: None,
            follow_up_text: Some(trimmed.to_string()),
        };
    }
    TelegramCaptionSplit {
        caption: Some(trimmed.to_string()),
        follow_up_text: None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_len_counts_astral_chars_as_two() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len("a😀b"), 4);
    }

    #[test]
    fn markdown_bold_italic_code() {
        let html = markdown_to_telegram_html("**bold** *it* `co<de>`");
        assert_eq!(html, "<b>bold</b> <i>it</i> <code>co&lt;de&gt;</code>");
    }

    #[test]
    fn markdown_link_and_spoiler() {
        let html = markdown_to_telegram_html("[link](https://x.test/a?b=1&c=2) ||sec||");
        assert!(html.contains("<a href=\"https://x.test/a?b=1&amp;c=2\">link</a>"));
        assert!(html.contains("<tg-spoiler>sec</tg-spoiler>"));
    }

    #[test]
    fn markdown_fenced_code_block_with_language() {
        let html = markdown_to_telegram_html("```rust\nfn main() { println!(\"<hi>\"); }\n```");
        assert_eq!(
            html,
            "<pre><code class=\"language-rust\">fn main() { println!(\"&lt;hi&gt;\"); }</code></pre>"
        );
    }

    #[test]
    fn markdown_heading_and_blockquote() {
        let html = markdown_to_telegram_html("# Title\n> quoted\n> more\nplain");
        assert!(html.starts_with("<b>Title</b>\n"));
        assert!(html.contains("<blockquote>quoted\nmore</blockquote>"));
        assert!(html.ends_with("plain"));
    }

    #[test]
    fn short_html_is_single_chunk() {
        let chunks = split_telegram_html_chunks("<b>hi</b>", 4000).unwrap();
        assert_eq!(chunks, vec!["<b>hi</b>".to_string()]);
    }

    #[test]
    fn splits_plain_text_at_limit() {
        let text = "a".repeat(25);
        let chunks = split_telegram_html_chunks(&text, 10).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 10);
        assert_eq!(chunks[1].len(), 10);
        assert_eq!(chunks[2].len(), 5);
    }

    #[test]
    fn reopens_bold_tag_across_chunks() {
        // "<b>" + 20 chars + "</b>" with limit 15: tag overhead 7 per chunk.
        let html = format!("<b>{}</b>", "x".repeat(20));
        let chunks = split_telegram_html_chunks(&html, 15).unwrap();
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.starts_with("<b>"), "chunk should reopen tag: {chunk}");
            assert!(chunk.ends_with("</b>"), "chunk should close tag: {chunk}");
            assert!(utf16_len(chunk) <= 15, "chunk over limit: {chunk}");
        }
        let payload: String = chunks
            .iter()
            .map(|c| c.trim_start_matches("<b>").trim_end_matches("</b>"))
            .collect();
        assert_eq!(payload, "x".repeat(20));
    }

    #[test]
    fn long_code_block_reopens_pre_code_tags() {
        let body = "line of code\n".repeat(40); // 520 chars
        let html = format!("<pre><code class=\"language-rust\">{body}</code></pre>");
        let chunks = split_telegram_html_chunks(&html, 200).unwrap();
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(utf16_len(chunk) <= 200);
            assert!(chunk.starts_with("<pre><code"), "chunk: {chunk}");
            assert!(chunk.ends_with("</code></pre>"), "chunk: {chunk}");
        }
        // Reopened prefix preserves the language attribute.
        assert!(chunks[1].starts_with("<pre><code class=\"language-rust\">"));
    }

    #[test]
    fn entity_never_split_across_boundary() {
        // Position the entity so a naive split would cut through "&amp;".
        let html = format!("{}&amp;tail", "a".repeat(8));
        // limit 10: naive split at index 10 would cut inside "&amp;".
        let chunks = split_telegram_html_chunks(&html, 10).unwrap();
        for chunk in &chunks {
            let amp_positions: Vec<_> = chunk.match_indices('&').collect();
            for (pos, _) in amp_positions {
                assert!(
                    chunk[pos..].starts_with("&amp;"),
                    "split entity in chunk: {chunk}"
                );
            }
        }
        let joined: String = chunks.join("");
        assert_eq!(joined, html);
    }

    #[test]
    fn numeric_and_hex_entities_kept_whole() {
        let html = format!("{}&#128512;x", "b".repeat(6));
        let chunks = split_telegram_html_chunks(&html, 9).unwrap();
        assert_eq!(chunks, vec!["bbbbbb", "&#128512;", "x"]);
    }

    #[test]
    fn leading_entity_larger_than_budget_errors() {
        // Upstream throws so callers fall back to plain-text chunking.
        let html = format!("{}&#128512;x", "b".repeat(6));
        let err = split_telegram_html_chunks(&html, 8).unwrap_err();
        assert!(matches!(err, TelegramChunkError::LeadingEntity(8)));
        // build_chunked_text_plan recovers via the plain-text path.
        let plan = build_chunked_text_plan("&#128512;", 4);
        assert!(!plan.is_empty());
    }

    #[test]
    fn emoji_not_split_at_boundary() {
        // 3 chars then emoji (2 UTF-16 units): limit 4 would land mid-pair.
        let html = "abc😀def";
        let chunks = split_telegram_html_chunks(html, 4).unwrap();
        // Emoji must appear whole in exactly one chunk.
        let with_emoji: Vec<_> = chunks.iter().filter(|c| c.contains('😀')).collect();
        assert_eq!(with_emoji.len(), 1);
        assert_eq!(chunks.join(""), html);
        for chunk in &chunks {
            assert!(utf16_len(chunk) <= 4);
        }
    }

    #[test]
    fn tag_overhead_exceeding_limit_errors() {
        let html = format!("<blockquote>{}</blockquote>", "y".repeat(50));
        let err = split_telegram_html_chunks(&html, 20).unwrap_err();
        assert!(matches!(err, TelegramChunkError::TagOverhead(20)));
    }

    #[test]
    fn nested_tags_reopened_in_order() {
        let html = format!("<b><i>{}</i></b>", "z".repeat(30));
        let chunks = split_telegram_html_chunks(&html, 20).unwrap();
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.starts_with("<b><i>"));
            assert!(chunk.ends_with("</i></b>"));
        }
    }

    #[test]
    fn self_closing_br_counts_as_payload() {
        let chunks = split_telegram_html_chunks("a<br/>b", 4000).unwrap();
        assert_eq!(chunks, vec!["a<br/>b".to_string()]);
    }

    #[test]
    fn plain_chunks_fixed_split() {
        let chunks = split_plain_text_chunks(&"a".repeat(10), 4);
        assert_eq!(chunks, vec!["aaaa", "aaaa", "aa"]);
    }

    #[test]
    fn plain_chunks_never_split_emoji() {
        let text = "ab😀cd";
        let chunks = split_plain_text_chunks(text, 3);
        assert_eq!(chunks.join(""), text);
        for chunk in &chunks {
            assert!(!chunk.contains('\u{FFFD}'));
        }
    }

    #[test]
    fn plain_fallback_distributes_evenly() {
        let text = "abcdefgh";
        let chunks = split_plain_text_fallback(text, 3, 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.join(""), text);
        // ceil(8/3)=3, then ceil(5/2)=3, then 2
        assert_eq!(chunks, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn chunked_text_plan_pairs_html_and_plain() {
        let plan = build_chunked_text_plan("**bold** text", 4000);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].html_text.as_deref(), Some("<b>bold</b> text"));
        assert_eq!(plan[0].plain_text, "**bold** text");
    }

    #[test]
    fn chunked_text_plan_multi_chunk_long_message() {
        let long = "word ".repeat(2000); // 10_000 chars
        let plan = build_chunked_text_plan(&long, TELEGRAM_TEXT_CHUNK_LIMIT);
        assert!(plan.len() >= 3);
        for chunk in &plan {
            if let Some(html) = &chunk.html_text {
                assert!(utf16_len(html) <= TELEGRAM_TEXT_CHUNK_LIMIT);
            }
            assert!(utf16_len(&chunk.plain_text) <= TELEGRAM_TEXT_CHUNK_LIMIT);
        }
    }

    #[test]
    fn caption_split_short_text_is_caption() {
        let split = split_telegram_caption(Some("  hello  "));
        assert_eq!(split.caption.as_deref(), Some("hello"));
        assert!(split.follow_up_text.is_none());
    }

    #[test]
    fn caption_split_long_text_becomes_follow_up() {
        let long = "x".repeat(TELEGRAM_MAX_CAPTION_LENGTH + 1);
        let split = split_telegram_caption(Some(&long));
        assert!(split.caption.is_none());
        assert_eq!(split.follow_up_text.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn caption_split_empty_is_none() {
        assert_eq!(split_telegram_caption(None), TelegramCaptionSplit::default());
        assert_eq!(
            split_telegram_caption(Some("   ")),
            TelegramCaptionSplit::default()
        );
    }

    #[test]
    fn markdown_table_normalized_to_pre_block() {
        let md = "before\n| a | b |\n|---|---|\n| 1 | 2 |\nafter";
        let html = markdown_to_telegram_html(md);
        assert!(html.contains("<pre><code>| a | b |\n|---|---|\n| 1 | 2 |</code></pre>"));
        assert!(html.starts_with("before\n"));
        assert!(html.ends_with("after"));
    }

    #[test]
    fn pipe_lines_without_separator_are_not_tables() {
        let html = markdown_to_telegram_html("| not a table |\nplain");
        assert!(!html.contains("<pre>"));
    }

    #[test]
    fn short_blockquote_stays_plain() {
        let html = markdown_to_telegram_html("> one\n> two");
        assert!(html.contains("<blockquote>one\ntwo</blockquote>"));
        assert!(!html.contains("expandable"));
    }

    #[test]
    fn long_blockquote_becomes_expandable() {
        let md = "> l1\n> l2\n> l3\n> l4\n> l5";
        let html = markdown_to_telegram_html(md);
        assert!(html.contains("<blockquote expandable>l1\nl2\nl3\nl4\nl5</blockquote>"));
    }

    #[test]
    fn expandable_blockquote_reopened_across_chunks() {
        // The chunker must preserve the `expandable` attribute when reopening.
        let body = "line one two three\n".repeat(12);
        let html = format!("<blockquote expandable>{}</blockquote>", body.trim_end());
        let chunks = split_telegram_html_chunks(&html, 120).unwrap();
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.starts_with("<blockquote expandable>"));
            assert!(chunk.ends_with("</blockquote>"));
        }
    }

    #[test]
    fn rich_chunks_respect_block_limit() {
        // 600 paragraphs of "<p>"-free markdown → block limit is enforced via
        // blockquote blocks; use many blockquotes to exercise the cap.
        let md = (0..40)
            .map(|i| format!("> quote {i}\n\ntext {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = render_rich_message_chunks(&md, TELEGRAM_RICH_TEXT_LIMIT).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(utf16_len(chunk) <= TELEGRAM_RICH_TEXT_LIMIT);
        }
        // Round-trip: joined content preserves all quotes.
        let joined = chunks.join("");
        assert!(joined.contains("quote 0"));
        assert!(joined.contains("quote 39"));
    }

    #[test]
    fn rich_chunks_allow_longer_than_plain_limit() {
        let long = "word ".repeat(1_500); // 7 500 chars > 4 000 plain limit
        let chunks = render_rich_message_chunks(&long, TELEGRAM_RICH_TEXT_LIMIT).unwrap();
        assert_eq!(chunks.len(), 1, "rich chunk carries up to 32k units");
    }

    #[test]
    fn sticker_reference_parsing() {
        assert_eq!(
            parse_sticker_reference("sticker:CAACAgIAAxkBAAI"),
            Some("CAACAgIAAxkBAAI")
        );
        assert_eq!(parse_sticker_reference("  sticker: CAACAgIAAxkBAAI "), Some("CAACAgIAAxkBAAI"));
        assert_eq!(parse_sticker_reference("sticker:"), None);
        assert_eq!(parse_sticker_reference("sticker:short"), None);
        assert_eq!(parse_sticker_reference("sticker:has spaces in it"), None);
        assert_eq!(parse_sticker_reference("plain text"), None);
    }

    #[test]
    fn html_to_plain_text_strips_tags_and_unescapes() {
        assert_eq!(
            telegram_html_to_plain_text("<b>a &amp; b</b> &lt;tag&gt;"),
            "a & b <tag>"
        );
    }
}
