//! Delivery recovery (v2026.7.1 parity, gateway side).
//!
//! - Paced outbound + restart-continuation replays after outages: replays
//!   are spread under a wall-clock budget instead of rate-limit bursts.
//! - Atomic pre-connect send-evidence clearing (no double-replay of already
//!   sent replies).
//! - Honest partial delivery status + dead-lettered queue surfacing.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

// ============================================================================
// Replay pacing
// ============================================================================

/// Minimum gap between paced replay sends.
pub const MIN_REPLAY_GAP: Duration = Duration::from_millis(250);

/// Default wall-clock budget for a replay batch.
pub const DEFAULT_REPLAY_BUDGET: Duration = Duration::from_secs(60);

/// Pacing plan for replaying `n` queued deliveries under a wall-clock
/// budget. Never schedules faster than `MIN_REPLAY_GAP`.
#[derive(Debug, Clone)]
pub struct ReplayPacer {
    pub budget: Duration,
    pub gap: Duration,
}

impl ReplayPacer {
    /// Build a pacer for `count` pending replays under `budget`.
    pub fn plan(count: usize, budget: Duration) -> Self {
        let gap = if count <= 1 {
            MIN_REPLAY_GAP
        } else {
            let spread = budget / (count as u32);
            spread.max(MIN_REPLAY_GAP)
        };
        Self { budget, gap }
    }

    /// Delay before the `i`-th replay (0-based).
    pub fn delay_for(&self, i: usize) -> Duration {
        self.gap * (i as u32)
    }

    /// Whether the `i`-th replay still fits the wall-clock budget. Items
    /// past the budget stay queued for the next recovery pass instead of
    /// bursting.
    pub fn within_budget(&self, i: usize) -> bool {
        self.delay_for(i) <= self.budget
    }
}

// ============================================================================
// Send evidence (double-replay prevention)
// ============================================================================

/// Tracks message ids with confirmed send evidence per account, cleared
/// atomically before a reconnect so stale evidence can never suppress a
/// legitimate replay — and replays never duplicate confirmed sends.
#[derive(Default)]
pub struct SendEvidenceStore {
    confirmed: parking_lot::Mutex<HashMap<String, HashSet<String>>>,
}

impl SendEvidenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn confirm(&self, account: &str, message_id: &str) {
        self.confirmed
            .lock()
            .entry(account.to_string())
            .or_default()
            .insert(message_id.to_string());
    }

    pub fn is_confirmed(&self, account: &str, message_id: &str) -> bool {
        self.confirmed
            .lock()
            .get(account)
            .map(|s| s.contains(message_id))
            .unwrap_or(false)
    }

    /// Atomically clear an account's evidence before reconnect; returns the
    /// cleared ids (for audit logging).
    pub fn clear_before_connect(&self, account: &str) -> Vec<String> {
        self.confirmed
            .lock()
            .remove(account)
            .map(|s| {
                let mut v: Vec<String> = s.into_iter().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Filter a replay queue to only messages without confirmed evidence.
    pub fn filter_unsent(&self, account: &str, queued: &[String]) -> Vec<String> {
        let confirmed = self.confirmed.lock();
        let set = confirmed.get(account);
        queued
            .iter()
            .filter(|id| set.map(|s| !s.contains(*id)).unwrap_or(true))
            .cloned()
            .collect()
    }
}

// ============================================================================
// Honest delivery status + dead-letter surfacing
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered { sent: usize },
    Partial { sent: usize, failed: usize },
    Failed { failed: usize },
    /// Nothing attempted (empty queue).
    Empty,
}

/// Compute the honest delivery status for a replay pass.
pub fn resolve_delivery_status(sent: usize, failed: usize) -> DeliveryStatus {
    match (sent, failed) {
        (0, 0) => DeliveryStatus::Empty,
        (s, 0) => DeliveryStatus::Delivered { sent: s },
        (0, f) => DeliveryStatus::Failed { failed: f },
        (s, f) => DeliveryStatus::Partial { sent: s, failed: f },
    }
}

impl DeliveryStatus {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            DeliveryStatus::Delivered { sent } => {
                serde_json::json!({"status": "delivered", "sent": sent})
            }
            DeliveryStatus::Partial { sent, failed } => {
                serde_json::json!({"status": "partial", "sent": sent, "failed": failed})
            }
            DeliveryStatus::Failed { failed } => {
                serde_json::json!({"status": "failed", "failed": failed})
            }
            DeliveryStatus::Empty => serde_json::json!({"status": "empty"}),
        }
    }
}

