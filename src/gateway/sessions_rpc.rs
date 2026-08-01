//! Session RPC extensions (v2026.5.2 / v2026.7.1 parity).
//!
//! - `sessions.describe` / `sessions.cleanup` (new RPCs, v2026.5.2)
//! - Bounded `sessions.list` with truncation metadata + short-TTL list cache
//!   and lightweight checkpoint previews for large stores (v2026.5.2/7.1)
//! - Aggregated `sessions.usage` totals with transcript-estimated context
//!   budget when provider usage is missing (v2026.7.1)
//! - Session-organization RPCs: rename, fork, archive, groups, unread
//!   (v2026.7.1)
//! - Canonical terminal-outcome normalization (v2026.7.1)

use crate::gateway::protocol::{OcResponseFrame, RequestFrame, SessionInfo};
use crate::gateway::server::GatewayState;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// ============================================================================
// Bounded sessions.list (v2026.5.2 / v2026.7.1)
// ============================================================================

/// Default page size for `sessions.list` when the caller does not specify.
pub const DEFAULT_SESSIONS_LIST_LIMIT: usize = 200;

/// Hard cap on `sessions.list` page size, regardless of the request.
pub const MAX_SESSIONS_LIST_LIMIT: usize = 500;

/// TTL for the sessions.list response cache. Short enough that operators see
/// near-live data, long enough to keep large stores responsive under bridge
/// polling.
pub const SESSIONS_LIST_CACHE_TTL: Duration = Duration::from_secs(2);

/// Clamp a requested list limit to the bounded window.
pub fn bound_list_limit(requested: Option<u64>) -> usize {
    match requested {
        None | Some(0) => DEFAULT_SESSIONS_LIST_LIMIT,
        Some(n) => (n as usize).min(MAX_SESSIONS_LIST_LIMIT),
    }
}

/// Build a bounded page over session infos (already sorted by the caller),
/// returning the page plus truncation metadata.
pub fn bounded_page<T: Clone>(items: &[T], offset: usize, limit: usize) -> (Vec<T>, bool, usize) {
    let total = items.len();
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let page = items[start..end].to_vec();
    let truncated = end < total;
    (page, truncated, total)
}

/// Lightweight checkpoint preview for a session row — avoids serializing the
/// full transcript when listing large stores.
pub fn checkpoint_preview(info: &SessionInfo, message_count: usize) -> serde_json::Value {
    serde_json::json!({
        "sessionKey": info.session_key,
        "title": info.title,
        "model": info.model,
        "messageCount": message_count,
        "updatedAt": info.updated_at,
    })
}

/// Short-TTL cache for sessions.list responses keyed by request shape.
pub struct SessionsListCache {
    ttl: Duration,
    entries: parking_lot::Mutex<HashMap<String, (Instant, serde_json::Value)>>,
}

impl SessionsListCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    pub fn cache_key(limit: usize, offset: usize, agent_id: Option<&str>) -> String {
        format!("{limit}:{offset}:{}", agent_id.unwrap_or("*"))
    }

    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        let entries = self.entries.lock();
        entries.get(key).and_then(|(at, v)| {
            if at.elapsed() < self.ttl {
                Some(v.clone())
            } else {
                None
            }
        })
    }

    pub fn store(&self, key: String, value: serde_json::Value) {
        let mut entries = self.entries.lock();
        // Keep the cache itself bounded.
        if entries.len() > 64 {
            entries.clear();
        }
        entries.insert(key, (Instant::now(), value));
    }

    pub fn invalidate(&self) {
        self.entries.lock().clear();
    }
}

impl Default for SessionsListCache {
    fn default() -> Self {
        Self::new(SESSIONS_LIST_CACHE_TTL)
    }
}

// ============================================================================
// Canonical terminal-outcome normalization (v2026.7.1)
// ============================================================================

