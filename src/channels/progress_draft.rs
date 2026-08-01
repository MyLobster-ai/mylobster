//! Shared progress-draft compositor for `streaming.mode: "progress"`.
//!
//! Compact behavior port of OpenClaw `src/channels/progress-draft-compositor.ts`
//! + `progress-draft-lines.ts` (v2026.5.x–6.x): channels that stream progress
//! render one *draft* message that merges tool, reasoning, and commentary
//! lanes until the final reply replaces it.
//!
//! Behaviors kept:
//! - Lane lines carry an identity; updating a line with the same identity
//!   replaces it in place instead of appending (`mergeChannelProgressDraftLine`).
//! - `streaming.progress.maxLines` clamps the rendered window to the most
//!   recent lines; `maxLineChars` clamps each line (char-boundary safe).
//! - Repeated identical renders are deduped (no no-op edits).
//! - Once the final reply starts, drafts stop rendering and the draft is
//!   cleared (`final reply wins`).
//! - Failed draft-start recovery: if the first render fails, the compositor
//!   resets so the next update retries the draft start instead of wedging.
//!
//! Channel senders call [`ProgressDraftCompositor::render`] and deliver the
//! returned text via their native edit/update APIs.

/// A progress lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressLane {
    Tool,
    Reasoning,
    Commentary,
}

/// One draft line with identity for in-place merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressDraftLine {
    /// Stable identity (e.g. `tool:web_fetch`, `reasoning`, `commentary`).
    pub id: String,
    pub lane: ProgressLane,
    pub text: String,
}

/// Render limits (`streaming.progress.{maxLines,maxLineChars}`).
#[derive(Debug, Clone, Copy)]
pub struct ProgressDraftLimits {
    pub max_lines: usize,
    pub max_line_chars: usize,
}

impl Default for ProgressDraftLimits {
    fn default() -> Self {
        // Upstream defaults: 6 lines, 200 chars/line.
        Self {
            max_lines: 6,
            max_line_chars: 200,
        }
    }
}

/// Outcome of a render request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressRender {
    /// Nothing to send (deduped, inactive, or final reply started).
    Skip,
    /// Send/edit the draft to this text.
    Update(String),
    /// Delete the current draft (final reply started with a live draft).
    Clear,
}

/// Stateful compositor for one streaming channel reply.
#[derive(Debug)]
pub struct ProgressDraftCompositor {
    lines: Vec<ProgressDraftLine>,
    limits: ProgressDraftLimits,
    seed: String,
    last_rendered: String,
    final_reply_started: bool,
    draft_live: bool,
}

impl ProgressDraftCompositor {
    pub fn new(seed: &str, limits: ProgressDraftLimits) -> Self {
        Self {
            lines: Vec::new(),
            limits,
            seed: seed.trim().to_string(),
            last_rendered: String::new(),
            final_reply_started: false,
            draft_live: false,
        }
    }

    /// Merge (or append) a lane line by identity.
    pub fn upsert_line(&mut self, line: ProgressDraftLine) {
        if let Some(existing) = self.lines.iter_mut().find(|l| l.id == line.id) {
            *existing = line;
        } else {
            self.lines.push(line);
        }
    }

    /// Remove a line by identity (e.g. tool finished before next render).
    pub fn remove_line(&mut self, id: &str) {
        self.lines.retain(|l| l.id != id);
    }

    fn clamp_line(text: &str, max_chars: usize) -> String {
        if max_chars == 0 || text.chars().count() <= max_chars {
            return text.to_string();
        }
        let clipped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{clipped}…")
    }

    fn compose(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.seed.is_empty() {
            parts.push(self.seed.clone());
        }
        let start = self.lines.len().saturating_sub(self.limits.max_lines.max(1));
        for line in &self.lines[start..] {
            let text = Self::clamp_line(line.text.trim(), self.limits.max_line_chars);
            if text.is_empty() {
                continue;
            }
            let rendered = match line.lane {
                ProgressLane::Commentary => format!("_{text}_"),
                _ => text,
            };
            parts.push(rendered);
        }
        parts.join("\n")
    }

