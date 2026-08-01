//! Heartbeat runtime helpers (OpenClaw v2026.5.2 / v2026.7.1 parity).
//!
//! Covers the agent-side heartbeat behaviors:
//! - `heartbeat_respond` structured tool for tool-capable heartbeat runs
//!   (first valid call stops the run; `status:"ok"` is a quiet ack,
//!   `status:"alert"` carries a user-visible message).
//! - Active-hours-aware phase scheduling: quiet-hours timers are not armed;
//!   the next wake is deferred to the window start, honoring
//!   `activeHours.timezone` (UTC / fixed-offset supported).
//! - Centralized cooldown gating for exec-event / notification / spawn /
//!   retry wakes, plus a per-agent flood guard (max 5 wakes per 60s).
//! - `HEARTBEAT.md` directives are always appended to the heartbeat prompt.
//!
//! The scheduler/dispatcher that owns the actual timers is not yet ported;
//! these helpers are the behavior core it will call.

use crate::agents::tools::{AgentTool, ToolContext, ToolInfo, ToolResult};
use crate::config::types::HeartbeatActiveHours;

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Timelike, Utc};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::time::Instant;

// ============================================================================
// heartbeat_respond structured tool (v2026.5.2)
// ============================================================================

/// Outcome of a `heartbeat_respond` tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// Nothing to report — deliver nothing (quiet ack).
    Ok,
    /// Something needs the user's attention — deliver `message`.
    Alert(String),
}

/// Parse `heartbeat_respond` params into a structured outcome.
///
/// Returns `Err` for unknown statuses or an alert without a message so the
/// model gets a corrective tool error instead of a silent misfire.
pub fn parse_heartbeat_respond(params: &serde_json::Value) -> Result<HeartbeatOutcome, String> {
    let status = params
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let message = params
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match status.as_str() {
        "ok" => Ok(HeartbeatOutcome::Ok),
        "alert" => match message {
            Some(m) => Ok(HeartbeatOutcome::Alert(m.to_string())),
            None => Err("heartbeat_respond status \"alert\" requires a non-empty message".into()),
        },
        other => Err(format!(
            "heartbeat_respond status must be \"ok\" or \"alert\" (got {other:?})"
        )),
    }
}

/// Structured heartbeat response tool. Only surfaced on tool-capable
/// heartbeat runs (hidden from normal chat tool listings); the first valid
/// call ends the heartbeat run (v2026.7.1 "first-valid stop").
pub struct HeartbeatRespondTool;

impl HeartbeatRespondTool {
    pub fn tool_info() -> ToolInfo {
        ToolInfo {
            name: "heartbeat_respond".to_string(),
            description: "Report the outcome of a heartbeat check. Call with status \"ok\" when \
                          nothing needs attention (nothing is delivered), or status \"alert\" \
                          with a message when the user should be notified."
                .to_string(),
            category: "system".to_string(),
            // Hidden from normal chat listings — the heartbeat runner
            // unhides it for tool-capable heartbeat runs.
            hidden: true,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["ok", "alert"] },
                    "message": { "type": "string", "description": "User-visible alert text (required for status \"alert\")" }
                },
                "required": ["status"]
            }),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for HeartbeatRespondTool {
    fn info(&self) -> ToolInfo {
        Self::tool_info()
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _context: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        match parse_heartbeat_respond(&params) {
            Ok(HeartbeatOutcome::Ok) => Ok(ToolResult::json(serde_json::json!({
                "status": "ok",
                "stop": true,
            }))),
            Ok(HeartbeatOutcome::Alert(message)) => Ok(ToolResult::json(serde_json::json!({
                "status": "alert",
                "message": message,
                "stop": true,
            }))),
            Err(e) => Ok(ToolResult::error(e)),
        }
    }
}

// ============================================================================
// Active-hours-aware phase scheduling (v2026.5.2)
// ============================================================================