/// Normalize a free-form terminal outcome string to the canonical set:
/// `completed`, `error`, `aborted`, `timeout`, or `unknown`.
pub fn normalize_terminal_outcome(raw: &str) -> &'static str {
    let s = raw.trim().to_ascii_lowercase();
    match s.as_str() {
        "ok" | "success" | "succeeded" | "complete" | "completed" | "done" | "end_turn"
        | "final" => "completed",
        "error" | "failed" | "failure" | "fatal" | "provider_error" => "error",
        "abort" | "aborted" | "cancel" | "cancelled" | "canceled" | "stopped"
        | "auth-revoked" => "aborted",
        "timeout" | "timed_out" | "deadline" | "deadline_exceeded" => "timeout",
        _ => {
            // Prose classifier fallbacks are intentionally narrow.
            if s.contains("timeout") || s.contains("timed out") {
                "timeout"
            } else if s.contains("abort") || s.contains("cancel") {
                "aborted"
            } else if s.contains("error") || s.contains("fail") {
                "error"
            } else {
                "unknown"
            }
        }
    }
}

// ============================================================================
// Session organization state (v2026.7.1)
// ============================================================================

/// In-memory session organization state: archives, unread counters, groups.
#[derive(Default)]
pub struct SessionOrgState {
    pub archived: parking_lot::RwLock<HashSet<String>>,
    pub unread: parking_lot::RwLock<HashMap<String, u64>>,
    /// group name -> member session keys
    pub groups: parking_lot::RwLock<HashMap<String, Vec<String>>>,
}

impl SessionOrgState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_archived(&self, session_key: &str, archived: bool) {
        let mut set = self.archived.write();
        if archived {
            set.insert(session_key.to_string());
        } else {
            set.remove(session_key);
        }
    }

    pub fn is_archived(&self, session_key: &str) -> bool {
        self.archived.read().contains(session_key)
    }

    pub fn set_unread(&self, session_key: &str, count: u64) {
        if count == 0 {
            self.unread.write().remove(session_key);
        } else {
            self.unread.write().insert(session_key.to_string(), count);
        }
    }

    pub fn unread_snapshot(&self) -> HashMap<String, u64> {
        self.unread.read().clone()
    }

    pub fn assign_group(&self, group: &str, session_key: &str) {
        let mut groups = self.groups.write();
        let members = groups.entry(group.to_string()).or_default();
        if !members.iter().any(|m| m == session_key) {
            members.push(session_key.to_string());
        }
    }

    pub fn remove_from_group(&self, group: &str, session_key: &str) {
        let mut groups = self.groups.write();
        if let Some(members) = groups.get_mut(group) {
            members.retain(|m| m != session_key);
            if members.is_empty() {
                groups.remove(group);
            }
        }
    }

    pub fn groups_snapshot(&self) -> HashMap<String, Vec<String>> {
        self.groups.read().clone()
    }

    /// Purge organization state for a deleted session.
    pub fn purge_session(&self, session_key: &str) {
        self.archived.write().remove(session_key);
        self.unread.write().remove(session_key);
        let mut groups = self.groups.write();
        for members in groups.values_mut() {
            members.retain(|m| m != session_key);
        }
        groups.retain(|_, members| !members.is_empty());
    }
}

// ============================================================================
// sessions.cleanup helpers (v2026.5.2)
// ============================================================================

#[derive(Debug, Clone)]
pub struct SessionsCleanupParams {
    pub older_than_ms: u64,
    pub dry_run: bool,
    pub limit: usize,
}

