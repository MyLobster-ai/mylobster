//! System / terminal / workspace / talk-session RPCs (v2026.7.1 parity).
//!
//! - `system.info` — host/OS/runtime/CPU/mem/disk snapshot.
//! - `terminal.list` / `terminal.text` / `terminal.detach` /
//!   `terminal.reattach` — detachable terminal session registry.
//! - `agents.workspace.list` / `agents.workspace.get` — read-only workspace
//!   file access (bounded, root-contained).
//! - `talk.session.start` / `talk.session.stop` / `talk.session.status` —
//!   unified Talk session controller surface.
//! - `tts.speak` — operator-scoped inline audio synthesis.

use crate::gateway::protocol::{OcResponseFrame, RequestFrame};
use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ============================================================================
// system.info
// ============================================================================

/// Build the `system.info` payload. Fields not derivable without extra
/// dependencies are reported as null rather than fabricated.
pub fn system_info_payload(version: &str, uptime_secs: u64) -> serde_json::Value {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .ok();
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok());
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "hostname": hostname,
        "cpus": cpus,
        "runtime": "rust",
        "runtimeVersion": option_env!("CARGO_PKG_RUST_VERSION"),
        "gatewayVersion": version,
        "uptimeSeconds": uptime_secs,
        "pid": std::process::id(),
        // Memory / disk metrics require a platform probe dependency; reported
        // as null until one is available.
        "memoryBytes": serde_json::Value::Null,
        "diskFreeBytes": serde_json::Value::Null,
    })
}

// ============================================================================
// Terminal session registry (terminal.list/text/detach/reattach)
// ============================================================================

/// A registered terminal session. Output is a bounded rolling text buffer.
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub id: String,
    pub title: String,
    pub attached: bool,
    pub text: String,
    pub updated_at_ms: u64,
}

/// Maximum retained text per terminal session (256 KiB).
pub const TERMINAL_TEXT_CAP: usize = 256 * 1024;

