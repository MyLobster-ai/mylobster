use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A cron schedule with optional stagger delay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronSchedule {
    pub kind: String,
    pub expr: String,
    pub tz: Option<String>,
    pub stagger_ms: Option<u64>,
}

/// Cron job with v2026.3.11 isolated delivery and error tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub schedule: CronSchedule,
    pub message: String,
    pub session_key: Option<String>,
    pub enabled: bool,
    pub created_at: u64,
    /// Whether this job uses isolated delivery (v2026.3.11).
    /// When true, no ad hoc sends or fallback main-session summaries.
    #[serde(default)]
    pub isolated_delivery: bool,
    /// Last error reason recorded for this job (v2026.3.11).
    pub last_error_reason: Option<String>,
    /// Whether to retry deliberately silent jobs (v2026.3.11).
    /// Default false: subagent follow-up won't retry silent jobs.
    #[serde(default)]
    pub retry_silent: bool,
}

/// Per-job error state for status reporting (v2026.3.11).
#[derive(Debug, Clone, Default)]
pub struct CronErrorTracker {
    pub errors: HashMap<String, String>,
    pub total_error_count: u64,
}

impl CronErrorTracker {
    pub fn record_error(&mut self, job_id: &str, reason: &str) {
        self.errors.insert(job_id.to_string(), reason.to_string());
        self.total_error_count += 1;
    }

    pub fn clear_error(&mut self, job_id: &str) {
        self.errors.remove(job_id);
    }

    pub fn last_error(&self, job_id: &str) -> Option<&str> {
        self.errors.get(job_id).map(|s| s.as_str())
    }
}

/// Default stagger for top-of-hour cron expressions (5 minutes).
pub const DEFAULT_TOP_OF_HOUR_STAGGER_MS: u64 = 300_000;

/// Apply stagger delay before job execution.
/// Returns the duration to sleep before running the task.
pub fn apply_stagger(schedule: &CronSchedule, default_stagger_ms: Option<u64>) -> std::time::Duration {
    if let Some(ms) = schedule.stagger_ms {
        return std::time::Duration::from_millis(ms);
    }

    // Auto-apply default stagger for top-of-hour cron expressions
    if is_top_of_hour(&schedule.expr) {
        let ms = default_stagger_ms.unwrap_or(DEFAULT_TOP_OF_HOUR_STAGGER_MS);
        return std::time::Duration::from_millis(ms);
    }

    std::time::Duration::ZERO
}

/// Check if a cron expression fires at the top of the hour (minute = 0).
fn is_top_of_hour(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() >= 2 {
        return parts[0] == "0";
    }
    false
}

pub fn list_jobs() -> Vec<serde_json::Value> {
    vec![]
}