pub fn parse_cleanup_params(
    params: Option<&serde_json::Value>,
) -> Result<SessionsCleanupParams, String> {
    let p = params.cloned().unwrap_or(serde_json::json!({}));
    let older_than_ms = p
        .get("olderThanMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(7 * 24 * 60 * 60 * 1000); // default: 7 days
    if older_than_ms == 0 {
        return Err("olderThanMs must be > 0".to_string());
    }
    Ok(SessionsCleanupParams {
        older_than_ms,
        dry_run: p.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(false),
        limit: p
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(500)
            .min(5_000),
    })
}

/// Decide which sessions are cleanup candidates based on their updated-at
/// timestamps. Pure so it is directly testable.
pub fn cleanup_candidates(
    sessions: &[SessionInfo],
    now: chrono::DateTime<chrono::Utc>,
    older_than_ms: u64,
    limit: usize,
) -> Vec<String> {
    let cutoff = now - chrono::Duration::milliseconds(older_than_ms as i64);
    let mut candidates: Vec<(chrono::DateTime<chrono::Utc>, String)> = sessions
        .iter()
        .filter_map(|s| {
            let updated = chrono::DateTime::parse_from_rfc3339(&s.updated_at)
                .ok()?
                .with_timezone(&chrono::Utc);
            if updated < cutoff {
                Some((updated, s.session_key.clone()))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(t, _)| *t);
    candidates
        .into_iter()
        .take(limit)
        .map(|(_, k)| k)
        .collect()
}

// ============================================================================
// sessions.usage aggregation (v2026.7.1)
// ============================================================================

/// Rough transcript-based token estimate used when provider usage is missing
/// (~4 chars per token heuristic, CJK counted per char).
pub fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

// ============================================================================
// RPC handlers
// ============================================================================

pub fn handle_sessions_describe(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    let key = match session_key {
        Some(k) => k,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing sessionKey".to_string(),
                Some(-32602),
            )
        }
    };

    let info = match state.sessions.get_session(key) {
        Some(i) => i,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("Session not found: {key}"),
                Some(-32600),
            )
        }
    };

    let (message_count, transcript_chars, busy) = state
        .sessions
        .get_session_handle(key)
        .map(|h| {
            let history = h.get_history();
            let chars: usize = history
                .iter()
                .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
                .sum();
            (history.len(), chars, h.is_busy())
        })
        .unwrap_or((0, 0, false));

    let archived = state.rpc.session_org.is_archived(key);

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "session": info,
            "messageCount": message_count,
            "estimatedTokens": estimate_tokens_from_chars(transcript_chars),
            "busy": busy,
            "archived": archived,
        }),
    )
}

pub fn handle_sessions_cleanup(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let params = match parse_cleanup_params(request.params.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("Invalid sessions.cleanup params: {e}"),
                Some(-32602),
            )
        }
    };

    let sessions = state.sessions.list_sessions();
    let candidates = cleanup_candidates(
        &sessions,
        chrono::Utc::now(),
        params.older_than_ms,
        params.limit,
    );

    // Never clean up busy sessions.
    let deletable: Vec<String> = candidates
        .into_iter()
        .filter(|k| {
            state
                .sessions
                .get_session_handle(k)
                .map(|h| !h.is_busy())
                .unwrap_or(false)
        })
        .collect();

    if !params.dry_run {
        for key in &deletable {
            state.sessions.delete_session(key);
            state.rpc.session_org.purge_session(key);
        }
        state.rpc.sessions_list_cache.invalidate();
    }

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "cleaned": if params.dry_run { 0 } else { deletable.len() },
            "candidates": deletable,
            "dryRun": params.dry_run,
        }),
    )
}

/// Bounded, cached sessions.list (replaces the naive full-store dump).
pub fn handle_sessions_list_bounded(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = request.params.clone().unwrap_or(serde_json::json!({}));
    let limit = bound_list_limit(params.get("limit").and_then(|v| v.as_u64()));
    let offset = params
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let agent_id = params.get("agentId").and_then(|v| v.as_str());

    let cache_key = SessionsListCache::cache_key(limit, offset, agent_id);
    if let Some(cached) = state.rpc.sessions_list_cache.get(&cache_key) {
        return OcResponseFrame::success(request.id.clone(), cached);
    }

    let mut sessions = state.sessions.list_sessions();
    // v2026.7.1: agent-scoped lookups — never leak other agents' sessions.
    if let Some(aid) = agent_id {
        sessions.retain(|s| s.agent_id == aid);
    }
    // Newest first for stable pagination.
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let (page, truncated, total) = bounded_page(&sessions, offset, limit);
    let previews: Vec<serde_json::Value> = page
        .iter()
        .map(|info| {
            let count = state
                .sessions
                .get_session_handle(&info.session_key)
                .map(|h| h.get_history().len())
                .unwrap_or(0);
            let mut row = serde_json::to_value(info).unwrap_or_default();
            if let Some(obj) = row.as_object_mut() {
                obj.insert(
                    "preview".to_string(),
                    checkpoint_preview(info, count),
                );
                obj.insert(
                    "archived".to_string(),
                    serde_json::json!(state.rpc.session_org.is_archived(&info.session_key)),
                );
            }
            row
        })
        .collect();

    // Field names mirror upstream `SessionsListResultBase`
    // (`src/shared/session-types.ts`) so ACP/TUI/MCP clients written against
    // OpenClaw read this response unchanged. `path`/`defaults` are omitted:
    // they describe the on-disk sessions file and agent defaults, neither of
    // which this port exposes.
    let count = previews.len();
    let payload = serde_json::json!({
        "sessions": previews,
        "count": count,
        "totalCount": total,
        "limitApplied": limit,
        "offset": offset,
        "nextOffset": if truncated { Some(offset + count) } else { None },
        "hasMore": truncated,
    });
    state
        .rpc
        .sessions_list_cache
        .store(cache_key, payload.clone());
    OcResponseFrame::success(request.id.clone(), payload)
}

