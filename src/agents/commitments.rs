//! Inferred follow-up commitments (OpenClaw v2026.4.29 carryover, v2026.5.2).
//!
//! When the assistant promises follow-up work in a reply ("I'll check back in
//! an hour", "I'll follow up tomorrow"), OpenClaw extracts those commitments
//! in a hidden batched pass, scopes them per agent + channel, and delivers
//! the due ones on the next heartbeat.
//!
//! Upstream uses an LLM for extraction; this port ships a deterministic
//! heuristic extractor with the same store/delivery semantics — the extractor
//! is a free function so an LLM-backed pass can replace it later without
//! touching the store.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use uuid::Uuid;

// ============================================================================
// Extraction
// ============================================================================

/// A commitment inferred from an assistant reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredCommitment {
    /// The sentence containing the commitment (trimmed).
    pub text: String,
    /// Parsed relative due offset, when the phrasing includes one
    /// ("in 20 minutes", "in 2 hours", "tomorrow"). `None` = unscheduled;
    /// stored commitments without a due time become due after
    /// [`DEFAULT_FOLLOWUP_DELAY`].
    pub due_in: Option<ChronoDuration>,
}

/// Default follow-up delay for commitments without an explicit time.
pub const DEFAULT_FOLLOWUP_DELAY_MINUTES: i64 = 60;

static COMMITMENT_PHRASE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(i(?:'|’)ll|i\s+will|let\s+me|i(?:'|’)m\s+going\s+to)\s+(check\s+back|follow\s+up|get\s+back\s+to\s+you|circle\s+back|keep\s+you\s+posted|update\s+you|remind\s+you|check\s+on|look\s+into|monitor)",
    )
    .unwrap()
});

static DUE_IN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bin\s+(?:about\s+|around\s+|~\s*)?(\d+)\s*(minutes?|mins?|m\b|hours?|hrs?|h\b|days?|d\b)")
        .unwrap()
});

static DUE_WORD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(tomorrow|tonight|later\s+today|this\s+evening|next\s+week)\b").unwrap());

fn parse_due_in(sentence: &str) -> Option<ChronoDuration> {
    if let Some(caps) = DUE_IN.captures(sentence) {
        let n: i64 = caps.get(1)?.as_str().parse().ok()?;
        let unit = caps.get(2)?.as_str().to_ascii_lowercase();
        let dur = if unit.starts_with('m') {
            ChronoDuration::minutes(n)
        } else if unit.starts_with('h') {
            ChronoDuration::hours(n)
        } else {
            ChronoDuration::days(n)
        };
        return Some(dur);
    }
    if let Some(caps) = DUE_WORD.captures(sentence) {
        let word = caps.get(1)?.as_str().to_ascii_lowercase();
        let dur = match word.as_str() {
            "tomorrow" => ChronoDuration::days(1),
            "next week" => ChronoDuration::days(7),
            // tonight / later today / this evening → a few hours out.
            _ => ChronoDuration::hours(4),
        };
        return Some(dur);
    }
    None
}

