use serde::{Deserialize, Serialize};

/// A cron schedule with optional stagger delay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronSchedule {
    pub kind: String,
    pub expr: String,
    pub tz: Option<String>,
    pub stagger_ms: Option<u64>,
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
}
