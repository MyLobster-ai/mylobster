//! Telegram streaming/progress window: multi-lane reasoning/commentary/tool
//! progress composed into one stable, durably-edited message with rotation,
//! dedupe, headings-shown-once, and clear-before-tool-output semantics.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `reasoning-lane-coordinator.ts`, `progress-summary.ts`, `draft-stream.ts`,
//! and the "clear progress drafts before visible tool output" fix (#93002),
//! on top of the port's durable-edit primitives (no Bot API draft state).

use super::telegram::{DurableEditOutcome, TelegramApi};
use super::telegram_format::{utf16_len, TELEGRAM_TEXT_CHUNK_LIMIT};
use super::telegram_net::TelegramApiError;

use std::collections::BTreeMap;
use std::collections::HashSet;
use tracing::debug;

/// Progress lanes rendered into the shared window, in stable order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProgressLane {
    Reasoning,
    Commentary,
    Tool,
}

impl ProgressLane {
    fn heading(&self) -> &'static str {
        match self {
            Self::Reasoning => "Reasoning",
            Self::Commentary => "Commentary",
            Self::Tool => "Tools",
        }
    }
}

/// What a progress update decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressUpdateAction {
    /// Rendered content unchanged — no edit issued (dedupe across modes).
    Deduped,
    /// Existing window edited in place.
    Edited,
    /// Content exceeded one message — old window finalized, new one started.
    Rotated { new_message_id: i64 },
    /// First window message created.
    Created { message_id: i64 },
}

/// Composes multi-lane progress into one message body. Headings are emitted
/// once per lane for the lifetime of the window (headings-shown-once), even
/// across rotations.
#[derive(Debug, Default)]
pub struct ProgressComposer {
    lanes: BTreeMap<ProgressLane, String>,
    emitted_headings: HashSet<ProgressLane>,
}

impl ProgressComposer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces a lane's content (streaming updates supply cumulative text).
    pub fn set_lane(&mut self, lane: ProgressLane, content: impl Into<String>) {
        self.lanes.insert(lane, content.into());
    }

    pub fn clear_lane(&mut self, lane: ProgressLane) {
        self.lanes.remove(&lane);
    }

    /// Renders the window body. A lane's heading is included only the first
    /// time that lane renders with content.
    pub fn render(&mut self) -> String {
        let mut sections = Vec::new();
        for (lane, content) in &self.lanes {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            if self.emitted_headings.contains(lane) {
                sections.push(trimmed.to_string());
            } else {
                self.emitted_headings.insert(*lane);
                sections.push(format!("**{}**\n{}", lane.heading(), trimmed));
            }
        }
        sections.join("\n\n")
    }
}

/// A stable progress window per (chat, thread): one real message durably
/// edited in place, rotated to a fresh message when the render exceeds the
/// chunk limit, deduped when the render is unchanged, and cleared (deleted)
/// before visible tool output.
#[derive(Debug)]
pub struct TelegramProgressWindow {
    chat_id: String,
    thread_id: Option<i64>,
    message_id: Option<i64>,
    last_rendered: String,
    pub composer: ProgressComposer,
}

impl TelegramProgressWindow {
    pub fn new(chat_id: impl Into<String>, thread_id: Option<i64>) -> Self {
        Self {
            chat_id: chat_id.into(),
            thread_id,
            message_id: None,
            last_rendered: String::new(),
            composer: ProgressComposer::new(),
        }
    }

    pub fn message_id(&self) -> Option<i64> {
        self.message_id
    }

    /// Pure rotation decision: rotate when the rendered body no longer fits a
    /// single message chunk.
    pub fn should_rotate(rendered: &str) -> bool {
        utf16_len(rendered) > TELEGRAM_TEXT_CHUNK_LIMIT
    }

    /// Pure dedupe decision.
    pub fn needs_edit(&self, rendered: &str) -> bool {
        !rendered.is_empty() && rendered != self.last_rendered
    }

