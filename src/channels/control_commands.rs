//! Immediate chat control commands + outbound reply scrubbing.
//!
//! Ported behavior from OpenClaw v2026.6.x–7.1 (Channels row "Control
//! commands"):
//!
//! - Bare `stop` / `abort` / `wait` (any casing, surrounding whitespace) are
//!   immediate control commands, as are their slash forms; `/stop` is a
//!   fast-abort that skips queue draining.
//! - `/verbose on|off` toggles verbose mode across chat types.
//! - Raw provider errors are suppressed from chat (classification helper).
//! - Internal tool-trace banners and web-search citation markers are
//!   stripped from outbound replies.
//!
//! Gateway wiring lands in `src/gateway/chat.rs` (agents-core cluster —
//! HANDOFF: call [`parse_control_command`] on inbound text before enqueueing
//! a turn, and [`sanitize_outbound_reply`] on final reply text).

/// Parsed control command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    /// Abort the active run (bare `stop`/`abort` or `/stop` fast-abort).
    Stop { fast: bool },
    /// Pause queue processing (`wait`).
    Wait,
    /// Toggle verbose mode (`/verbose on|off`, bare `/verbose` = on).
    Verbose { on: bool },
}

/// Recognize an immediate control command in inbound chat text.
///
/// Only *bare* keywords qualify — `stop the build` is a normal message.
pub fn parse_control_command(text: &str) -> Option<ControlCommand> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "stop" | "abort" => return Some(ControlCommand::Stop { fast: false }),
        // Slash form is the fast-abort path (skips queue drain).
        "/stop" | "/abort" => return Some(ControlCommand::Stop { fast: true }),
        "wait" | "/wait" => return Some(ControlCommand::Wait),
        "/verbose" | "/verbose on" => return Some(ControlCommand::Verbose { on: true }),
        "/verbose off" => return Some(ControlCommand::Verbose { on: false }),
        _ => {}
    }
    None
}

/// True when an error string is a raw provider error that must not reach
/// chat verbatim (v2026.7.1 "raw provider errors suppressed from chat").
pub fn is_raw_provider_error(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("{\"error\"")
        || t.starts_with("Error: 4")
        || t.starts_with("Error: 5")
        || t.contains("\"type\":\"error\"")
        || t.contains("invalid_request_error")
        || t.contains("rate_limit_error")
        || t.contains("overloaded_error")
}

/// Strip internal tool-trace banners and serialized tool-call scaffolding
/// from a user-facing reply.
pub fn strip_internal_banners(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_tool_call_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[TOOL_CALL]") {
            in_tool_call_block = true;
            continue;
        }
        if in_tool_call_block {
            if trimmed.starts_with("[/TOOL_CALL]") || trimmed.is_empty() {
                in_tool_call_block = false;
            }
            continue;
        }
        // Internal tool-trace banner lines.
        if trimmed.starts_with("⚙ tool:")
            || trimmed.starts_with("⚙️ tool:")
            || trimmed.starts_with("[tool-trace]")
        {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Remove web-search citation markers (`【…】` clusters) from outbound text.
pub fn strip_citation_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '【' {
            // Skip to the matching closing bracket (or end of input).
            for inner in chars.by_ref() {
                if inner == '】' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Full outbound scrub: banners then citation markers, trimming trailing
/// whitespace introduced by removals.
pub fn sanitize_outbound_reply(text: &str) -> String {
    let stripped = strip_internal_banners(text);
    let stripped = strip_citation_markers(&stripped);
    stripped.trim_end().to_string()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_control_words() {
        assert_eq!(
            parse_control_command("stop"),
            Some(ControlCommand::Stop { fast: false })
        );
        assert_eq!(
            parse_control_command("  STOP  "),
            Some(ControlCommand::Stop { fast: false })
        );
        assert_eq!(
            parse_control_command("Abort"),
            Some(ControlCommand::Stop { fast: false })
        );
        assert_eq!(parse_control_command("wait"), Some(ControlCommand::Wait));
        // Embedded words are normal messages.
        assert_eq!(parse_control_command("stop the build"), None);
        assert_eq!(parse_control_command("please wait"), None);
    }

    #[test]
    fn slash_stop_is_fast_abort() {
        assert_eq!(
            parse_control_command("/stop"),
            Some(ControlCommand::Stop { fast: true })
        );
        assert_eq!(
            parse_control_command("/abort"),
            Some(ControlCommand::Stop { fast: true })
        );
    }

    #[test]
    fn verbose_toggle() {
        assert_eq!(
            parse_control_command("/verbose on"),
            Some(ControlCommand::Verbose { on: true })
        );
        assert_eq!(
            parse_control_command("/verbose"),
            Some(ControlCommand::Verbose { on: true })
        );
        assert_eq!(
            parse_control_command("/verbose off"),
            Some(ControlCommand::Verbose { on: false })
        );
    }

    #[test]
    fn provider_error_classification() {
        assert!(is_raw_provider_error(
            "{\"error\":{\"type\":\"rate_limit_error\"}}"
        ));
        assert!(is_raw_provider_error("Error: 429 Too Many Requests"));
        assert!(!is_raw_provider_error("All done!"));
    }

    #[test]
    fn banner_stripping() {
        let text = "Result ready.\n[TOOL_CALL] exec {\"cmd\":\"ls\"}\n[/TOOL_CALL]\n⚙ tool: web_fetch done\nFinal line.";
        let cleaned = strip_internal_banners(text);
        assert_eq!(cleaned, "Result ready.\nFinal line.");
    }

    #[test]
    fn citation_marker_stripping() {
        assert_eq!(
            strip_citation_markers("Rust is fast【3†src】 and safe【5】."),
            "Rust is fast and safe."
        );
        assert_eq!(strip_citation_markers("no markers"), "no markers");
        // Unterminated marker swallows to end (never leaks partial internals).
        assert_eq!(strip_citation_markers("text【broken"), "text");
    }

    #[test]
    fn full_sanitize() {
        let text = "Answer【1†a】\n[tool-trace] step\n";
        assert_eq!(sanitize_outbound_reply(text), "Answer");
    }
}