/// Dead-letter queue for deliveries that exhausted retries; surfaced via
/// status RPCs rather than silently dropped.
#[derive(Default)]
pub struct DeadLetterQueue {
    items: parking_lot::Mutex<Vec<serde_json::Value>>,
}

/// Maximum dead-letter entries retained.
pub const DEAD_LETTER_CAP: usize = 200;

impl DeadLetterQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, account: &str, message_id: &str, error: &str) {
        let mut items = self.items.lock();
        if items.len() >= DEAD_LETTER_CAP {
            items.remove(0);
        }
        items.push(serde_json::json!({
            "account": account,
            "messageId": message_id,
            "error": error,
        }));
    }

    pub fn surface(&self) -> Vec<serde_json::Value> {
        self.items.lock().clone()
    }

    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pacing ----

    #[test]
    fn pacer_spreads_over_budget() {
        let p = ReplayPacer::plan(10, Duration::from_secs(60));
        assert_eq!(p.gap, Duration::from_secs(6));
        assert_eq!(p.delay_for(0), Duration::ZERO);
        assert_eq!(p.delay_for(9), Duration::from_secs(54));
        assert!(p.within_budget(9));
    }

    #[test]
    fn pacer_never_faster_than_min_gap() {
        let p = ReplayPacer::plan(10_000, Duration::from_secs(10));
        assert_eq!(p.gap, MIN_REPLAY_GAP);
        // Items beyond the budget wait for the next pass — no burst.
        assert!(p.within_budget(40));
        assert!(!p.within_budget(41));
    }

    #[test]
    fn pacer_single_item() {
        let p = ReplayPacer::plan(1, DEFAULT_REPLAY_BUDGET);
        assert_eq!(p.delay_for(0), Duration::ZERO);
        assert!(p.within_budget(0));
    }

    // ---- send evidence ----

    #[test]
    fn evidence_prevents_double_replay() {
        let store = SendEvidenceStore::new();
        store.confirm("acct", "m1");
        store.confirm("acct", "m2");
        let queued = vec!["m1".to_string(), "m2".to_string(), "m3".to_string()];
        assert_eq!(store.filter_unsent("acct", &queued), vec!["m3".to_string()]);
        // Other accounts unaffected
        assert_eq!(store.filter_unsent("other", &queued).len(), 3);
    }

    #[test]
    fn evidence_cleared_atomically_before_connect() {
        let store = SendEvidenceStore::new();
        store.confirm("acct", "m1");
        store.confirm("acct", "m2");
        let cleared = store.clear_before_connect("acct");
        assert_eq!(cleared, vec!["m1".to_string(), "m2".to_string()]);
        assert!(!store.is_confirmed("acct", "m1"));
        // Second clear is empty (already cleared)
        assert!(store.clear_before_connect("acct").is_empty());
    }

    // ---- status ----

    #[test]
    fn delivery_status_is_honest() {
        assert_eq!(resolve_delivery_status(0, 0), DeliveryStatus::Empty);
        assert_eq!(
            resolve_delivery_status(3, 0),
            DeliveryStatus::Delivered { sent: 3 }
        );
        assert_eq!(
            resolve_delivery_status(2, 1),
            DeliveryStatus::Partial { sent: 2, failed: 1 }
        );
        assert_eq!(
            resolve_delivery_status(0, 4),
            DeliveryStatus::Failed { failed: 4 }
        );
        let v = DeliveryStatus::Partial { sent: 2, failed: 1 }.to_json();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["sent"], 2);
        assert_eq!(v["failed"], 1);
    }

    // ---- dead letters ----

    #[test]
    fn dead_letter_queue_caps_and_surfaces() {
        let dlq = DeadLetterQueue::new();
        for i in 0..(DEAD_LETTER_CAP + 5) {
            dlq.push("acct", &format!("m{i}"), "gave up");
        }
        assert_eq!(dlq.len(), DEAD_LETTER_CAP);
        let surfaced = dlq.surface();
        // Oldest dropped
        assert_eq!(surfaced[0]["messageId"], "m5");
        assert_eq!(surfaced.last().unwrap()["messageId"], format!("m{}", DEAD_LETTER_CAP + 4));
    }
}
