//! Gateway startup lifecycle helpers (v2026.5.2 / v2026.4.29 parity).
//!
//! - `StartupGate`: shared retryable "startup sidecars not ready" error for
//!   early control-plane RPCs (sessions.create/send/abort, agent.wait,
//!   tools.effective, chat.send).
//! - Secrets preflight: skip plugin-backed auth-profile overlays during the
//!   startup secrets preflight so gateway readiness is not blocked on plugin
//!   sidecars (v2026.5.2).
//! - Slow-host startup diagnostics + event-loop readiness checks
//!   (v2026.4.27 upstream).
//! - Startup diagnostics timeline (v2026.4.29) — recorded phases are
//!   consumable by `infra::doctor`.

use crate::gateway::protocol::OcResponseFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ============================================================================
// Startup gate — shared retryable startup-sidecars error (v2026.5.2)
// ============================================================================

/// JSON-RPC error code used for the shared retryable startup error.
pub const STARTUP_NOT_READY_CODE: i32 = -32050;

/// Control-plane RPC methods that must return the shared retryable
/// startup-sidecars error until sidecars are ready (v2026.5.2).
pub const STARTUP_GATED_METHODS: &[&str] = &[
    "chat.send",
    "agent",
    "agent.wait",
    "tools.effective",
    "tools.invoke",
    "sessions.create",
    "sessions.send",
    "sessions.abort",
];

/// Whether a method is gated behind startup-sidecar readiness.
pub fn is_startup_gated(method: &str) -> bool {
    STARTUP_GATED_METHODS.contains(&method)
}

/// Build the shared retryable startup error response. The message is
/// deliberately identical for every gated method so clients can pattern-match
/// one error shape.
pub fn retryable_startup_error(request_id: String) -> OcResponseFrame {
    OcResponseFrame::error(
        request_id,
        "gateway is still starting (sidecars not ready); retry shortly".to_string(),
        Some(STARTUP_NOT_READY_CODE),
    )
}

/// Tracks whether startup sidecars (channels, plugins, providers) have
/// finished initializing.
pub struct StartupGate {
    sidecars_ready: AtomicBool,
}

impl StartupGate {
    pub fn new() -> Self {
        Self {
            sidecars_ready: AtomicBool::new(false),
        }
    }

    pub fn mark_sidecars_ready(&self) {
        self.sidecars_ready.store(true, Ordering::Release);
    }

    pub fn sidecars_ready(&self) -> bool {
        self.sidecars_ready.load(Ordering::Acquire)
    }

    /// If `method` is gated and sidecars are not ready, return the shared
    /// retryable error to send; otherwise `None`.
    pub fn gate_check(&self, method: &str, request_id: &str) -> Option<OcResponseFrame> {
        if is_startup_gated(method) && !self.sidecars_ready() {
            Some(retryable_startup_error(request_id.to_string()))
        } else {
            None
        }
    }
}

impl Default for StartupGate {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Secrets preflight — skip plugin-backed auth-profile overlays (v2026.5.2)
// ============================================================================

/// Whether a secret path refers to a plugin-backed auth-profile overlay.
///
/// During the startup secrets preflight, these are skipped so that gateway
/// readiness does not block on plugin sidecar startup. They are still
/// resolved lazily on first use.
pub fn is_plugin_backed_auth_overlay(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let plugin_scoped = lower.starts_with("plugins.") || lower.contains(".plugins.");
    let auth_overlay = lower.contains("authprofile") || lower.contains("auth.profiles");
    (plugin_scoped && auth_overlay)
        || lower.starts_with("plugins.entries.") && lower.contains(".auth")
}

/// Filter the required-secret path list used during startup preflight,
/// removing plugin-backed auth-profile overlays.
pub fn preflight_required_secret_paths<S: AsRef<str>>(all: &[S]) -> Vec<String> {
    all.iter()
        .map(|p| p.as_ref().to_string())
        .filter(|p| !is_plugin_backed_auth_overlay(p))
        .collect()
}

// ============================================================================
// Startup diagnostics timeline (v2026.4.29) + slow-host detection (v2026.4.27)
// ============================================================================

/// A single recorded startup phase.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupPhase {
    pub name: String,
    pub duration_ms: u64,
}

/// Total-startup threshold beyond which a host is classified as slow.
pub const SLOW_HOST_TOTAL_MS: u64 = 10_000;

/// Per-phase threshold beyond which an individual phase is flagged.
pub const SLOW_PHASE_MS: u64 = 5_000;

#[derive(Default)]
pub struct StartupTimeline {
    phases: parking_lot::Mutex<Vec<StartupPhase>>,
}

impl StartupTimeline {
    pub fn record(&self, name: &str, duration: Duration) {
        self.phases.lock().push(StartupPhase {
            name: name.to_string(),
            duration_ms: duration.as_millis() as u64,
        });
    }

    pub fn snapshot(&self) -> Vec<StartupPhase> {
        self.phases.lock().clone()
    }

    pub fn total_ms(&self) -> u64 {
        self.phases.lock().iter().map(|p| p.duration_ms).sum()
    }

    /// Phases exceeding the slow-phase threshold.
    pub fn slow_phases(&self) -> Vec<StartupPhase> {
        self.phases
            .lock()
            .iter()
            .filter(|p| p.duration_ms >= SLOW_PHASE_MS)
            .cloned()
            .collect()
    }

