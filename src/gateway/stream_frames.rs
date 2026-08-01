//! Protocol v4 chat stream frames + protocol negotiation (v2026.7.1 parity).
//!
//! - Explicit `deltaText` / `replace` fields on chat stream payloads: delta
//!   frames carry only the incremental text; final frames carry the full
//!   text with `replace: true`.
//! - Compatibility-range advertisement (`minProtocol`/`maxProtocol` in the
//!   hello payload) + protocol-mismatch diagnostics.
//! - Authenticated `node.presence.alive` event payload (v2026.4.29).
//! - `spawnedBy` propagation on subagent chat/broadcast payloads
//!   (v2026.4.29).

use crate::gateway::protocol::PROTOCOL_VERSION;

/// Minimum protocol version this gateway can speak.
pub const MIN_PROTOCOL_VERSION: u32 = 3;

// ============================================================================
// Protocol negotiation + mismatch diagnostics
// ============================================================================

/// Negotiate a protocol version against a client's advertised range.
/// Returns the negotiated version, or a diagnostic message on mismatch.
pub fn negotiate_protocol(
    client_min: Option<u32>,
    client_max: Option<u32>,
) -> Result<u32, String> {
    let client_min = client_min.unwrap_or(MIN_PROTOCOL_VERSION);
    let client_max = client_max.unwrap_or(PROTOCOL_VERSION);
    if client_min > client_max {
        return Err(format!(
            "protocol mismatch: client advertised inverted range {client_min}..{client_max}"
        ));
    }
    if client_min > PROTOCOL_VERSION {
        return Err(format!(
            "protocol mismatch: client requires >= v{client_min}, server supports \
             v{MIN_PROTOCOL_VERSION}..v{PROTOCOL_VERSION} — upgrade the gateway"
        ));
    }
    if client_max < MIN_PROTOCOL_VERSION {
        return Err(format!(
            "protocol mismatch: client supports <= v{client_max}, server requires \
             v{MIN_PROTOCOL_VERSION}..v{PROTOCOL_VERSION} — upgrade the client"
        ));
    }
    Ok(client_max.min(PROTOCOL_VERSION))
}

/// Compatibility-range advertisement fields for the hello payload.
pub fn protocol_range_advertisement() -> serde_json::Value {
    serde_json::json!({
        "minProtocol": MIN_PROTOCOL_VERSION,
        "maxProtocol": PROTOCOL_VERSION,
    })
}

// ============================================================================
// v4 delta/replace chat frames (emitting side)
// ============================================================================

/// Tracks accumulated text per run so successive delta payloads can be
/// annotated with the incremental `deltaText`.
#[derive(Default, Debug, Clone)]
pub struct ChatDeltaTracker {
    accumulated: String,
}

impl ChatDeltaTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Annotate a chat event payload (JSON form of `ChatEvent`) with v4
    /// stream-frame fields:
    ///
    /// - `state == "delta"` → `deltaText` is the suffix beyond previously
    ///   accumulated text. Non-append rewrites fall back to `replace: true`
    ///   with the full text.
    /// - `state == "final"` → `replace: true` (full text is authoritative).
    pub fn annotate(&mut self, payload: &mut serde_json::Value) {
        let state = payload
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let full_text = extract_text(payload);

        match state.as_str() {
            "delta" => {
                let Some(full) = full_text else { return };
                if let Some(suffix) = full.strip_prefix(self.accumulated.as_str()) {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "deltaText".to_string(),
                            serde_json::Value::String(suffix.to_string()),
                        );
                    }
                } else {
                    // Non-append rewrite → replace frame.
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("replace".to_string(), serde_json::Value::Bool(true));
                    }
                }
                self.accumulated = full;
            }
            "final" => {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("replace".to_string(), serde_json::Value::Bool(true));
                }
                if let Some(full) = full_text {
                    self.accumulated = full;
                }
            }
            _ => {}
        }
    }
}

/// Extract the concatenated text blocks from a chat event payload
/// (`message.content[].text`).
fn extract_text(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    Some(out)
}

// ============================================================================
// node.presence.alive (v2026.4.29)
// ============================================================================

/// Build the authenticated `node.presence.alive` event payload.
pub fn node_presence_alive_payload(
    node_id: &str,
    client_id: &str,
    ts_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "nodeId": node_id,
        "clientId": client_id,
        "ts": ts_ms,
        "alive": true,
    })
}

// ============================================================================
// spawnedBy on subagent broadcast (v2026.4.29)
// ============================================================================