/// Migrate legacy cron storage to v2026.3.11 format.
/// Called by `mylobster doctor --fix`.
pub fn migrate_legacy_storage(jobs: &mut Vec<CronJob>) {
    for job in jobs.iter_mut() {
        // Ensure v2026.3.11 fields have defaults
        if !job.isolated_delivery {
            job.isolated_delivery = true; // New default: isolated delivery enabled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_schedule(kind: &str, expr: &str, stagger_ms: Option<u64>) -> CronSchedule {
        CronSchedule {
            kind: kind.to_string(),
            expr: expr.to_string(),
            tz: None,
            stagger_ms,
        }
    }

    #[test]
    fn test_apply_stagger_explicit() {
        let schedule = make_schedule("cron", "*/5 * * * *", Some(2000));
        let duration = apply_stagger(&schedule, None);
        assert_eq!(duration, std::time::Duration::from_millis(2000));
    }

    #[test]
    fn test_apply_stagger_zero_explicit() {
        let schedule = make_schedule("cron", "0 * * * *", Some(0));
        let duration = apply_stagger(&schedule, None);
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_apply_stagger_top_of_hour_auto() {
        let schedule = make_schedule("cron", "0 * * * *", None);
        let duration = apply_stagger(&schedule, None);
        assert_eq!(
            duration,
            std::time::Duration::from_millis(DEFAULT_TOP_OF_HOUR_STAGGER_MS)
        );
    }

    #[test]
    fn test_apply_stagger_top_of_hour_custom_default() {
        let schedule = make_schedule("cron", "0 12 * * *", None);
        let duration = apply_stagger(&schedule, Some(10_000));
        assert_eq!(duration, std::time::Duration::from_millis(10_000));
    }

    #[test]
    fn test_apply_stagger_non_top_of_hour_no_stagger() {
        let schedule = make_schedule("cron", "30 * * * *", None);
        let duration = apply_stagger(&schedule, None);
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_is_top_of_hour() {
        assert!(is_top_of_hour("0 * * * *"));
        assert!(is_top_of_hour("0 12 * * 1"));
        assert!(!is_top_of_hour("30 * * * *"));
        assert!(!is_top_of_hour("*/5 * * * *"));
        assert!(!is_top_of_hour(""));
    }

    #[test]
    fn test_cron_schedule_serde_round_trip() {
        let schedule = make_schedule("cron", "0 9 * * 1-5", Some(5000));
        let json = serde_json::to_string(&schedule).unwrap();
        let restored: CronSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.kind, "cron");
        assert_eq!(restored.expr, "0 9 * * 1-5");
        assert_eq!(restored.stagger_ms, Some(5000));
    }

    // ====================================================================
    // CronJob v2026.3.11 fields
    // ====================================================================

    fn make_job(id: &str) -> CronJob {
        CronJob {
            id: id.to_string(),
            name: format!("job-{id}"),
            schedule: make_schedule("cron", "0 * * * *", None),
            message: "hello".to_string(),
            session_key: None,
            enabled: true,
            created_at: 1000,
            isolated_delivery: false,
            last_error_reason: None,
            retry_silent: false,
        }
    }

    #[test]
    fn cron_job_serde_round_trip_with_v2026_3_11_fields() {
        let mut job = make_job("j1");
        job.isolated_delivery = true;
        job.last_error_reason = Some("timeout".to_string());
        job.retry_silent = true;

        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert!(restored.isolated_delivery);
        assert_eq!(restored.last_error_reason.as_deref(), Some("timeout"));
        assert!(restored.retry_silent);
    }

    #[test]
    fn cron_job_defaults_v2026_3_11_fields() {
        let raw = serde_json::json!({
            "id": "j2",
            "name": "test",
            "schedule": { "kind": "cron", "expr": "0 * * * *" },
            "message": "hi",
            "enabled": true,
            "createdAt": 1000
        });
        let job: CronJob = serde_json::from_value(raw).unwrap();
        assert!(!job.isolated_delivery);
        assert!(job.last_error_reason.is_none());
        assert!(!job.retry_silent);
    }

    // ====================================================================
    // CronErrorTracker (v2026.3.11)
    // ====================================================================

    #[test]
    fn error_tracker_starts_empty() {
        let tracker = CronErrorTracker::default();
        assert_eq!(tracker.total_error_count, 0);
        assert!(tracker.errors.is_empty());
    }

    #[test]
    fn error_tracker_record_and_query() {
        let mut tracker = CronErrorTracker::default();
        tracker.record_error("job-1", "timeout after 30s");
        assert_eq!(tracker.last_error("job-1"), Some("timeout after 30s"));
        assert_eq!(tracker.total_error_count, 1);
    }

    #[test]
    fn error_tracker_record_overwrites_previous() {
        let mut tracker = CronErrorTracker::default();
        tracker.record_error("job-1", "first error");
        tracker.record_error("job-1", "second error");
        assert_eq!(tracker.last_error("job-1"), Some("second error"));
        assert_eq!(tracker.total_error_count, 2);
    }

    #[test]
    fn error_tracker_clear_error() {
        let mut tracker = CronErrorTracker::default();
        tracker.record_error("job-1", "err");
        tracker.clear_error("job-1");
        assert!(tracker.last_error("job-1").is_none());
        // total_error_count is not decremented on clear
        assert_eq!(tracker.total_error_count, 1);
    }

    #[test]
    fn error_tracker_multiple_jobs() {
        let mut tracker = CronErrorTracker::default();
        tracker.record_error("job-1", "rate limit");
        tracker.record_error("job-2", "auth failure");
        assert_eq!(tracker.last_error("job-1"), Some("rate limit"));
        assert_eq!(tracker.last_error("job-2"), Some("auth failure"));
        assert!(tracker.last_error("job-3").is_none());
        assert_eq!(tracker.total_error_count, 2);
    }

    // ====================================================================
    // migrate_legacy_storage (v2026.3.11)
    // ====================================================================

    #[test]
    fn migrate_legacy_enables_isolated_delivery() {
        let mut jobs = vec![make_job("j1"), make_job("j2")];
        assert!(!jobs[0].isolated_delivery);
        assert!(!jobs[1].isolated_delivery);

        migrate_legacy_storage(&mut jobs);

        assert!(jobs[0].isolated_delivery);
        assert!(jobs[1].isolated_delivery);
    }

    #[test]
    fn migrate_legacy_preserves_already_isolated() {
        let mut job = make_job("j1");
        job.isolated_delivery = true;
        let mut jobs = vec![job];

        migrate_legacy_storage(&mut jobs);

        assert!(jobs[0].isolated_delivery);
    }

    #[test]
    fn migrate_legacy_empty_list() {
        let mut jobs: Vec<CronJob> = vec![];
        migrate_legacy_storage(&mut jobs);
        assert!(jobs.is_empty());
    }
}