    pub fn is_slow_host(&self) -> bool {
        self.total_ms() >= SLOW_HOST_TOTAL_MS
    }
}

static GLOBAL_TIMELINE: OnceLock<StartupTimeline> = OnceLock::new();

/// Global startup timeline recorder used by the gateway server and read by
/// `infra::doctor` for the startup diagnostics timeline.
pub fn startup_timeline() -> &'static StartupTimeline {
    GLOBAL_TIMELINE.get_or_init(StartupTimeline::default)
}

/// Record a phase on the global startup timeline.
pub fn record_startup_phase(name: &str, started: Instant) {
    startup_timeline().record(name, started.elapsed());
}

/// Snapshot the global startup timeline as JSON (for doctor / RPC use).
pub fn startup_timeline_snapshot() -> serde_json::Value {
    let tl = startup_timeline();
    serde_json::json!({
        "phases": tl.snapshot(),
        "totalMs": tl.total_ms(),
        "slowHost": tl.is_slow_host(),
        "slowPhases": tl.slow_phases(),
    })
}

// ============================================================================
// Event-loop readiness (v2026.4.27)
// ============================================================================

/// Lag threshold above which the async runtime is considered not-ready /
/// overloaded.
pub const EVENT_LOOP_READY_MAX_LAG: Duration = Duration::from_millis(100);

/// Measure current async runtime timer lag by sleeping a short interval and
/// measuring drift beyond the requested duration.
pub async fn measure_event_loop_lag() -> Duration {
    let requested = Duration::from_millis(10);
    let start = Instant::now();
    tokio::time::sleep(requested).await;
    start.elapsed().saturating_sub(requested)
}

/// Whether measured lag indicates a ready event loop.
pub fn event_loop_ready(lag: Duration) -> bool {
    lag <= EVENT_LOOP_READY_MAX_LAG
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- startup gate ----

    #[test]
    fn gated_methods_include_control_plane_set() {
        for m in ["chat.send", "agent.wait", "tools.effective", "sessions.create"] {
            assert!(is_startup_gated(m), "{m} should be gated");
        }
        assert!(!is_startup_gated("gateway.info"));
        assert!(!is_startup_gated("config.get"));
    }

    #[test]
    fn gate_returns_shared_retryable_error_until_ready() {
        let gate = StartupGate::new();
        let err = gate.gate_check("chat.send", "r1").expect("gated");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], STARTUP_NOT_READY_CODE);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retry shortly"));

        // Non-gated method passes even before readiness.
        assert!(gate.gate_check("gateway.info", "r2").is_none());

        gate.mark_sidecars_ready();
        assert!(gate.gate_check("chat.send", "r3").is_none());
    }

    #[test]
    fn shared_error_is_identical_across_methods() {
        let a = retryable_startup_error("x".into());
        let b = retryable_startup_error("x".into());
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    // ---- secrets preflight ----

    #[test]
    fn plugin_auth_overlays_are_skipped() {
        assert!(is_plugin_backed_auth_overlay(
            "plugins.entries.slack.authProfiles.default"
        ));
        assert!(is_plugin_backed_auth_overlay(
            "channels.plugins.matrix.authProfiles.main"
        ));
        assert!(!is_plugin_backed_auth_overlay("models.providers.anthropic.apiKey"));
        assert!(!is_plugin_backed_auth_overlay("gateway.auth.token"));
    }

    #[test]
    fn preflight_filters_plugin_overlays_only() {
        let all = vec![
            "gateway.auth.token".to_string(),
            "plugins.entries.foo.authProfiles.bar".to_string(),
            "models.providers.openai.apiKey".to_string(),
        ];
        let filtered = preflight_required_secret_paths(&all);
        assert_eq!(
            filtered,
            vec![
                "gateway.auth.token".to_string(),
                "models.providers.openai.apiKey".to_string()
            ]
        );
    }

    // ---- timeline ----

    #[test]
    fn timeline_records_and_flags_slow_phases() {
        let tl = StartupTimeline::default();
        tl.record("config", Duration::from_millis(20));
        tl.record("channels", Duration::from_millis(6_000));
        assert_eq!(tl.snapshot().len(), 2);
        assert_eq!(tl.total_ms(), 6_020);
        let slow = tl.slow_phases();
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].name, "channels");
        assert!(!tl.is_slow_host());
        tl.record("plugins", Duration::from_millis(5_000));
        assert!(tl.is_slow_host());
    }

    #[test]
    fn timeline_snapshot_json_shape() {
        let tl = StartupTimeline::default();
        tl.record("bind", Duration::from_millis(5));
        let v = serde_json::to_value(tl.snapshot()).unwrap();
        assert_eq!(v[0]["name"], "bind");
        assert_eq!(v[0]["durationMs"], 5);
    }

    // ---- event loop ----

    #[test]
    fn event_loop_readiness_threshold() {
        assert!(event_loop_ready(Duration::from_millis(0)));
        assert!(event_loop_ready(Duration::from_millis(100)));
        assert!(!event_loop_ready(Duration::from_millis(101)));
    }

    #[tokio::test]
    async fn measure_lag_returns_small_value_on_healthy_runtime() {
        let lag = measure_event_loop_lag().await;
        // On a healthy test runner the drift should be well under 1s.
        assert!(lag < Duration::from_secs(1));
    }
}
