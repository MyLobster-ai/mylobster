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