/// Split text into rough sentences (heuristic: `.`, `!`, `?`, newline).
fn sentences(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c| matches!(c, '.' | '!' | '?' | '\n'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Extract inferred follow-up commitments from an assistant reply.
///
/// Batched: one pass over the reply, one commitment per matching sentence.
/// Returns an empty vec for replies without commitment phrasing.
pub fn infer_followup_commitments(reply: &str) -> Vec<InferredCommitment> {
    if reply.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for sentence in sentences(reply) {
        if COMMITMENT_PHRASE.is_match(sentence) {
            out.push(InferredCommitment {
                text: sentence.to_string(),
                due_in: parse_due_in(sentence),
            });
        }
    }
    out
}

// ============================================================================
// Store + heartbeat delivery
// ============================================================================

/// A stored commitment, scoped to agent + channel + session.
#[derive(Debug, Clone)]
pub struct Commitment {
    pub id: String,
    pub agent_id: String,
    /// Channel the commitment was made on (e.g. "telegram"); `None` for
    /// direct/gateway chats.
    pub channel: Option<String>,
    pub session_key: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
}

/// In-memory commitment store with per-agent/channel scoping and
/// heartbeat-drain semantics.
#[derive(Debug, Default)]
pub struct CommitmentStore {
    by_agent: HashMap<String, Vec<Commitment>>,
    /// Cap per agent so a chatty model can't grow the store unboundedly.
    cap_per_agent: usize,
}

pub const DEFAULT_COMMITMENT_CAP_PER_AGENT: usize = 50;

impl CommitmentStore {
    pub fn new() -> Self {
        Self {
            by_agent: HashMap::new(),
            cap_per_agent: DEFAULT_COMMITMENT_CAP_PER_AGENT,
        }
    }

    /// Record commitments inferred from a reply. Returns the stored entries.
    pub fn record(
        &mut self,
        agent_id: &str,
        channel: Option<&str>,
        session_key: &str,
        inferred: Vec<InferredCommitment>,
        now: DateTime<Utc>,
    ) -> Vec<Commitment> {
        let entries = self.by_agent.entry(agent_id.to_string()).or_default();
        let mut stored = Vec::new();
        for inf in inferred {
            if entries.len() >= self.cap_per_agent {
                break;
            }
            let due_at = now
                + inf
                    .due_in
                    .unwrap_or_else(|| ChronoDuration::minutes(DEFAULT_FOLLOWUP_DELAY_MINUTES));
            let c = Commitment {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                channel: channel.map(String::from),
                session_key: session_key.to_string(),
                text: inf.text,
                created_at: now,
                due_at,
            };
            entries.push(c.clone());
            stored.push(c);
        }
        stored
    }

    /// Number of pending commitments for an agent.
    pub fn pending(&self, agent_id: &str) -> usize {
        self.by_agent.get(agent_id).map(Vec::len).unwrap_or(0)
    }

    /// Peek all pending commitments for an agent (any channel).
    pub fn pending_for_agent(&self, agent_id: &str) -> Vec<&Commitment> {
        self.by_agent
            .get(agent_id)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Drain the commitments due at `now` for this agent — called by the
    /// heartbeat runner to fold due follow-ups into the heartbeat prompt.
    /// Drained entries are removed (delivery is one-shot).
    pub fn drain_due(&mut self, agent_id: &str, now: DateTime<Utc>) -> Vec<Commitment> {
        let Some(entries) = self.by_agent.get_mut(agent_id) else {
            return Vec::new();
        };
        let (due, pending): (Vec<_>, Vec<_>) =
            entries.drain(..).partition(|c| c.due_at <= now);
        *entries = pending;
        due
    }

    /// Drain due commitments scoped to one channel (heartbeats configured
    /// with a channel target only deliver commitments made on that channel).
    pub fn drain_due_for_channel(
        &mut self,
        agent_id: &str,
        channel: &str,
        now: DateTime<Utc>,
    ) -> Vec<Commitment> {
        let Some(entries) = self.by_agent.get_mut(agent_id) else {
            return Vec::new();
        };
        let (due, pending): (Vec<_>, Vec<_>) = entries
            .drain(..)
            .partition(|c| c.due_at <= now && c.channel.as_deref() == Some(channel));
        *entries = pending;
        due
    }

    /// Drop all commitments for a session (e.g. on `/reset`).
    pub fn clear_session(&mut self, agent_id: &str, session_key: &str) {
        if let Some(entries) = self.by_agent.get_mut(agent_id) {
            entries.retain(|c| c.session_key != session_key);
        }
    }
}

/// Render drained commitments as a heartbeat prompt block. Empty input →
/// empty string (heartbeat proceeds without a follow-up section).
pub fn format_heartbeat_followups(due: &[Commitment]) -> String {
    if due.is_empty() {
        return String::new();
    }
    let mut out = String::from("Pending follow-ups you committed to:\n");
    for c in due {
        out.push_str(&format!("- {} (from session {})\n", c.text, c.session_key));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap()
    }

    // ------------------------------------------------------------------
    // extraction
    // ------------------------------------------------------------------

    #[test]
    fn extracts_ill_check_back() {
        let out = infer_followup_commitments("Deployed. I'll check back in 20 minutes to verify.");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].due_in, Some(ChronoDuration::minutes(20)));
    }

    #[test]
    fn extracts_follow_up_tomorrow() {
        let out = infer_followup_commitments("I will follow up tomorrow with the results.");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].due_in, Some(ChronoDuration::days(1)));
    }

    #[test]
    fn extracts_curly_apostrophe_variant() {
        let out = infer_followup_commitments("I’ll get back to you in 2 hours.");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].due_in, Some(ChronoDuration::hours(2)));
    }

    #[test]
    fn extracts_let_me_check_on() {
        let out = infer_followup_commitments("Let me check on the build and report back.");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].due_in, None);
    }

    #[test]
    fn no_commitment_in_plain_reply() {
        assert!(infer_followup_commitments("The capital of France is Paris.").is_empty());
        assert!(infer_followup_commitments("").is_empty());
    }

    #[test]
    fn will_without_followup_verb_not_extracted() {
        // "I will explain" is not a follow-up commitment.
        assert!(infer_followup_commitments("I will explain the tradeoffs now.").is_empty());
    }

    #[test]
    fn batched_extraction_multiple_sentences() {
        let reply = "Done! I'll check back in 1 hour. Also, I'll remind you tomorrow about the renewal.";
        let out = infer_followup_commitments(reply);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].due_in, Some(ChronoDuration::hours(1)));
        assert_eq!(out[1].due_in, Some(ChronoDuration::days(1)));
    }

    #[test]
    fn later_today_maps_to_hours() {
        let out = infer_followup_commitments("I'll circle back later today.");
        assert_eq!(out[0].due_in, Some(ChronoDuration::hours(4)));
    }

    // ------------------------------------------------------------------
    // store + heartbeat drain
    // ------------------------------------------------------------------

    #[test]
    fn record_applies_default_delay_when_unscheduled() {
        let mut store = CommitmentStore::new();
        let stored = store.record(
            "main",
            Some("telegram"),
            "sess-1",
            vec![InferredCommitment { text: "Let me look into it".into(), due_in: None }],
            t0(),
        );
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].due_at,
            t0() + ChronoDuration::minutes(DEFAULT_FOLLOWUP_DELAY_MINUTES)
        );
    }

    #[test]
    fn drain_due_returns_only_due_and_removes_them() {
        let mut store = CommitmentStore::new();
        store.record(
            "main",
            None,
            "s",
            vec![
                InferredCommitment { text: "soon".into(), due_in: Some(ChronoDuration::minutes(10)) },
                InferredCommitment { text: "later".into(), due_in: Some(ChronoDuration::hours(5)) },
            ],
            t0(),
        );
        let due = store.drain_due("main", t0() + ChronoDuration::minutes(30));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].text, "soon");
        assert_eq!(store.pending("main"), 1);
        // Draining again at the same time returns nothing (one-shot).
        assert!(store.drain_due("main", t0() + ChronoDuration::minutes(30)).is_empty());
    }

    #[test]
    fn drain_is_scoped_per_agent() {
        let mut store = CommitmentStore::new();
        store.record(
            "a",
            None,
            "s",
            vec![InferredCommitment { text: "x".into(), due_in: Some(ChronoDuration::zero()) }],
            t0(),
        );
        assert!(store.drain_due("b", t0()).is_empty());
        assert_eq!(store.drain_due("a", t0()).len(), 1);
    }

    #[test]
    fn drain_for_channel_scopes_to_channel() {
        let mut store = CommitmentStore::new();
        store.record(
            "a",
            Some("telegram"),
            "s1",
            vec![InferredCommitment { text: "tg".into(), due_in: Some(ChronoDuration::zero()) }],
            t0(),
        );
        store.record(
            "a",
            Some("discord"),
            "s2",
            vec![InferredCommitment { text: "dc".into(), due_in: Some(ChronoDuration::zero()) }],
            t0(),
        );
        let due = store.drain_due_for_channel("a", "telegram", t0());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].text, "tg");
        // Discord one still pending.
        assert_eq!(store.pending("a"), 1);
    }

    #[test]
    fn store_caps_per_agent() {
        let mut store = CommitmentStore::new();
        let many: Vec<InferredCommitment> = (0..100)
            .map(|i| InferredCommitment { text: format!("c{i}"), due_in: None })
            .collect();
        let stored = store.record("a", None, "s", many, t0());
        assert_eq!(stored.len(), DEFAULT_COMMITMENT_CAP_PER_AGENT);
        assert_eq!(store.pending("a"), DEFAULT_COMMITMENT_CAP_PER_AGENT);
    }

    #[test]
    fn clear_session_drops_only_that_session() {
        let mut store = CommitmentStore::new();
        store.record(
            "a",
            None,
            "s1",
            vec![InferredCommitment { text: "one".into(), due_in: None }],
            t0(),
        );
        store.record(
            "a",
            None,
            "s2",
            vec![InferredCommitment { text: "two".into(), due_in: None }],
            t0(),
        );
        store.clear_session("a", "s1");
        let remaining = store.pending_for_agent("a");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_key, "s2");
    }

    #[test]
    fn heartbeat_format_lists_commitments() {
        let c = Commitment {
            id: "1".into(),
            agent_id: "a".into(),
            channel: None,
            session_key: "s".into(),
            text: "I'll check back".into(),
            created_at: t0(),
            due_at: t0(),
        };
        let block = format_heartbeat_followups(&[c]);
        assert!(block.contains("I'll check back"));
        assert!(block.contains("session s"));
        assert!(format_heartbeat_followups(&[]).is_empty());
    }
}
