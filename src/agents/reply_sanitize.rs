//! Shared user-facing reply sanitization (OpenClaw v2026.5.2 / v2026.7.1).
//!
//! Some models leak tool-call scaffolding into their visible text output:
//! - Legacy bracket protocol: `[TOOL_CALL]…[/TOOL_CALL]` / `[TOOL_RESULT]…[/TOOL_RESULT]`
//!   (seen from heartbeat runs replaying old transcripts).
//! - MiniMax scaffolding: `<mm:tool_call>…</mm:tool_call>`, `<mm:reasoning>…</mm:reasoning>`
//!   and the `<minimax:tool_call>` variant.
//! - Generic XML tool-call scaffolding: `<tool_call>…</tool_call>`,
//!   `<function_call>…</function_call>`, `<function_calls>…</function_calls>`,
//!   `<invoke …>…</invoke>`, `<function_response>…</function_response>`.
//! - DSML leakage from DeepSeek-family models: `<dsml:…>…</dsml:…>`.
//! - `<final>…</final>` sentinels (wrapper removed, inner text preserved).
//! - Runtime-context sentinels (internal runtime-event markers must never be
//!   shown to users).
//!
//! All user-facing reply paths (chat finals, heartbeat deliveries, channel
//! outbound) should run through [`sanitize_user_facing_reply`] before text
//! reaches a user.

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};

/// Internal marker submitted as the user-visible turn text for pre-compaction
/// memory-flush turns (see `agents::compaction`). Never user-visible.
pub const RUNTIME_EVENT_SENTINEL_PREFIX: &str = "[[runtime-event]]";

fn block_re(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
        .expect("static sanitize regex must compile")
}

/// `[TOOL_CALL]…[/TOOL_CALL]` (and TOOL_RESULT) blocks, closed form.
/// (The `regex` crate has no backreferences, so each kind is spelled out.)
static LEGACY_BRACKET_BLOCK: Lazy<Regex> = Lazy::new(|| {
    block_re(r"\[TOOL_CALL\].*?\[/TOOL_CALL\]|\[TOOL_RESULT\].*?\[/TOOL_RESULT\]")
});

/// Unclosed trailing legacy block — strip from the opener to end of text.
///
/// Runs only after the closed-pair pass, so any surviving opener is by
/// definition unterminated and everything after it is scaffolding. The tail is
/// matched with `.*` (not `[^\[]*`) so a *mismatched* pair such as
/// `[TOOL_CALL]…[/TOOL_RESULT]` — which is not a closed pair and must not
/// cross-match — is still stripped rather than leaking its body to the user.
static LEGACY_BRACKET_TRAILING: Lazy<Regex> =
    Lazy::new(|| block_re(r"\[TOOL_(CALL|RESULT)\].*\z"));

/// MiniMax `mm:` / `minimax:` scaffolding blocks (tool calls + reasoning).
static MINIMAX_BLOCK: Lazy<Regex> = Lazy::new(|| {
    block_re(r"<(mm|minimax):(tool_call|tool_calls|reasoning|thinking)\b[^>]*>.*?</(mm|minimax):(tool_call|tool_calls|reasoning|thinking)>")
});

/// Unclosed trailing MiniMax scaffolding.
static MINIMAX_TRAILING: Lazy<Regex> = Lazy::new(|| {
    block_re(r"<(mm|minimax):(tool_call|tool_calls|reasoning|thinking)\b[^>]*>.*\z")
});

/// Generic XML tool-call scaffolding emitted as plain text by some models.
static XML_TOOL_BLOCK: Lazy<Regex> = Lazy::new(|| {
    block_re(
        r"<(tool_call|tool_calls|function_call|function_calls|function_response|invoke)\b[^>]*>.*?</(tool_call|tool_calls|function_call|function_calls|function_response|invoke)>",
    )
});

/// Unclosed trailing XML tool-call scaffolding. Runs after the closed-block
/// pass, so any remaining opener is unterminated → strip to end of text.
static XML_TOOL_TRAILING: Lazy<Regex> = Lazy::new(|| {
    block_re(
        r"<(tool_call|tool_calls|function_call|function_calls|function_response|invoke)\b[^>]*>.*\z",
    )
});