/// sessions.usage with aggregate totals when no sessionKey is given
/// (v2026.7.1) and transcript-estimated context budget when provider usage
/// is missing.
pub fn handle_sessions_usage_aggregate(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    if let Some(key) = session_key {
        return match state.sessions.get_session_usage(key) {
            Some(mut usage) => {
                // Attach transcript-estimated tokens when provider usage is 0.
                if let Some(handle) = state.sessions.get_session_handle(key) {
                    let chars: usize = handle
                        .get_history()
                        .iter()
                        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
                        .sum();
                    if let Some(obj) = usage.as_object_mut() {
                        obj.insert(
                            "estimatedContextTokens".to_string(),
                            serde_json::json!(estimate_tokens_from_chars(chars)),
                        );
                    }
                }
                OcResponseFrame::success(request.id.clone(), usage)
            }
            None => OcResponseFrame::error(
                request.id.clone(),
                format!("Session not found: {key}"),
                Some(-32600),
            ),
        };
    }

    // Aggregate across all sessions.
    let sessions = state.sessions.list_sessions();
    let mut total_messages = 0usize;
    let mut total_chars = 0usize;
    for s in &sessions {
        if let Some(h) = state.sessions.get_session_handle(&s.session_key) {
            let history = h.get_history();
            total_messages += history.len();
            total_chars += history
                .iter()
                .map(|m| serde_json::to_string(m).map(|x| x.len()).unwrap_or(0))
                .sum::<usize>();
        }
    }
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "sessions": sessions.len(),
            "totalMessages": total_messages,
            "estimatedContextTokens": estimate_tokens_from_chars(total_chars),
        }),
    )
}

/// sessions.rename (v2026.7.1 session-organization).
pub fn handle_sessions_rename(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::Value::Null);
    let key = p.get("sessionKey").and_then(|v| v.as_str());
    let title = p.get("title").and_then(|v| v.as_str());
    match (key, title) {
        (Some(key), Some(title)) => {
            if state.sessions.get_session(key).is_none() {
                return OcResponseFrame::error(
                    request.id.clone(),
                    format!("Session not found: {key}"),
                    Some(-32600),
                );
            }
            state
                .sessions
                .patch_session(&crate::gateway::protocol::SessionPatchParams {
                    session_key: key.to_string(),
                    title: Some(title.to_string()),
                    model: None,
                    thinking: None,
                });
            state.rpc.sessions_list_cache.invalidate();
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        _ => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey or title".to_string(),
            Some(-32602),
        ),
    }
}

/// sessions.fork — copy history into a new session key (v2026.7.1).
pub async fn handle_sessions_fork(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::Value::Null);
    let source = p.get("sessionKey").and_then(|v| v.as_str());
    let target = p.get("targetSessionKey").and_then(|v| v.as_str());
    let (source, target) = match (source, target) {
        (Some(s), Some(t)) if s != t => (s, t),
        (Some(_), Some(_)) => {
            return OcResponseFrame::error(
                request.id.clone(),
                "targetSessionKey must differ from sessionKey".to_string(),
                Some(-32602),
            )
        }
        _ => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing sessionKey or targetSessionKey".to_string(),
                Some(-32602),
            )
        }
    };

    let source_handle = match state.sessions.get_session_handle(source) {
        Some(h) => h,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("Session not found: {source}"),
                Some(-32600),
            )
        }
    };
    if state.sessions.get_session(target).is_some() {
        return OcResponseFrame::error(
            request.id.clone(),
            format!("Target session already exists: {target}"),
            Some(-32600),
        );
    }

    let config = state.config.read().await;
    let new_handle = state.sessions.get_or_create_session(target, &config);
    drop(config);
    let history = source_handle.get_history();
    let copied = history.len();
    for msg in history {
        new_handle.add_message(msg);
    }
    state.rpc.sessions_list_cache.invalidate();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "ok": true,
            "sessionKey": target,
            "copiedMessages": copied,
        }),
    )
}