    /// Render the current draft, deduping identical output.
    pub fn render(&mut self) -> ProgressRender {
        if self.final_reply_started {
            return ProgressRender::Skip;
        }
        let text = self.compose();
        if text.is_empty() || text == self.last_rendered {
            return ProgressRender::Skip;
        }
        self.last_rendered = text.clone();
        self.draft_live = true;
        ProgressRender::Update(text)
    }

    /// The draft update failed to deliver — reset so the next update retries
    /// the draft start (failed-draft-start recovery).
    pub fn on_render_failed(&mut self) {
        self.last_rendered.clear();
        self.draft_live = false;
    }

    /// The final reply is starting: drafts stop; a live draft is cleared.
    pub fn on_final_reply(&mut self) -> ProgressRender {
        self.final_reply_started = true;
        self.lines.clear();
        self.last_rendered.clear();
        if self.draft_live {
            self.draft_live = false;
            ProgressRender::Clear
        } else {
            ProgressRender::Skip
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, lane: ProgressLane, text: &str) -> ProgressDraftLine {
        ProgressDraftLine {
            id: id.into(),
            lane,
            text: text.into(),
        }
    }

    #[test]
    fn renders_and_dedupes() {
        let mut c = ProgressDraftCompositor::new("Working…", ProgressDraftLimits::default());
        c.upsert_line(line("tool:web_fetch", ProgressLane::Tool, "web_fetch example.com"));
        assert_eq!(
            c.render(),
            ProgressRender::Update("Working…\nweb_fetch example.com".into())
        );
        // Identical state: no no-op edit.
        assert_eq!(c.render(), ProgressRender::Skip);
    }

    #[test]
    fn identity_merge_replaces_in_place() {
        let mut c = ProgressDraftCompositor::new("", ProgressDraftLimits::default());
        c.upsert_line(line("tool:exec", ProgressLane::Tool, "exec: starting"));
        let _ = c.render();
        c.upsert_line(line("tool:exec", ProgressLane::Tool, "exec: 50%"));
        assert_eq!(c.render(), ProgressRender::Update("exec: 50%".into()));
    }

    #[test]
    fn commentary_is_italicized_and_lines_clamped() {
        let mut c = ProgressDraftCompositor::new(
            "",
            ProgressDraftLimits {
                max_lines: 2,
                max_line_chars: 10,
            },
        );
        c.upsert_line(line("a", ProgressLane::Tool, "first line"));
        c.upsert_line(line("b", ProgressLane::Reasoning, "abcdefghijKLMNOP"));
        c.upsert_line(line("c", ProgressLane::Commentary, "note"));
        // max_lines=2 keeps only the most recent two lines.
        assert_eq!(
            c.render(),
            ProgressRender::Update("abcdefghi…\n_note_".into())
        );
    }

    #[test]
    fn final_reply_wins_and_clears_live_draft() {
        let mut c = ProgressDraftCompositor::new("", ProgressDraftLimits::default());
        c.upsert_line(line("a", ProgressLane::Tool, "step"));
        let _ = c.render();
        assert_eq!(c.on_final_reply(), ProgressRender::Clear);
        // Draft updates after final are ignored.
        c.upsert_line(line("b", ProgressLane::Tool, "late"));
        assert_eq!(c.render(), ProgressRender::Skip);
    }

    #[test]
    fn final_reply_without_live_draft_skips_clear() {
        let mut c = ProgressDraftCompositor::new("", ProgressDraftLimits::default());
        assert_eq!(c.on_final_reply(), ProgressRender::Skip);
    }

    #[test]
    fn failed_draft_start_recovers() {
        let mut c = ProgressDraftCompositor::new("", ProgressDraftLimits::default());
        c.upsert_line(line("a", ProgressLane::Tool, "step"));
        assert!(matches!(c.render(), ProgressRender::Update(_)));
        // Delivery failed: next render retries the same text.
        c.on_render_failed();
        assert_eq!(c.render(), ProgressRender::Update("step".into()));
    }

    #[test]
    fn remove_line_updates_draft() {
        let mut c = ProgressDraftCompositor::new("", ProgressDraftLimits::default());
        c.upsert_line(line("a", ProgressLane::Tool, "one"));
        c.upsert_line(line("b", ProgressLane::Tool, "two"));
        let _ = c.render();
        c.remove_line("a");
        assert_eq!(c.render(), ProgressRender::Update("two".into()));
    }
}
