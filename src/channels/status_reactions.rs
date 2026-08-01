//! Lifecycle status-reaction controller shared by channel adapters.
//!
//! Ported behavior from OpenClaw's status-reaction lifecycle
//! (queued→thinking→tool→done/error) as extended in v2026.5.2/v2026.7.1:
//!
//! - Each lifecycle stage maps to one reaction emoji on the triggering
//!   message; advancing stages swaps the previous stage reaction for the new
//!   one.
//! - **When a run reaches a terminal stage (done/error), any stale
//!   non-terminal lifecycle reaction is removed** — even reactions left over
//!   from earlier runs on the same message (v2026.5.2 Channels row "Status
//!   reactions: remove stale non-terminal lifecycle reactions when run
//!   reaches done/error").
//! - Terminal reactions may be kept (`keep_terminal`) or removed after a TTL
//!   via [`StatusReactionLifecycle::sweep_stale`].
//!
//! Channels apply the returned [`ReactionEdit`]s through their native
//! reaction APIs.

use std::collections::HashMap;

/// Lifecycle stages, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleStage {
    Queued,
    Thinking,
    Tool,
    Done,
    Error,
}

impl LifecycleStage {
    pub fn is_terminal(&self) -> bool {
        matches!(self, LifecycleStage::Done | LifecycleStage::Error)
    }
}

/// Default lifecycle emojis (upstream defaults).
pub fn default_stage_emoji(stage: LifecycleStage) -> &'static str {
    match stage {
        LifecycleStage::Queued => "👀",
        LifecycleStage::Thinking => "🤔",
        LifecycleStage::Tool => "🛠️",
        LifecycleStage::Done => "✅",
        LifecycleStage::Error => "❌",
    }
}

/// A reaction mutation the channel adapter must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionEdit {
    Add { emoji: String },
    Remove { emoji: String },
}

/// Per-message lifecycle reaction state.
#[derive(Debug, Default, Clone)]
struct MessageReactionState {
    /// Emojis this controller currently has applied, with their stage.
    applied: Vec<(LifecycleStage, String)>,
    /// Millisecond timestamp of the last transition.
    updated_at_ms: u64,
}

/// Tracks lifecycle reactions per message and computes edit batches.
#[derive(Debug, Default)]
pub struct StatusReactionLifecycle {
    emojis: HashMap<LifecycleStage, String>,
    /// Keep the terminal reaction applied after done/error (default true).
    keep_terminal: bool,
    states: HashMap<String, MessageReactionState>,
}

impl StatusReactionLifecycle {
    pub fn new() -> Self {
        Self {
            emojis: HashMap::new(),
            keep_terminal: true,
            states: HashMap::new(),
        }
    }

    /// Override an emoji for a stage (from `messages.statusReactions.emojis`).
    pub fn with_emoji(mut self, stage: LifecycleStage, emoji: &str) -> Self {
        self.emojis.insert(stage, emoji.to_string());
        self
    }

    /// Whether terminal reactions stay applied (default) or are removed too.
    pub fn with_keep_terminal(mut self, keep: bool) -> Self {
        self.keep_terminal = keep;
        self
    }

    fn emoji_for(&self, stage: LifecycleStage) -> String {
        self.emojis
            .get(&stage)
            .cloned()
            .unwrap_or_else(|| default_stage_emoji(stage).to_string())
    }

    /// Transition a message to a lifecycle stage, returning the reaction
    /// edits to apply, in order (removals first, then the addition).
    ///
    /// Terminal transitions remove **all** applied non-terminal reactions —
    /// including any stale ones from prior runs — before adding the terminal
    /// reaction (or nothing when `keep_terminal` is false).
    pub fn transition(
        &mut self,
        message_id: &str,
        stage: LifecycleStage,
        now_ms: u64,
    ) -> Vec<ReactionEdit> {
        let new_emoji = self.emoji_for(stage);
        let state = self.states.entry(message_id.to_string()).or_default();
        state.updated_at_ms = now_ms;
        let mut edits: Vec<ReactionEdit> = Vec::new();

        if stage.is_terminal() {
            // Remove every stale non-terminal lifecycle reaction.
            for (applied_stage, emoji) in state.applied.drain(..) {
                if !applied_stage.is_terminal() {
                    edits.push(ReactionEdit::Remove { emoji });
                } else if !self.keep_terminal || emoji != new_emoji {
                    edits.push(ReactionEdit::Remove { emoji });
                }
            }
            if self.keep_terminal {
                edits.push(ReactionEdit::Add {
                    emoji: new_emoji.clone(),
                });
                state.applied.push((stage, new_emoji));
            }
            return edits;
        }

        // Non-terminal advance: swap the previous stage reaction.
        for (_, emoji) in state.applied.drain(..) {
            if emoji != new_emoji {
                edits.push(ReactionEdit::Remove { emoji });
            }
        }
        if !edits
            .iter()
            .any(|e| matches!(e, ReactionEdit::Add { emoji } if *emoji == new_emoji))
        {
            edits.push(ReactionEdit::Add {
                emoji: new_emoji.clone(),
            });
        }
        state.applied.push((stage, new_emoji));
        edits
    }