    /// Applies the current composer state to Telegram.
    pub async fn apply(
        &mut self,
        api: &TelegramApi,
    ) -> Result<ProgressUpdateAction, TelegramApiError> {
        let rendered = self.composer.render();
        if !self.needs_edit(&rendered) {
            return Ok(ProgressUpdateAction::Deduped);
        }

        // Rotation: finalize the full old window, start fresh with the tail.
        if Self::should_rotate(&rendered) && self.message_id.is_some() {
            let tail_start = self.last_rendered.len().min(rendered.len());
            let tail = rendered[tail_start..].trim().to_string();
            let new_id = api
                .send_message_chunked(&self.chat_id, &tail, self.thread_id, None)
                .await?
                .unwrap_or_default();
            self.message_id = Some(new_id);
            self.last_rendered = rendered;
            debug!("telegram progress window rotated to message {new_id}");
            return Ok(ProgressUpdateAction::Rotated { new_message_id: new_id });
        }

        match self.message_id {
            Some(message_id) => {
                let outcome = api
                    .edit_message_durable(&self.chat_id, message_id, &rendered)
                    .await?;
                if outcome == DurableEditOutcome::MessageGone {
                    // Window vanished (deleted externally): recreate.
                    let new_id = api
                        .send_message_chunked(&self.chat_id, &rendered, self.thread_id, None)
                        .await?
                        .unwrap_or_default();
                    self.message_id = Some(new_id);
                    self.last_rendered = rendered;
                    return Ok(ProgressUpdateAction::Rotated { new_message_id: new_id });
                }
                self.last_rendered = rendered;
                Ok(ProgressUpdateAction::Edited)
            }
            None => {
                let new_id = api
                    .send_message_chunked(&self.chat_id, &rendered, self.thread_id, None)
                    .await?
                    .unwrap_or_default();
                self.message_id = Some(new_id);
                self.last_rendered = rendered;
                Ok(ProgressUpdateAction::Created { message_id: new_id })
            }
        }
    }

    /// Clears the progress bubble BEFORE visible tool output so stale
    /// progress never sits above real output (upstream #93002). Benign delete
    /// 400s are no-ops.
    pub async fn clear_before_tool_output(
        &mut self,
        api: &TelegramApi,
    ) -> Result<(), TelegramApiError> {
        if let Some(message_id) = self.message_id.take() {
            api.delete_message_benign(&self.chat_id, message_id).await?;
            self.last_rendered.clear();
        }
        Ok(())
    }

    /// Finalizes the window: keeps the last message as-is and detaches, so
    /// verbose status after a streamed final goes to a NEW message instead of
    /// overwriting the final (verbose-status isolation).
    pub fn finalize(&mut self) {
        self.message_id = None;
        self.last_rendered.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_orders_lanes_and_emits_headings_once() {
        let mut composer = ProgressComposer::new();
        composer.set_lane(ProgressLane::Commentary, "thinking about it");
        composer.set_lane(ProgressLane::Reasoning, "step 1");
        let first = composer.render();
        // Reasoning lane renders before Commentary (stable order).
        let reasoning_pos = first.find("**Reasoning**").unwrap();
        let commentary_pos = first.find("**Commentary**").unwrap();
        assert!(reasoning_pos < commentary_pos);

        // Second render with updated content: headings NOT repeated.
        composer.set_lane(ProgressLane::Reasoning, "step 1\nstep 2");
        let second = composer.render();
        assert!(!second.contains("**Reasoning**"));
        assert!(second.contains("step 2"));
    }

    #[test]
    fn composer_skips_empty_lanes() {
        let mut composer = ProgressComposer::new();
        composer.set_lane(ProgressLane::Tool, "  ");
        assert_eq!(composer.render(), "");
        composer.set_lane(ProgressLane::Tool, "running bash");
        assert!(composer.render().contains("**Tools**\nrunning bash"));
    }

    #[test]
    fn dedupe_and_rotation_decisions() {
        let mut window = TelegramProgressWindow::new("1", None);
        assert!(!window.needs_edit(""));
        assert!(window.needs_edit("content"));
        window.last_rendered = "content".to_string();
        assert!(!window.needs_edit("content"));
        assert!(window.needs_edit("content more"));

        assert!(!TelegramProgressWindow::should_rotate("short"));
        let long = "x".repeat(TELEGRAM_TEXT_CHUNK_LIMIT + 1);
        assert!(TelegramProgressWindow::should_rotate(&long));
    }

    #[test]
    fn finalize_detaches_window() {
        let mut window = TelegramProgressWindow::new("1", Some(5));
        window.message_id = Some(77);
        window.last_rendered = "final".to_string();
        window.finalize();
        assert_eq!(window.message_id(), None);
        assert!(window.needs_edit("anything"));
    }
}