#[derive(Default)]
pub struct TerminalRegistry {
    sessions: parking_lot::RwLock<HashMap<String, TerminalSession>>,
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, id: &str, title: &str) {
        let mut sessions = self.sessions.write();
        let now = now_ms();
        sessions
            .entry(id.to_string())
            .and_modify(|s| {
                s.title = title.to_string();
                s.updated_at_ms = now;
            })
            .or_insert(TerminalSession {
                id: id.to_string(),
                title: title.to_string(),
                attached: true,
                text: String::new(),
                updated_at_ms: now,
            });
    }

    /// Append output text, enforcing the rolling cap (keeps the tail).
    pub fn append_text(&self, id: &str, chunk: &str) -> bool {
        let mut sessions = self.sessions.write();
        match sessions.get_mut(id) {
            Some(s) => {
                s.text.push_str(chunk);
                if s.text.len() > TERMINAL_TEXT_CAP {
                    let excess = s.text.len() - TERMINAL_TEXT_CAP;
                    // Trim on a char boundary at/after `excess`.
                    let cut = (excess..s.text.len())
                        .find(|i| s.text.is_char_boundary(*i))
                        .unwrap_or(s.text.len());
                    s.text.drain(..cut);
                }
                s.updated_at_ms = now_ms();
                true
            }
            None => false,
        }
    }

    pub fn set_attached(&self, id: &str, attached: bool) -> bool {
        let mut sessions = self.sessions.write();
        match sessions.get_mut(id) {
            Some(s) => {
                s.attached = attached;
                s.updated_at_ms = now_ms();
                true
            }
            None => false,
        }
    }

    pub fn list(&self) -> Vec<serde_json::Value> {
        let mut rows: Vec<serde_json::Value> = self
            .sessions
            .read()
            .values()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "title": s.title,
                    "attached": s.attached,
                    "updatedAtMs": s.updated_at_ms,
                    "textBytes": s.text.len(),
                })
            })
            .collect();
        rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        rows
    }

    pub fn text(&self, id: &str, max_chars: usize) -> Option<String> {
        self.sessions.read().get(id).map(|s| {
            let count = s.text.chars().count();
            if count <= max_chars {
                s.text.clone()
            } else {
                s.text
                    .chars()
                    .skip(count - max_chars)
                    .collect()
            }
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ============================================================================
// Talk session controller (talk.session.*)
// ============================================================================

#[derive(Default)]
pub struct TalkSessionController {
    sessions: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
}

impl TalkSessionController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&self, id: &str, mode: &str) -> serde_json::Value {
        let session = serde_json::json!({
            "id": id,
            "mode": mode,
            "state": "active",
            "startedAtMs": now_ms(),
        });
        self.sessions
            .write()
            .insert(id.to_string(), session.clone());
        session
    }

    pub fn stop(&self, id: &str) -> bool {
        self.sessions.write().remove(id).is_some()
    }

    pub fn status(&self) -> Vec<serde_json::Value> {
        self.sessions.read().values().cloned().collect()
    }
}

// ============================================================================
// agents.workspace (read-only)
// ============================================================================

/// Maximum bytes returned by `agents.workspace.get`.
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum entries returned by `agents.workspace.list`.
pub const MAX_WORKSPACE_LIST_ENTRIES: usize = 500;

pub fn workspace_root() -> PathBuf {
    if let Ok(root) = std::env::var("MYLOBSTER_WORKSPACE_DIR") {
        if !root.trim().is_empty() {
            return PathBuf::from(root);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mylobster")
        .join("workspace")
}

/// Resolve a relative workspace path with containment enforcement.
pub fn resolve_workspace_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err("path must be non-empty".to_string());
    }
    let rel_path = Path::new(trimmed);
    if rel_path.is_absolute() {
        return Err("path must be relative to the workspace root".to_string());
    }
    for comp in rel_path.components() {
        if !matches!(comp, std::path::Component::Normal(_)) {
            return Err("path must not contain '..' or special components".to_string());
        }
    }
    let joined = root.join(rel_path);
    let canonical = joined
        .canonicalize()
        .map_err(|_| format!("file not found: {trimmed}"))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("workspace root unavailable: {e}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err("path escapes workspace root".to_string());
    }
    Ok(canonical)
}

pub fn handle_workspace_list(request: &RequestFrame) -> OcResponseFrame {
    let root = workspace_root();
    if !root.exists() {
        return OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({ "files": [], "root": root.display().to_string() }),
        );
    }
    let mut files = Vec::new();
    let mut stack = vec![root.clone()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > 256 || files.len() >= MAX_WORKSPACE_LIST_ENTRIES {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            if files.len() >= MAX_WORKSPACE_LIST_ENTRIES {
                break;
            }
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
            } else if let (Ok(rel), Ok(meta)) = (path.strip_prefix(&root), e.metadata()) {
                files.push(serde_json::json!({
                    "path": rel.to_string_lossy(),
                    "sizeBytes": meta.len(),
                }));
            }
        }
    }
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "files": files, "root": root.display().to_string() }),
    )
}

pub fn handle_workspace_get(request: &RequestFrame) -> OcResponseFrame {
    let rel = match request
        .params
        .as_ref()
        .and_then(|p| p.get("path"))
        .and_then(|v| v.as_str())
    {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing 'path' param".to_string(),
                Some(-32602),
            )
        }
    };
    let root = workspace_root();
    let path = match resolve_workspace_path(&root, rel) {
        Ok(p) => p,
        Err(e) => return OcResponseFrame::error(request.id.clone(), e, Some(-32602)),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("not a file: {rel}"),
                Some(-32600),
            )
        }
    };
    if meta.len() > MAX_WORKSPACE_FILE_BYTES {
        return OcResponseFrame::error(
            request.id.clone(),
            format!(
                "file is {} bytes; exceeds read limit of {} bytes",
                meta.len(),
                MAX_WORKSPACE_FILE_BYTES
            ),
            Some(-32600),
        );
    }
    match std::fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(text) => OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({ "path": rel, "content": text }),
            ),
            Err(_) => OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({
                    "path": rel,
                    "contentBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }),
            ),
        },
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("failed to read: {e}"),
            Some(-32603),
        ),
    }
}

// ============================================================================
// tts.speak (operator-scoped inline audio)
// ============================================================================