    /// Remove reactions for messages idle longer than `ttl_ms`; returns
    /// (message_id, edits) batches and forgets the messages.
    pub fn sweep_stale(&mut self, now_ms: u64, ttl_ms: u64) -> Vec<(String, Vec<ReactionEdit>)> {
        let stale_ids: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| now_ms.saturating_sub(s.updated_at_ms) >= ttl_ms)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in stale_ids {
            if let Some(state) = self.states.remove(&id) {
                let edits: Vec<ReactionEdit> = state
                    .applied
                    .into_iter()
                    .map(|(_, emoji)| ReactionEdit::Remove { emoji })
                    .collect();
                if !edits.is_empty() {
                    out.push((id, edits));
                }
            }
        }
        out
    }

    /// Forget a message without producing edits (message deleted upstream).
    pub fn forget(&mut self, message_id: &str) {
        self.states.remove(message_id);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_advance_swaps_reactions() {
        let mut lc = StatusReactionLifecycle::new();
        let e1 = lc.transition("m1", LifecycleStage::Queued, 0);
        assert_eq!(e1, vec![ReactionEdit::Add { emoji: "👀".into() }]);
        let e2 = lc.transition("m1", LifecycleStage::Thinking, 1);
        assert_eq!(
            e2,
            vec![
                ReactionEdit::Remove { emoji: "👀".into() },
                ReactionEdit::Add { emoji: "🤔".into() },
            ]
        );
    }

    #[test]
    fn terminal_removes_stale_non_terminal() {
        let mut lc = StatusReactionLifecycle::new();
        lc.transition("m1", LifecycleStage::Queued, 0);
        lc.transition("m1", LifecycleStage::Tool, 1);
        let edits = lc.transition("m1", LifecycleStage::Done, 2);
        assert!(edits.contains(&ReactionEdit::Remove { emoji: "🛠️".into() }));
        assert!(edits.contains(&ReactionEdit::Add { emoji: "✅".into() }));
        // No stale non-terminal reaction survives.
        assert!(!edits.contains(&ReactionEdit::Remove { emoji: "✅".into() }));
    }

    #[test]
    fn error_terminal_also_clears() {
        let mut lc = StatusReactionLifecycle::new();
        lc.transition("m1", LifecycleStage::Thinking, 0);
        let edits = lc.transition("m1", LifecycleStage::Error, 1);
        assert_eq!(
            edits,
            vec![
                ReactionEdit::Remove { emoji: "🤔".into() },
                ReactionEdit::Add { emoji: "❌".into() },
            ]
        );
    }

    #[test]
    fn keep_terminal_false_leaves_message_clean() {
        let mut lc = StatusReactionLifecycle::new().with_keep_terminal(false);
        lc.transition("m1", LifecycleStage::Queued, 0);
        let edits = lc.transition("m1", LifecycleStage::Done, 1);
        assert_eq!(edits, vec![ReactionEdit::Remove { emoji: "👀".into() }]);
    }

    #[test]
    fn custom_emojis_used() {
        let mut lc = StatusReactionLifecycle::new().with_emoji(LifecycleStage::Queued, "🦞");
        let edits = lc.transition("m1", LifecycleStage::Queued, 0);
        assert_eq!(edits, vec![ReactionEdit::Add { emoji: "🦞".into() }]);
    }

    #[test]
    fn repeated_terminal_transition_is_stable() {
        let mut lc = StatusReactionLifecycle::new();
        lc.transition("m1", LifecycleStage::Done, 0);
        // A second done transition keeps the same terminal reaction without
        // duplicate adds/removes of it.
        let edits = lc.transition("m1", LifecycleStage::Done, 1);
        assert_eq!(edits, vec![ReactionEdit::Add { emoji: "✅".into() }]);
    }

    #[test]
    fn sweep_removes_idle_message_reactions() {
        let mut lc = StatusReactionLifecycle::new();
        lc.transition("m1", LifecycleStage::Queued, 0);
        lc.transition("m2", LifecycleStage::Queued, 900);
        let swept = lc.sweep_stale(1_000, 1_000);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, "m1");
        assert_eq!(
            swept[0].1,
            vec![ReactionEdit::Remove { emoji: "👀".into() }]
        );
        // m2 untouched and still tracked.
        assert!(lc.sweep_stale(1_000, 1_000).is_empty());
    }
}