/// Orphaned XML tool-call tags (self-closing, or an open/close tag whose
/// partner was consumed by a non-greedy block match). `<function_calls>` wraps
/// `<invoke>`, so the closed-block pass ends at the inner `</invoke>` and leaves
/// the outer `</function_calls>` behind; without this pass that stray tag ships
/// to the user. Mirrors the equivalent `DSML_TAG` sweep below.
static XML_TOOL_TAG: Lazy<Regex> = Lazy::new(|| {
    block_re(
        r"</?(tool_call|tool_calls|function_call|function_calls|function_response|invoke)\b[^>]*/?>",
    )
});

/// DSML scaffolding (`<dsml:…>` … `</dsml:…>`) leaked by DeepSeek-family models.
static DSML_BLOCK: Lazy<Regex> =
    Lazy::new(|| block_re(r"<dsml:[a-z0-9_-]+\b[^>]*>.*?</dsml:[a-z0-9_-]+>"));

/// Standalone DSML tags (self-closing or orphaned open/close tags).
static DSML_TAG: Lazy<Regex> = Lazy::new(|| block_re(r"</?dsml:[a-z0-9_-]+\b[^>]*/?>"));

/// `<final>` sentinel wrappers — keep inner text.
static FINAL_OPEN: Lazy<Regex> = Lazy::new(|| block_re(r"<final\b[^>]*>"));
static FINAL_CLOSE: Lazy<Regex> = Lazy::new(|| block_re(r"</final>"));

/// Runtime-context sentinel lines (internal markers).
static RUNTIME_CONTEXT_LINE: Lazy<Regex> =
    Lazy::new(|| block_re(r"(?m)^[ \t]*\[\[runtime-(event|context)\]\][^\n]*\n?"));

/// Strip legacy `[TOOL_CALL]…[/TOOL_CALL]` / `[TOOL_RESULT]…[/TOOL_RESULT]`
/// blocks from a reply before delivery (v2026.5.2 heartbeat fix).
pub fn strip_legacy_tool_blocks(text: &str) -> String {
    let out = LEGACY_BRACKET_BLOCK.replace_all(text, "");
    LEGACY_BRACKET_TRAILING.replace_all(&out, "").into_owned()
}

/// Strip MiniMax / XML tool-call scaffolding from a reply (v2026.5.2).
pub fn strip_tool_scaffolding(text: &str) -> String {
    let out = MINIMAX_BLOCK.replace_all(text, "");
    let out = MINIMAX_TRAILING.replace_all(&out, "");
    let out = XML_TOOL_BLOCK.replace_all(&out, "");
    let out = XML_TOOL_TRAILING.replace_all(&out, "");
    let out = XML_TOOL_TAG.replace_all(&out, "");
    let out = DSML_BLOCK.replace_all(&out, "");
    DSML_TAG.replace_all(&out, "").into_owned()
}

/// Strip `<final>` sentinels (keeping inner text) and runtime-context
/// sentinel lines (v2026.7.1).
pub fn strip_sentinels(text: &str) -> String {
    let out = FINAL_OPEN.replace_all(text, "");
    let out = FINAL_CLOSE.replace_all(&out, "");
    let out = RUNTIME_CONTEXT_LINE.replace_all(&out, "");
    out.into_owned()
}

/// Collapse the whitespace damage left behind by block removal:
/// 3+ consecutive newlines → 2, and trim outer whitespace.
fn tidy_whitespace(text: &str) -> String {
    static MULTI_NEWLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
    MULTI_NEWLINE.replace_all(text, "\n\n").trim().to_string()
}

/// Shared user-facing reply sanitization.
///
/// Runs every strip pass in order and normalizes whitespace. Idempotent:
/// sanitizing already-clean text returns it unchanged (modulo outer trim).
pub fn sanitize_user_facing_reply(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    // Fast path: nothing that looks like scaffolding. Every pattern below opens
    // with either `[` (legacy tool blocks, `[[runtime-*]]` sentinels) or `<`
    // (MiniMax/XML/DSML/`<final>`), so those two bytes are a sound prefilter.
    //
    // This deliberately does NOT test for `"[TOOL_"`: the strip regexes are
    // case-insensitive, but `str::contains` is not, so a lowercase
    // `[tool_call]…[/tool_call]` used to escape through the fast path
    // completely unsanitized.
    if !text.contains('[') && !text.contains('<') {
        return text.to_string();
    }
    let out = strip_legacy_tool_blocks(text);
    let out = strip_tool_scaffolding(&out);
    let out = strip_sentinels(&out);
    tidy_whitespace(&out)
}