pub async fn handle_tts_speak(request: &RequestFrame) -> OcResponseFrame {
    let p = request.params.clone().unwrap_or(serde_json::Value::Null);
    let text = match p.get("text").and_then(|v| v.as_str()) {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing 'text' param".to_string(),
                Some(-32602),
            )
        }
    };
    if text.chars().count() > 4_000 {
        return OcResponseFrame::error(
            request.id.clone(),
            "text exceeds 4000-char tts.speak limit".to_string(),
            Some(-32602),
        );
    }
    let voice = p
        .get("voice")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    match crate::tts::TtsManager::from_env().await {
        Ok(manager) => match manager.generate(&text, &voice).await {
            Ok(audio) => OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({
                    "provider": manager.provider_name(),
                    "voice": voice,
                    "audioBase64": base64::engine::general_purpose::STANDARD.encode(&audio),
                    "sizeBytes": audio.len(),
                }),
            ),
            Err(e) => OcResponseFrame::error(
                request.id.clone(),
                format!("tts synthesis failed: {e}"),
                Some(-32603),
            ),
        },
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("no TTS provider available: {e}"),
            Some(-32603),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- system.info ----

    #[test]
    fn system_info_shape() {
        let v = system_info_payload("1.2.3", 42);
        assert_eq!(v["gatewayVersion"], "1.2.3");
        assert_eq!(v["uptimeSeconds"], 42);
        assert_eq!(v["runtime"], "rust");
        assert_eq!(v["os"], std::env::consts::OS);
        assert!(v["pid"].as_u64().unwrap() > 0);
        // Unprobed metrics are null, not fabricated.
        assert!(v["memoryBytes"].is_null());
        assert!(v["diskFreeBytes"].is_null());
    }

    // ---- terminals ----

    #[test]
    fn terminal_lifecycle() {
        let reg = TerminalRegistry::new();
        reg.upsert("t1", "build");
        assert!(reg.append_text("t1", "hello "));
        assert!(reg.append_text("t1", "world"));
        assert_eq!(reg.text("t1", 100).unwrap(), "hello world");
        // Bounded tail read
        assert_eq!(reg.text("t1", 5).unwrap(), "world");
        // Detach / reattach
        assert!(reg.set_attached("t1", false));
        assert_eq!(reg.list()[0]["attached"], false);
        assert!(reg.set_attached("t1", true));
        assert_eq!(reg.list()[0]["attached"], true);
        // Unknown ids
        assert!(!reg.append_text("nope", "x"));
        assert!(!reg.set_attached("nope", false));
        assert!(reg.text("nope", 10).is_none());
    }

    #[test]
    fn terminal_text_rolling_cap() {
        let reg = TerminalRegistry::new();
        reg.upsert("t1", "big");
        let chunk = "x".repeat(TERMINAL_TEXT_CAP / 2 + 100);
        reg.append_text("t1", &chunk);
        reg.append_text("t1", &chunk);
        reg.append_text("t1", "TAIL");
        let text = reg.text("t1", usize::MAX).unwrap();
        assert!(text.len() <= TERMINAL_TEXT_CAP + 4);
        assert!(text.ends_with("TAIL"));
    }

    // ---- talk sessions ----

    #[test]
    fn talk_session_controller_lifecycle() {
        let ctl = TalkSessionController::new();
        let s = ctl.start("call-1", "realtime");
        assert_eq!(s["state"], "active");
        assert_eq!(ctl.status().len(), 1);
        assert!(ctl.stop("call-1"));
        assert!(!ctl.stop("call-1"));
        assert!(ctl.status().is_empty());
    }

    // ---- workspace ----

    #[test]
    fn workspace_path_containment() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.md"), "hi").unwrap();
        assert!(resolve_workspace_path(dir.path(), "notes.md").is_ok());
        assert!(resolve_workspace_path(dir.path(), "../escape").is_err());
        assert!(resolve_workspace_path(dir.path(), "/abs").is_err());
        assert!(resolve_workspace_path(dir.path(), "").is_err());
        assert!(resolve_workspace_path(dir.path(), "missing.txt").is_err());
    }
}
