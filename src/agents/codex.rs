//! Codex-harness policy helpers (OpenClaw v2026.5.2 parity).
//!
//! Behavior defaults for runs driven through the OpenAI Codex harness:
//! - App-server dynamic tools default to **native-first** (v2026.5.2).
//! - Direct source replies default to the **`message` tool** when
//!   `visibleReplies` is not configured (v2026.5.2).
//! - Message-tool-only source turns are no longer prompted to finish with
//!   `NO_REPLY` (v2026.5.2).
//! - Malformed tool-call argument repair is enabled for native Codex and
//!   Azure OpenAI Responses backends; generic OpenAI Responses endpoints are
//!   **out** of the repair gate (v2026.5.2).

// ============================================================================
// Dynamic tools mode (v2026.5.2)
// ============================================================================

/// How app-server dynamic tools are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicToolsMode {
    /// Prefer the Codex-native tool implementation, falling back to the
    /// OpenClaw implementation (v2026.5.2 default).
    NativeFirst,
    /// Prefer the OpenClaw implementation.
    HarnessFirst,
    /// Native tools only.
    NativeOnly,
}

/// Resolve the configured dynamic-tools mode; unset/unknown → native-first.
pub fn resolve_dynamic_tools_mode(configured: Option<&str>) -> DynamicToolsMode {
    match configured.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("harness-first") => DynamicToolsMode::HarnessFirst,
        Some(s) if s.eq_ignore_ascii_case("native-only") => DynamicToolsMode::NativeOnly,
        _ => DynamicToolsMode::NativeFirst,
    }
}

// ============================================================================
// Direct source reply default (v2026.5.2)
// ============================================================================

/// How a Codex-harness turn's reply reaches the source conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexReplyRoute {
    MessageTool,
    Automatic,
}

/// Codex-harness direct source replies default to the `message` tool when
/// `visibleReplies` is not explicitly configured; an explicit configuration
/// wins.
pub fn default_direct_source_reply_route(
    visible_replies_configured: Option<bool>,
) -> CodexReplyRoute {
    match visible_replies_configured {
        Some(true) => CodexReplyRoute::MessageTool,
        Some(false) => CodexReplyRoute::Automatic,
        None => CodexReplyRoute::MessageTool,
    }
}

// ============================================================================
// NO_REPLY prompting (v2026.5.2)
// ============================================================================

/// Whether the harness should append the "finish with NO_REPLY if nothing to
/// say" instruction to the turn prompt. v2026.5.2: message-tool-only source
/// turns must NOT be prompted for `NO_REPLY` — their bare finals are already
/// suppressed, and the token leaks into `message` tool sends.
pub fn should_prompt_no_reply(turn_is_message_tool_only: bool) -> bool {
    !turn_is_message_tool_only
}

// ============================================================================
// Malformed tool-call argument repair gate (v2026.5.2)
// ============================================================================

/// Whether tool-call argument repair applies to this provider/backend.
///
/// Enabled: native Codex backends (chatgpt.com backend-api) and Azure OpenAI
/// Responses. Generic OpenAI Responses (api.openai.com and custom compat
/// endpoints) stay out of the repair gate.
pub fn repair_gate_enabled(provider: &str, base_url: Option<&str>) -> bool {
    let p = provider.trim().to_ascii_lowercase();
    if p == "openai-codex" || p == "codex" {
        return true;
    }
    if p.contains("azure") {
        return true;
    }
    if let Some(url) = base_url {
        let u = url.to_ascii_lowercase();
        if u.contains("chatgpt.com/backend-api") {
            return true;
        }
        if u.contains(".openai.azure.com") || u.contains(".azure.com") {
            return true;
        }
    }
    false
}

/// Attempt to repair a malformed tool-call argument string into valid JSON.
///
/// Repairs applied (in order):
/// 1. Already-valid JSON object → returned as-is.
/// 2. Smart quotes (`“ ” ‘ ’`) normalized to ASCII quotes.
/// 3. Trailing garbage after the last balanced top-level object stripped
///    (e.g. duplicated `}{"a":1}` tails or appended prose).
/// 4. Truncated JSON closed (unterminated strings closed, missing `}`/`]`
///    appended), with trailing commas removed.
///
/// Returns `None` when no repair produces a JSON **object** (tool arguments
/// must be objects).
pub fn repair_malformed_tool_call_arguments(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(serde_json::json!({}));
    }

    // 1. Fast path.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }

    // 2. Smart-quote normalization.
    let normalized: String = trimmed
        .chars()
        .map(|c| match c {
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            other => other,
        })
        .collect();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&normalized) {
        if v.is_object() {
            return Some(v);
        }
    }

    // 3. Strip trailing garbage after the first balanced top-level object.
    if let Some(prefix) = balanced_object_prefix(&normalized) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(prefix) {
            if v.is_object() {
                return Some(v);
            }
        }
    }

    // 4. Close truncated JSON.
    let closed = close_truncated_json(&normalized);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&closed) {
        if v.is_object() {
            return Some(v);
        }
    }

    None
}