/// Attach `spawnedBy` routing metadata to a chat/broadcast payload when the
/// run was spawned by a parent session.
pub fn attach_spawned_by(payload: &mut serde_json::Value, spawned_by: Option<&str>) {
    if let (Some(obj), Some(parent)) = (payload.as_object_mut(), spawned_by) {
        if !parent.is_empty() {
            obj.insert(
                "spawnedBy".to_string(),
                serde_json::Value::String(parent.to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta_payload(text: &str) -> serde_json::Value {
        json!({
            "runId": "r1",
            "sessionKey": "s1",
            "seq": 1,
            "state": "delta",
            "message": {"content": [{"type": "text", "text": text}]},
        })
    }

    // ---- negotiation ----

    #[test]
    fn negotiation_picks_highest_common() {
        assert_eq!(negotiate_protocol(Some(3), Some(4)), Ok(4));
        assert_eq!(negotiate_protocol(Some(3), Some(3)), Ok(3));
        assert_eq!(negotiate_protocol(Some(3), Some(9)), Ok(PROTOCOL_VERSION));
        assert_eq!(negotiate_protocol(None, None), Ok(PROTOCOL_VERSION));
    }

    #[test]
    fn negotiation_mismatch_diagnostics() {
        let err = negotiate_protocol(Some(9), Some(10)).unwrap_err();
        assert!(err.contains("upgrade the gateway"), "{err}");
        let err = negotiate_protocol(Some(1), Some(2)).unwrap_err();
        assert!(err.contains("upgrade the client"), "{err}");
        let err = negotiate_protocol(Some(5), Some(4)).unwrap_err();
        assert!(err.contains("inverted"), "{err}");
    }

    #[test]
    fn range_advertisement_shape() {
        let v = protocol_range_advertisement();
        assert_eq!(v["minProtocol"], MIN_PROTOCOL_VERSION);
        assert_eq!(v["maxProtocol"], PROTOCOL_VERSION);
    }

    // ---- delta frames ----

    #[test]
    fn delta_frames_carry_incremental_text() {
        let mut tracker = ChatDeltaTracker::new();

        let mut p1 = delta_payload("Hello");
        tracker.annotate(&mut p1);
        assert_eq!(p1["deltaText"], "Hello");
        assert!(p1.get("replace").is_none());

        let mut p2 = delta_payload("Hello, world");
        tracker.annotate(&mut p2);
        assert_eq!(p2["deltaText"], ", world");

        let mut p3 = delta_payload("Hello, world!");
        tracker.annotate(&mut p3);
        assert_eq!(p3["deltaText"], "!");
    }

    #[test]
    fn non_append_rewrite_becomes_replace() {
        let mut tracker = ChatDeltaTracker::new();
        let mut p1 = delta_payload("Hello world");
        tracker.annotate(&mut p1);
        // Model rewrote earlier text — not an append.
        let mut p2 = delta_payload("Goodbye");
        tracker.annotate(&mut p2);
        assert!(p2.get("deltaText").is_none());
        assert_eq!(p2["replace"], true);
        // Subsequent appends resume delta framing from the rewrite.
        let mut p3 = delta_payload("Goodbye!");
        tracker.annotate(&mut p3);
        assert_eq!(p3["deltaText"], "!");
    }

    #[test]
    fn final_frames_are_replace() {
        let mut tracker = ChatDeltaTracker::new();
        let mut p1 = delta_payload("partial");
        tracker.annotate(&mut p1);
        let mut fin = json!({
            "runId": "r1",
            "sessionKey": "s1",
            "seq": 9,
            "state": "final",
            "message": {"content": [{"type": "text", "text": "partial done"}]},
        });
        tracker.annotate(&mut fin);
        assert_eq!(fin["replace"], true);
        assert!(fin.get("deltaText").is_none());
    }

    #[test]
    fn error_frames_untouched() {
        let mut tracker = ChatDeltaTracker::new();
        let mut err = json!({"state": "error", "errorMessage": "boom"});
        tracker.annotate(&mut err);
        assert!(err.get("deltaText").is_none());
        assert!(err.get("replace").is_none());
    }

    #[test]
    fn multi_block_text_concatenated() {
        let mut tracker = ChatDeltaTracker::new();
        let mut p = json!({
            "state": "delta",
            "message": {"content": [
                {"type": "text", "text": "a"},
                {"type": "tool_use", "name": "t"},
                {"type": "text", "text": "b"},
            ]},
        });
        tracker.annotate(&mut p);
        assert_eq!(p["deltaText"], "ab");
    }

    // ---- presence + spawnedBy ----

    #[test]
    fn presence_alive_payload_shape() {
        let v = node_presence_alive_payload("node-1", "client-9", 1234);
        assert_eq!(v["nodeId"], "node-1");
        assert_eq!(v["clientId"], "client-9");
        assert_eq!(v["ts"], 1234);
        assert_eq!(v["alive"], true);
    }

    #[test]
    fn spawned_by_attached_only_when_present() {
        let mut p = json!({"runId": "r1"});
        attach_spawned_by(&mut p, Some("parent-session"));
        assert_eq!(p["spawnedBy"], "parent-session");

        let mut q = json!({"runId": "r2"});
        attach_spawned_by(&mut q, None);
        assert!(q.get("spawnedBy").is_none());

        let mut r = json!({"runId": "r3"});
        attach_spawned_by(&mut r, Some(""));
        assert!(r.get("spawnedBy").is_none());
    }
}