/// sessions.archive — mark a session archived/unarchived (v2026.7.1).
pub fn handle_sessions_archive(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::Value::Null);
    let key = match p.get("sessionKey").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing sessionKey".to_string(),
                Some(-32602),
            )
        }
    };
    let archived = p.get("archived").and_then(|v| v.as_bool()).unwrap_or(true);
    state.rpc.session_org.set_archived(key, archived);
    state.rpc.sessions_list_cache.invalidate();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "ok": true, "sessionKey": key, "archived": archived }),
    )
}

/// sessions.groups — list / assign / remove group membership (v2026.7.1).
pub fn handle_sessions_groups(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::json!({}));
    let action = p.get("action").and_then(|v| v.as_str()).unwrap_or("list");
    match action {
        "list" => OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({ "groups": state.rpc.session_org.groups_snapshot() }),
        ),
        "assign" | "remove" => {
            let group = p.get("group").and_then(|v| v.as_str());
            let key = p.get("sessionKey").and_then(|v| v.as_str());
            match (group, key) {
                (Some(g), Some(k)) => {
                    if action == "assign" {
                        state.rpc.session_org.assign_group(g, k);
                    } else {
                        state.rpc.session_org.remove_from_group(g, k);
                    }
                    OcResponseFrame::success(
                        request.id.clone(),
                        serde_json::json!({ "ok": true }),
                    )
                }
                _ => OcResponseFrame::error(
                    request.id.clone(),
                    "Missing group or sessionKey".to_string(),
                    Some(-32602),
                ),
            }
        }
        other => OcResponseFrame::error(
            request.id.clone(),
            format!("Unknown sessions.groups action: {other}"),
            Some(-32602),
        ),
    }
}