/// True when a sanitized reply has no user-visible content left — callers
/// should suppress delivery entirely (e.g. a reply that was *only* tool
/// scaffolding).
pub fn is_effectively_empty_reply(sanitized: &str) -> bool {
    sanitized.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Legacy [TOOL_CALL]/[TOOL_RESULT] blocks
    // ------------------------------------------------------------------

    #[test]
    fn strips_closed_tool_call_block() {
        let raw = "Sure! [TOOL_CALL]{\"name\":\"web_search\"}[/TOOL_CALL] Here you go.";
        assert_eq!(sanitize_user_facing_reply(raw), "Sure!  Here you go.");
    }

    #[test]
    fn strips_closed_tool_result_block() {
        let raw = "Answer: [TOOL_RESULT]{\"ok\":true}[/TOOL_RESULT]42";
        assert_eq!(sanitize_user_facing_reply(raw), "Answer: 42");
    }

    #[test]
    fn strips_multiline_tool_blocks() {
        let raw = "Hi\n[TOOL_CALL]\nline1\nline2\n[/TOOL_CALL]\nBye";
        assert_eq!(sanitize_user_facing_reply(raw), "Hi\n\nBye");
    }

    #[test]
    fn strips_unclosed_trailing_tool_call() {
        let raw = "Done. [TOOL_CALL]{\"name\":\"exec\"";
        assert_eq!(sanitize_user_facing_reply(raw), "Done.");
    }

    #[test]
    fn tool_blocks_case_insensitive() {
        let raw = "x [tool_call]y[/tool_call] z";
        assert_eq!(sanitize_user_facing_reply(raw), "x  z");
    }

    #[test]
    fn mismatched_bracket_kinds_do_not_cross_match() {
        // [TOOL_CALL]…[/TOOL_RESULT] is not a closed pair; the unclosed
        // trailing rule takes over instead.
        let raw = "keep [TOOL_CALL]abc[/TOOL_RESULT]";
        let out = sanitize_user_facing_reply(raw);
        assert!(out.starts_with("keep"), "prefix kept: {out:?}");
        assert!(!out.contains("abc"));
    }

    // ------------------------------------------------------------------
    // MiniMax scaffolding
    // ------------------------------------------------------------------

    #[test]
    fn strips_minimax_tool_call_block() {
        let raw = "ok <mm:tool_call>{\"name\":\"x\"}</mm:tool_call> done";
        assert_eq!(sanitize_user_facing_reply(raw), "ok  done");
    }

    #[test]
    fn strips_minimax_reasoning_block() {
        let raw = "<mm:reasoning>I should think…</mm:reasoning>The answer is 4.";
        assert_eq!(sanitize_user_facing_reply(raw), "The answer is 4.");
    }

    #[test]
    fn strips_minimax_long_prefix_variant() {
        let raw = "<minimax:tool_call>{}</minimax:tool_call>hi";
        assert_eq!(sanitize_user_facing_reply(raw), "hi");
    }

    #[test]
    fn strips_unclosed_minimax_trailing_block() {
        let raw = "answer<mm:tool_call>{\"partial\":";
        assert_eq!(sanitize_user_facing_reply(raw), "answer");
    }

    // ------------------------------------------------------------------
    // XML tool-call scaffolding
    // ------------------------------------------------------------------

    #[test]
    fn strips_xml_tool_call_block() {
        let raw = "a<tool_call>{\"name\":\"exec\"}</tool_call>b";
        assert_eq!(sanitize_user_facing_reply(raw), "ab");
    }

    #[test]
    fn strips_function_call_and_response_blocks() {
        let raw = "x<function_call>f()</function_call>y<function_response>r</function_response>z";
        assert_eq!(sanitize_user_facing_reply(raw), "xyz");
    }

    #[test]
    fn strips_invoke_block_with_attributes() {
        let raw = "pre <invoke name=\"web_search\"><param>q</param></invoke> post";
        assert_eq!(sanitize_user_facing_reply(raw), "pre  post");
    }

    #[test]
    fn strips_function_calls_wrapper() {
        let raw = "<function_calls><invoke name=\"a\"></invoke></function_calls>tail";
        assert_eq!(sanitize_user_facing_reply(raw), "tail");
    }

    #[test]
    fn strips_unclosed_trailing_xml_tool_call() {
        let raw = "visible text <tool_call>{\"name\": \"exec\", \"args\":";
        assert_eq!(sanitize_user_facing_reply(raw), "visible text");
    }

    // ------------------------------------------------------------------
    // DSML
    // ------------------------------------------------------------------

    #[test]
    fn strips_dsml_blocks() {
        let raw = "before<dsml:invoke name=\"t\">body</dsml:invoke>after";
        assert_eq!(sanitize_user_facing_reply(raw), "beforeafter");
    }

    #[test]
    fn strips_orphaned_dsml_tags() {
        let raw = "a</dsml:parameter>b<dsml:thing/>c";
        assert_eq!(sanitize_user_facing_reply(raw), "abc");
    }

    // ------------------------------------------------------------------
    // Sentinels
    // ------------------------------------------------------------------

    #[test]
    fn final_sentinel_wrapper_removed_inner_kept() {
        let raw = "<final>The actual reply.</final>";
        assert_eq!(sanitize_user_facing_reply(raw), "The actual reply.");
    }

    #[test]
    fn runtime_event_sentinel_lines_removed() {
        let raw = "[[runtime-event]] compaction memory flush\nReal text";
        assert_eq!(sanitize_user_facing_reply(raw), "Real text");
    }

    #[test]
    fn runtime_context_sentinel_lines_removed() {
        let raw = "Real text\n[[runtime-context]] internal note";
        assert_eq!(sanitize_user_facing_reply(raw), "Real text");
    }

    // ------------------------------------------------------------------
    // Preservation — normal content must survive untouched
    // ------------------------------------------------------------------

    #[test]
    fn plain_text_unchanged() {
        let raw = "Hello! Here's your summary.";
        assert_eq!(sanitize_user_facing_reply(raw), raw);
    }

    #[test]
    fn markdown_and_code_fences_preserved() {
        let raw = "Use this:\n```rust\nlet x = 1;\n```\nDone.";
        assert_eq!(sanitize_user_facing_reply(raw), raw);
    }

    #[test]
    fn html_like_but_benign_tags_preserved() {
        let raw = "Set <b>bold</b> and use <code>foo()</code>; a < b holds.";
        assert_eq!(sanitize_user_facing_reply(raw), raw);
    }

    #[test]
    fn bracket_text_that_is_not_tool_block_preserved() {
        let raw = "[TOOL_TIP] is not a tool call block";
        assert_eq!(sanitize_user_facing_reply(raw), raw);
    }

    #[test]
    fn sanitize_is_idempotent() {
        let raw = "a<tool_call>x</tool_call>b [TOOL_RESULT]y[/TOOL_RESULT] c";
        let once = sanitize_user_facing_reply(raw);
        let twice = sanitize_user_facing_reply(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn scaffolding_only_reply_is_effectively_empty() {
        let raw = "<mm:tool_call>{\"name\":\"x\"}</mm:tool_call>";
        let out = sanitize_user_facing_reply(raw);
        assert!(is_effectively_empty_reply(&out));
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(sanitize_user_facing_reply(""), "");
        assert!(is_effectively_empty_reply(""));
    }

    #[test]
    fn whitespace_collapsed_after_block_removal() {
        let raw = "top\n\n\n[TOOL_CALL]x[/TOOL_CALL]\n\n\n\nbottom";
        assert_eq!(sanitize_user_facing_reply(raw), "top\n\nbottom");
    }

    #[test]
    fn mixed_scaffolding_corpus() {
        let raw = concat!(
            "<final>",
            "[[runtime-event]] flush\n",
            "Result: <tool_call>{\"a\":1}</tool_call>done ",
            "<dsml:parameter>p</dsml:parameter>",
            "[TOOL_RESULT]raw[/TOOL_RESULT]",
            "</final>"
        );
        assert_eq!(sanitize_user_facing_reply(raw), "Result: done");
    }
}