/// Parse an `activeHours.timezone` value into a fixed offset.
///
/// Supported: `"UTC"`/`"Z"`, `"+HH:MM"`, `"-HH:MM"`, `"+HHMM"`, `"+H"`.
/// IANA zone names are not resolvable without a tz database dependency and
/// return `None` (callers fall back to UTC).
pub fn parse_timezone_offset(tz: &str) -> Option<FixedOffset> {
    let t = tz.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("utc") || t == "Z" || t == "z" {
        return FixedOffset::east_opt(0);
    }
    let (sign, rest) = match t.as_bytes()[0] {
        b'+' => (1i32, &t[1..]),
        b'-' => (-1i32, &t[1..]),
        _ => return None,
    };
    let (h, m) = if let Some((h, m)) = rest.split_once(':') {
        (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
    } else if rest.len() == 4 {
        (
            rest[..2].parse::<i32>().ok()?,
            rest[2..].parse::<i32>().ok()?,
        )
    } else {
        (rest.parse::<i32>().ok()?, 0)
    };
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

fn effective_offset(cfg: &HeartbeatActiveHours) -> FixedOffset {
    cfg.timezone
        .as_deref()
        .and_then(parse_timezone_offset)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

/// Whether `now` falls inside the configured active-hours window.
///
/// `start`/`end` are hours in the configured timezone. A wrap-around window
/// (`start > end`, e.g. 22 → 6) spans midnight. `start == end` or a missing
/// bound means always active.
pub fn is_within_active_hours(cfg: &HeartbeatActiveHours, now: DateTime<Utc>) -> bool {
    let (start, end) = match (cfg.start, cfg.end) {
        (Some(s), Some(e)) => (s % 24, e % 24),
        _ => return true,
    };
    if start == end {
        return true;
    }
    let hour = now.with_timezone(&effective_offset(cfg)).hour();
    if start < end {
        (start..end).contains(&hour)
    } else {
        hour >= start || hour < end
    }
}

/// Next instant the active window opens at/after `now`.
/// Returns `now` when already inside the window.
pub fn next_active_start(cfg: &HeartbeatActiveHours, now: DateTime<Utc>) -> DateTime<Utc> {
    if is_within_active_hours(cfg, now) {
        return now;
    }
    let start = match cfg.start {
        Some(s) => s % 24,
        None => return now,
    };
    let offset = effective_offset(cfg);
    let local = now.with_timezone(&offset);
    let mut candidate = local
        .date_naive()
        .and_hms_opt(start, 0, 0)
        .expect("valid start hour")
        .and_local_timezone(offset)
        .single()
        .unwrap_or_else(|| local.with_time(chrono::NaiveTime::MIN).unwrap());
    if candidate <= local {
        candidate += ChronoDuration::days(1);
    }
    candidate.with_timezone(&Utc)
}

/// Compute the next heartbeat wake instant.
///
/// Quiet-hours phase scheduling (v2026.5.2): the naive `now + every` wake is
/// deferred to the start of the next active window instead of arming a timer
/// that would fire (and be skipped) during quiet hours.
pub fn next_heartbeat_wake(
    every: Duration,
    active_hours: Option<&HeartbeatActiveHours>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let naive = now + ChronoDuration::from_std(every).unwrap_or_else(|_| ChronoDuration::minutes(30));
    match active_hours {
        Some(cfg) if !is_within_active_hours(cfg, naive) => next_active_start(cfg, naive),
        _ => naive,
    }
}

// ============================================================================
// Centralized wake gating: cooldown + per-agent flood guard (v2026.5.2)
// ============================================================================

/// Why a heartbeat wake was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WakeReason {
    /// Regular cadence timer — bypasses the event cooldown (its cadence is
    /// already rate-limited) but still counts against the flood guard.
    Timer,
    ExecEvent,
    Notification,
    SubagentSpawn,
    Retry,
}

impl WakeReason {
    /// Event-driven wakes are gated through the centralized cooldown.
    pub fn is_cooldown_gated(&self) -> bool {
        !matches!(self, WakeReason::Timer)
    }
}

/// Decision for a wake request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDecision {
    Allowed,
    /// Denied: still inside the centralized event-wake cooldown.
    CooledDown { remaining: Duration },
    /// Denied: per-agent flood guard tripped (5 wakes / 60s).
    Flooded,
}

/// Per-agent flood guard limits (v2026.5.2): max 5 wakes per rolling 60s.
pub const FLOOD_MAX_WAKES: usize = 5;
pub const FLOOD_WINDOW: Duration = Duration::from_secs(60);

/// Default centralized cooldown between event-driven wakes.
pub const DEFAULT_EVENT_WAKE_COOLDOWN: Duration = Duration::from_secs(30);

/// Centralized heartbeat wake gate.
///
/// All wake sources (exec events, notifications, subagent spawns, retries,
/// cadence timers) funnel through [`HeartbeatWakeGate::try_wake`]; allowed
/// wakes are recorded so the flood guard sees every path.
pub struct HeartbeatWakeGate {
    cooldown: Duration,
    last_event_wake: HashMap<String, Instant>,
    recent_wakes: HashMap<String, VecDeque<Instant>>,
}

impl HeartbeatWakeGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_event_wake: HashMap::new(),
            recent_wakes: HashMap::new(),
        }
    }

    /// Request a wake for `agent_id` at `now`. Flood guard is checked first
    /// (it protects the agent regardless of wake source), then the
    /// centralized cooldown for event-driven reasons.
    pub fn try_wake(&mut self, agent_id: &str, reason: WakeReason, now: Instant) -> WakeDecision {
        // Flood guard: prune the rolling window, then check.
        let window = self.recent_wakes.entry(agent_id.to_string()).or_default();
        while let Some(front) = window.front() {
            if now.duration_since(*front) >= FLOOD_WINDOW {
                window.pop_front();
            } else {
                break;
            }
        }
        if window.len() >= FLOOD_MAX_WAKES {
            return WakeDecision::Flooded;
        }

        // Centralized cooldown for event-driven wakes.
        if reason.is_cooldown_gated() {
            if let Some(last) = self.last_event_wake.get(agent_id) {
                let elapsed = now.duration_since(*last);
                if elapsed < self.cooldown {
                    return WakeDecision::CooledDown {
                        remaining: self.cooldown - elapsed,
                    };
                }
            }
            self.last_event_wake.insert(agent_id.to_string(), now);
        }

        window.push_back(now);
        WakeDecision::Allowed
    }
}