/// Longest prefix of `s` forming one balanced top-level `{…}` object
/// (string-aware). `None` when `s` doesn't start with `{` or never closes.
fn balanced_object_prefix(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Close a truncated JSON fragment: terminate an open string, drop a
/// trailing comma / dangling key, and append missing closers.
fn close_truncated_json(s: &str) -> String {
    let mut out = s.trim_end().to_string();
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in out.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    if in_string {
        out.push('"');
    }
    // Trailing comma or dangling `"key":` would still be invalid — trim them.
    loop {
        let t = out.trim_end().to_string();
        if let Some(stripped) = t.strip_suffix(',') {
            out = stripped.to_string();
            continue;
        }
        if let Some(stripped) = t.strip_suffix(':') {
            // dangling key — give it a null value.
            out = format!("{stripped}: null");
        }
        break;
    }
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // dynamic tools mode
    // ------------------------------------------------------------------

    #[test]
    fn dynamic_tools_default_native_first() {
        assert_eq!(resolve_dynamic_tools_mode(None), DynamicToolsMode::NativeFirst);
        assert_eq!(resolve_dynamic_tools_mode(Some("bogus")), DynamicToolsMode::NativeFirst);
        assert_eq!(resolve_dynamic_tools_mode(Some("native-first")), DynamicToolsMode::NativeFirst);
    }

    #[test]
    fn dynamic_tools_explicit_modes() {
        assert_eq!(
            resolve_dynamic_tools_mode(Some("harness-first")),
            DynamicToolsMode::HarnessFirst
        );
        assert_eq!(resolve_dynamic_tools_mode(Some("Native-Only")), DynamicToolsMode::NativeOnly);
    }

    // ------------------------------------------------------------------
    // direct source reply default
    // ------------------------------------------------------------------

    #[test]
    fn unconfigured_visible_replies_defaults_to_message_tool() {
        assert_eq!(default_direct_source_reply_route(None), CodexReplyRoute::MessageTool);
    }

    #[test]
    fn explicit_visible_replies_config_wins() {
        assert_eq!(
            default_direct_source_reply_route(Some(false)),
            CodexReplyRoute::Automatic
        );
        assert_eq!(
            default_direct_source_reply_route(Some(true)),
            CodexReplyRoute::MessageTool
        );
    }

    // ------------------------------------------------------------------
    // NO_REPLY prompting
    // ------------------------------------------------------------------

    #[test]
    fn message_tool_only_turns_not_prompted_for_no_reply() {
        assert!(!should_prompt_no_reply(true));
        assert!(should_prompt_no_reply(false));
    }

    // ------------------------------------------------------------------
    // repair gate
    // ------------------------------------------------------------------

    #[test]
    fn repair_gate_native_codex_and_azure_only() {
        assert!(repair_gate_enabled("openai-codex", None));
        assert!(repair_gate_enabled("codex", None));
        assert!(repair_gate_enabled("azure-openai", None));
        assert!(repair_gate_enabled("openai", Some("https://chatgpt.com/backend-api/codex")));
        assert!(repair_gate_enabled("openai", Some("https://myres.openai.azure.com/openai/v1")));
        // Generic OpenAI Responses stays out of the gate.
        assert!(!repair_gate_enabled("openai", None));
        assert!(!repair_gate_enabled("openai", Some("https://api.openai.com/v1")));
        assert!(!repair_gate_enabled("openai", Some("https://my-compat.example.com/v1")));
    }

    // ------------------------------------------------------------------
    // argument repair
    // ------------------------------------------------------------------

    #[test]
    fn valid_json_passes_through() {
        let v = repair_malformed_tool_call_arguments(r#"{"path": "a.txt", "n": 1}"#).unwrap();
        assert_eq!(v["path"], "a.txt");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        assert_eq!(
            repair_malformed_tool_call_arguments("").unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            repair_malformed_tool_call_arguments("   ").unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn smart_quotes_repaired() {
        let v = repair_malformed_tool_call_arguments("{\u{201C}cmd\u{201D}: \u{201C}ls\u{201D}}").unwrap();
        assert_eq!(v["cmd"], "ls");
    }

    #[test]
    fn trailing_garbage_stripped() {
        let v = repair_malformed_tool_call_arguments(r#"{"a": 1}{"a": 1}"#).unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));
        let v2 = repair_malformed_tool_call_arguments(r#"{"a": 1} and then some prose"#).unwrap();
        assert_eq!(v2, serde_json::json!({"a": 1}));
    }

    #[test]
    fn truncated_object_closed() {
        let v = repair_malformed_tool_call_arguments(r#"{"path": "src/main.rs", "content": "let x"#)
            .unwrap();
        assert_eq!(v["path"], "src/main.rs");
        assert_eq!(v["content"], "let x");
    }

    #[test]
    fn truncated_nested_structures_closed() {
        let v = repair_malformed_tool_call_arguments(r#"{"items": ["a", "b""#).unwrap();
        assert_eq!(v["items"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn trailing_comma_removed() {
        let v = repair_malformed_tool_call_arguments(r#"{"a": 1,"#).unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    #[test]
    fn dangling_key_gets_null() {
        let v = repair_malformed_tool_call_arguments(r#"{"a": 1, "b":"#).unwrap();
        assert_eq!(v["a"], 1);
        assert!(v["b"].is_null());
    }

    #[test]
    fn non_object_json_rejected() {
        assert!(repair_malformed_tool_call_arguments("[1, 2, 3]").is_none());
        assert!(repair_malformed_tool_call_arguments("\"just a string\"").is_none());
        assert!(repair_malformed_tool_call_arguments("complete nonsense !!!").is_none());
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_prefix_scan() {
        let v = repair_malformed_tool_call_arguments(r#"{"code": "if (a) { b(); }"} trailing"#)
            .unwrap();
        assert_eq!(v["code"], "if (a) { b(); }");
    }
}