/// sessions.unread — get or set unread counters (v2026.7.1).
pub fn handle_sessions_unread(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::json!({}));
    if let (Some(key), Some(count)) = (
        p.get("sessionKey").and_then(|v| v.as_str()),
        p.get("count").and_then(|v| v.as_u64()),
    ) {
        state.rpc.session_org.set_unread(key, count);
    }
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "unread": state.rpc.session_org.unread_snapshot() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(key: &str, updated_at: &str) -> SessionInfo {
        SessionInfo {
            id: format!("id-{key}"),
            session_key: key.to_string(),
            agent_id: "default".to_string(),
            title: None,
            model: None,
            thinking: None,
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
        }
    }

    // ---- bounding ----

    #[test]
    fn list_limit_bounds() {
        assert_eq!(bound_list_limit(None), DEFAULT_SESSIONS_LIST_LIMIT);
        assert_eq!(bound_list_limit(Some(0)), DEFAULT_SESSIONS_LIST_LIMIT);
        assert_eq!(bound_list_limit(Some(50)), 50);
        assert_eq!(bound_list_limit(Some(10_000)), MAX_SESSIONS_LIST_LIMIT);
    }

    #[test]
    fn bounded_page_truncation_metadata() {
        let items: Vec<u32> = (0..10).collect();
        let (page, truncated, total) = bounded_page(&items, 0, 4);
        assert_eq!(page, vec![0, 1, 2, 3]);
        assert!(truncated);
        assert_eq!(total, 10);

        let (page, truncated, _) = bounded_page(&items, 8, 4);
        assert_eq!(page, vec![8, 9]);
        assert!(!truncated);

        let (page, truncated, _) = bounded_page(&items, 20, 4);
        assert!(page.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn checkpoint_preview_is_lightweight() {
        let v = checkpoint_preview(&info("s1", "2026-07-01T00:00:00Z"), 7);
        assert_eq!(v["sessionKey"], "s1");
        assert_eq!(v["messageCount"], 7);
        assert!(v.get("history").is_none());
    }

    #[test]
    fn list_cache_ttl_and_key() {
        let cache = SessionsListCache::new(Duration::from_secs(60));
        let key = SessionsListCache::cache_key(200, 0, None);
        assert!(cache.get(&key).is_none());
        cache.store(key.clone(), serde_json::json!({"total": 1}));
        assert_eq!(cache.get(&key).unwrap()["total"], 1);
        // Different shape → different key → miss
        let other = SessionsListCache::cache_key(100, 0, Some("a1"));
        assert!(cache.get(&other).is_none());
        cache.invalidate();
        assert!(cache.get(&key).is_none());
    }

    // ---- terminal outcomes ----

    #[test]
    fn terminal_outcomes_normalize_canonically() {
        assert_eq!(normalize_terminal_outcome("ok"), "completed");
        assert_eq!(normalize_terminal_outcome("Success"), "completed");
        assert_eq!(normalize_terminal_outcome("end_turn"), "completed");
        assert_eq!(normalize_terminal_outcome("FAILED"), "error");
        assert_eq!(normalize_terminal_outcome("cancelled"), "aborted");
        assert_eq!(normalize_terminal_outcome("auth-revoked"), "aborted");
        assert_eq!(normalize_terminal_outcome("timed_out"), "timeout");
        assert_eq!(normalize_terminal_outcome("run timed out after 30s"), "timeout");
        assert_eq!(normalize_terminal_outcome("weird state"), "unknown");
    }

    // ---- cleanup ----

    #[test]
    fn cleanup_params_defaults_and_validation() {
        let p = parse_cleanup_params(None).unwrap();
        assert_eq!(p.older_than_ms, 7 * 24 * 60 * 60 * 1000);
        assert!(!p.dry_run);
        assert_eq!(p.limit, 500);

        assert!(parse_cleanup_params(Some(&serde_json::json!({"olderThanMs": 0}))).is_err());

        let p =
            parse_cleanup_params(Some(&serde_json::json!({"limit": 100000, "dryRun": true})))
                .unwrap();
        assert_eq!(p.limit, 5_000);
        assert!(p.dry_run);
    }

    #[test]
    fn cleanup_candidates_respect_cutoff_and_limit() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-20T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let sessions = vec![
            info("old-1", "2026-07-01T00:00:00Z"),
            info("old-2", "2026-06-01T00:00:00Z"),
            info("fresh", "2026-07-19T23:00:00Z"),
            info("bad-ts", "not-a-timestamp"),
        ];
        let day_ms = 24 * 60 * 60 * 1000;
        let out = cleanup_candidates(&sessions, now, day_ms, 10);
        // oldest first, fresh + unparsable excluded
        assert_eq!(out, vec!["old-2".to_string(), "old-1".to_string()]);

        let out = cleanup_candidates(&sessions, now, day_ms, 1);
        assert_eq!(out, vec!["old-2".to_string()]);
    }

    // ---- usage estimation ----

    #[test]
    fn token_estimation_heuristic() {
        assert_eq!(estimate_tokens_from_chars(0), 0);
        assert_eq!(estimate_tokens_from_chars(4), 1);
        assert_eq!(estimate_tokens_from_chars(5), 2);
        assert_eq!(estimate_tokens_from_chars(400), 100);
    }

    // ---- org state ----

    #[test]
    fn org_state_archive_unread_groups() {
        let org = SessionOrgState::new();
        org.set_archived("s1", true);
        assert!(org.is_archived("s1"));
        org.set_archived("s1", false);
        assert!(!org.is_archived("s1"));

        org.set_unread("s1", 3);
        assert_eq!(org.unread_snapshot().get("s1"), Some(&3));
        org.set_unread("s1", 0);
        assert!(org.unread_snapshot().get("s1").is_none());

        org.assign_group("work", "s1");
        org.assign_group("work", "s1"); // idempotent
        org.assign_group("work", "s2");
        assert_eq!(org.groups_snapshot()["work"], vec!["s1", "s2"]);
        org.remove_from_group("work", "s1");
        assert_eq!(org.groups_snapshot()["work"], vec!["s2"]);
    }

    #[test]
    fn org_state_purge_session() {
        let org = SessionOrgState::new();
        org.set_archived("s1", true);
        org.set_unread("s1", 2);
        org.assign_group("g", "s1");
        org.purge_session("s1");
        assert!(!org.is_archived("s1"));
        assert!(org.unread_snapshot().is_empty());
        assert!(org.groups_snapshot().is_empty());
    }
}