impl Default for HeartbeatWakeGate {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_WAKE_COOLDOWN)
    }
}

// ============================================================================
// Prompt building (v2026.7.1: HEARTBEAT.md directives always appended)
// ============================================================================

/// Build the heartbeat prompt: workspace `HEARTBEAT.md` directives are always
/// appended after the base prompt (previously they were dropped when a custom
/// prompt was configured).
pub fn build_heartbeat_prompt(base_prompt: &str, heartbeat_md: Option<&str>) -> String {
    let base = base_prompt.trim();
    match heartbeat_md.map(str::trim).filter(|d| !d.is_empty()) {
        Some(directives) if base.is_empty() => directives.to_string(),
        Some(directives) => format!("{base}\n\n{directives}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn hours(start: u32, end: u32, tz: Option<&str>) -> HeartbeatActiveHours {
        HeartbeatActiveHours {
            start: Some(start),
            end: Some(end),
            timezone: tz.map(String::from),
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    // ------------------------------------------------------------------
    // heartbeat_respond parsing
    // ------------------------------------------------------------------

    #[test]
    fn respond_ok_parses() {
        let out = parse_heartbeat_respond(&serde_json::json!({"status": "ok"})).unwrap();
        assert_eq!(out, HeartbeatOutcome::Ok);
    }

    #[test]
    fn respond_ok_ignores_message() {
        let out =
            parse_heartbeat_respond(&serde_json::json!({"status": "OK", "message": "hi"})).unwrap();
        assert_eq!(out, HeartbeatOutcome::Ok);
    }

    #[test]
    fn respond_alert_requires_message() {
        assert!(parse_heartbeat_respond(&serde_json::json!({"status": "alert"})).is_err());
        assert!(
            parse_heartbeat_respond(&serde_json::json!({"status": "alert", "message": "  "}))
                .is_err()
        );
    }

    #[test]
    fn respond_alert_carries_message() {
        let out = parse_heartbeat_respond(
            &serde_json::json!({"status": "alert", "message": "disk almost full"}),
        )
        .unwrap();
        assert_eq!(out, HeartbeatOutcome::Alert("disk almost full".into()));
    }

    #[test]
    fn respond_unknown_status_rejected() {
        assert!(parse_heartbeat_respond(&serde_json::json!({"status": "maybe"})).is_err());
        assert!(parse_heartbeat_respond(&serde_json::json!({})).is_err());
    }

    #[tokio::test]
    async fn respond_tool_execute_ok_and_alert() {
        let ctx = ToolContext {
            session_key: "s".into(),
            agent_id: "a".into(),
            config: crate::config::Config::default(),
        };
        let tool = HeartbeatRespondTool;

        let ok = tool
            .execute(serde_json::json!({"status": "ok"}), &ctx)
            .await
            .unwrap();
        assert!(!ok.is_error);
        assert_eq!(ok.json.as_ref().unwrap()["stop"], true);

        let alert = tool
            .execute(serde_json::json!({"status": "alert", "message": "m"}), &ctx)
            .await
            .unwrap();
        assert_eq!(alert.json.as_ref().unwrap()["message"], "m");

        let bad = tool
            .execute(serde_json::json!({"status": "nah"}), &ctx)
            .await
            .unwrap();
        assert!(bad.is_error);
    }

    #[test]
    fn respond_tool_is_hidden_by_default() {
        assert!(HeartbeatRespondTool::tool_info().hidden);
        assert_eq!(HeartbeatRespondTool::tool_info().name, "heartbeat_respond");
    }

    // ------------------------------------------------------------------
    // timezone parsing
    // ------------------------------------------------------------------

    #[test]
    fn tz_utc_variants() {
        assert_eq!(parse_timezone_offset("UTC").unwrap().local_minus_utc(), 0);
        assert_eq!(parse_timezone_offset("Z").unwrap().local_minus_utc(), 0);
    }

    #[test]
    fn tz_fixed_offsets() {
        assert_eq!(
            parse_timezone_offset("+07:00").unwrap().local_minus_utc(),
            7 * 3600
        );
        assert_eq!(
            parse_timezone_offset("-05:30").unwrap().local_minus_utc(),
            -(5 * 3600 + 30 * 60)
        );
        assert_eq!(
            parse_timezone_offset("+0930").unwrap().local_minus_utc(),
            9 * 3600 + 30 * 60
        );
        assert_eq!(
            parse_timezone_offset("+7").unwrap().local_minus_utc(),
            7 * 3600
        );
    }

    #[test]
    fn tz_iana_names_unsupported() {
        assert!(parse_timezone_offset("America/New_York").is_none());
        assert!(parse_timezone_offset("").is_none());
        assert!(parse_timezone_offset("+25:00").is_none());
    }

    // ------------------------------------------------------------------
    // active hours
    // ------------------------------------------------------------------

    #[test]
    fn active_hours_simple_window() {
        let cfg = hours(9, 17, None); // UTC
        assert!(!is_within_active_hours(&cfg, utc(2026, 7, 20, 8, 59)));
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 9, 0)));
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 16, 59)));
        assert!(!is_within_active_hours(&cfg, utc(2026, 7, 20, 17, 0)));
    }

    #[test]
    fn active_hours_overnight_window() {
        let cfg = hours(22, 6, None);
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 23, 0)));
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 2, 0)));
        assert!(!is_within_active_hours(&cfg, utc(2026, 7, 20, 12, 0)));
    }

    #[test]
    fn active_hours_timezone_shifts_window() {
        // 9–17 in +07:00 = 02:00–10:00 UTC.
        let cfg = hours(9, 17, Some("+07:00"));
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 2, 0)));
        assert!(!is_within_active_hours(&cfg, utc(2026, 7, 20, 11, 0)));
    }

    #[test]
    fn active_hours_missing_bounds_always_active() {
        let cfg = HeartbeatActiveHours::default();
        assert!(is_within_active_hours(&cfg, utc(2026, 7, 20, 3, 0)));
        let same = hours(8, 8, None);
        assert!(is_within_active_hours(&same, utc(2026, 7, 20, 3, 0)));
    }

    #[test]
    fn next_active_start_same_day() {
        let cfg = hours(9, 17, None);
        let now = utc(2026, 7, 20, 6, 30);
        assert_eq!(next_active_start(&cfg, now), utc(2026, 7, 20, 9, 0));
    }

    #[test]
    fn next_active_start_rolls_to_next_day() {
        let cfg = hours(9, 17, None);
        let now = utc(2026, 7, 20, 18, 0);
        assert_eq!(next_active_start(&cfg, now), utc(2026, 7, 21, 9, 0));
    }

    #[test]
    fn next_active_start_identity_when_active() {
        let cfg = hours(9, 17, None);
        let now = utc(2026, 7, 20, 10, 0);
        assert_eq!(next_active_start(&cfg, now), now);
    }

    // ------------------------------------------------------------------
    // phase scheduling
    // ------------------------------------------------------------------

    #[test]
    fn wake_inside_window_unchanged() {
        let cfg = hours(9, 17, None);
        let now = utc(2026, 7, 20, 10, 0);
        let wake = next_heartbeat_wake(Duration::from_secs(30 * 60), Some(&cfg), now);
        assert_eq!(wake, utc(2026, 7, 20, 10, 30));
    }

    #[test]
    fn wake_in_quiet_hours_deferred_to_window_start() {
        let cfg = hours(9, 17, None);
        let now = utc(2026, 7, 20, 16, 45);
        // naive wake at 17:15 is quiet → deferred to next 09:00.
        let wake = next_heartbeat_wake(Duration::from_secs(30 * 60), Some(&cfg), now);
        assert_eq!(wake, utc(2026, 7, 21, 9, 0));
    }

    #[test]
    fn wake_without_active_hours_is_naive() {
        let now = utc(2026, 7, 20, 23, 50);
        let wake = next_heartbeat_wake(Duration::from_secs(600), None, now);
        assert_eq!(wake, utc(2026, 7, 21, 0, 0));
    }

    // ------------------------------------------------------------------
    // wake gate: cooldown + flood guard (tokio::time::pause)
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn event_wakes_gated_by_cooldown() {
        let mut gate = HeartbeatWakeGate::new(Duration::from_secs(30));
        let t0 = Instant::now();
        assert_eq!(
            gate.try_wake("a", WakeReason::ExecEvent, t0),
            WakeDecision::Allowed
        );
        // 10s later: still cooling.
        tokio::time::advance(Duration::from_secs(10)).await;
        match gate.try_wake("a", WakeReason::Notification, Instant::now()) {
            WakeDecision::CooledDown { remaining } => {
                assert_eq!(remaining, Duration::from_secs(20));
            }
            other => panic!("expected CooledDown, got {other:?}"),
        }
        // After the cooldown expires the next event wake is allowed.
        tokio::time::advance(Duration::from_secs(21)).await;
        assert_eq!(
            gate.try_wake("a", WakeReason::Retry, Instant::now()),
            WakeDecision::Allowed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timer_wakes_bypass_cooldown() {
        let mut gate = HeartbeatWakeGate::new(Duration::from_secs(30));
        assert_eq!(
            gate.try_wake("a", WakeReason::ExecEvent, Instant::now()),
            WakeDecision::Allowed
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        // Timer wake right inside the event cooldown is still allowed.
        assert_eq!(
            gate.try_wake("a", WakeReason::Timer, Instant::now()),
            WakeDecision::Allowed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_is_per_agent() {
        let mut gate = HeartbeatWakeGate::new(Duration::from_secs(30));
        let now = Instant::now();
        assert_eq!(
            gate.try_wake("a", WakeReason::ExecEvent, now),
            WakeDecision::Allowed
        );
        assert_eq!(
            gate.try_wake("b", WakeReason::ExecEvent, now),
            WakeDecision::Allowed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flood_guard_trips_at_five_wakes_per_minute() {
        // Zero cooldown isolates the flood guard.
        let mut gate = HeartbeatWakeGate::new(Duration::ZERO);
        for i in 0..FLOOD_MAX_WAKES {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(
                gate.try_wake("a", WakeReason::Timer, Instant::now()),
                WakeDecision::Allowed,
                "wake {i} should be allowed"
            );
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            gate.try_wake("a", WakeReason::Timer, Instant::now()),
            WakeDecision::Flooded
        );
        // Other agents are unaffected.
        assert_eq!(
            gate.try_wake("b", WakeReason::Timer, Instant::now()),
            WakeDecision::Allowed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flood_guard_window_rolls_off() {
        let mut gate = HeartbeatWakeGate::new(Duration::ZERO);
        for _ in 0..FLOOD_MAX_WAKES {
            gate.try_wake("a", WakeReason::Timer, Instant::now());
        }
        assert_eq!(
            gate.try_wake("a", WakeReason::Timer, Instant::now()),
            WakeDecision::Flooded
        );
        // After the 60s window passes, wakes are allowed again.
        tokio::time::advance(FLOOD_WINDOW).await;
        assert_eq!(
            gate.try_wake("a", WakeReason::Timer, Instant::now()),
            WakeDecision::Allowed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn denied_wakes_do_not_count_toward_flood_window() {
        let mut gate = HeartbeatWakeGate::new(Duration::from_secs(30));
        assert_eq!(
            gate.try_wake("a", WakeReason::ExecEvent, Instant::now()),
            WakeDecision::Allowed
        );
        // Burst of denied event wakes inside the cooldown.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(matches!(
                gate.try_wake("a", WakeReason::ExecEvent, Instant::now()),
                WakeDecision::CooledDown { .. }
            ));
        }
        // Only 1 recorded wake — the flood guard has 4 slots left.
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(31)).await;
            assert_eq!(
                gate.try_wake("a", WakeReason::ExecEvent, Instant::now()),
                WakeDecision::Allowed
            );
        }
    }

    // ------------------------------------------------------------------
    // prompt building
    // ------------------------------------------------------------------

    #[test]
    fn heartbeat_md_always_appended() {
        assert_eq!(
            build_heartbeat_prompt("Check things.", Some("- watch disk")),
            "Check things.\n\n- watch disk"
        );
    }

    #[test]
    fn heartbeat_md_alone_when_no_base() {
        assert_eq!(build_heartbeat_prompt("", Some("- watch disk")), "- watch disk");
    }

    #[test]
    fn empty_directives_ignored() {
        assert_eq!(build_heartbeat_prompt("Base.", Some("   ")), "Base.");
        assert_eq!(build_heartbeat_prompt("Base.", None), "Base.");
    }
}
