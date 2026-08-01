//! Artifact RPCs: `artifacts.list` / `artifacts.get` / `artifacts.download`
//! (v2026.5.2 parity).
//!
//! Artifacts are files produced by agent runs (canvas exports, generated
//! media, tool outputs) stored under a per-gateway artifacts root. All three
//! RPCs enforce root containment (no traversal, no symlink escape) and
//! bounded reads.

use crate::gateway::protocol::{OcResponseFrame, RequestFrame};
use base64::Engine;
use std::path::{Path, PathBuf};

/// Maximum artifact bytes returned inline by `artifacts.download` (8 MiB).
pub const MAX_ARTIFACT_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum entries returned by `artifacts.list`.
pub const MAX_ARTIFACT_LIST_ENTRIES: usize = 500;

/// Resolve the artifacts root directory.
pub fn artifacts_root() -> PathBuf {
    if let Ok(root) = std::env::var("MYLOBSTER_ARTIFACTS_DIR") {
        if !root.trim().is_empty() {
            return PathBuf::from(root);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mylobster")
        .join("artifacts")
}

/// Validate a client-supplied artifact id (relative path) and resolve it
/// within `root`. Rejects absolute paths, traversal, empty ids, and (after
/// canonicalization) any path escaping the root.
pub fn resolve_artifact_path(root: &Path, id: &str) -> Result<PathBuf, String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err("artifact id must be non-empty".to_string());
    }
    let rel = Path::new(trimmed);
    if rel.is_absolute() {
        return Err("artifact id must be relative".to_string());
    }
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            _ => return Err("artifact id must not contain '..' or special components".to_string()),
        }
    }
    let joined = root.join(rel);
    // Canonicalize to catch symlink escapes; the file must exist for get/download.
    match joined.canonicalize() {
        Ok(canonical) => {
            let canonical_root = root
                .canonicalize()
                .map_err(|e| format!("artifacts root unavailable: {e}"))?;
            if !canonical.starts_with(&canonical_root) {
                return Err("artifact path escapes artifacts root".to_string());
            }
            Ok(canonical)
        }
        Err(_) => Err(format!("artifact not found: {trimmed}")),
    }
}

fn artifact_entry(root: &Path, path: &Path) -> Option<serde_json::Value> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let id = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Some(serde_json::json!({
        "id": id,
        "sizeBytes": meta.len(),
        "modifiedAtMs": modified_ms,
    }))
}

/// List artifacts (bounded, newest-first by modified time).
pub fn list_artifacts(root: &Path, limit: usize) -> Vec<serde_json::Value> {
    let mut entries: Vec<(u64, serde_json::Value)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited_dirs = 0usize;
    while let Some(dir) = stack.pop() {
        visited_dirs += 1;
        if visited_dirs > 512 {
            break; // bounded traversal
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue; // never follow symlinks while listing
            }
            if ft.is_dir() {
                stack.push(path);
            } else if let Some(entry) = artifact_entry(root, &path) {
                let ts = entry["modifiedAtMs"].as_u64().unwrap_or(0);
                entries.push((ts, entry));
            }
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .take(limit.min(MAX_ARTIFACT_LIST_ENTRIES))
        .map(|(_, e)| e)
        .collect()
}

// ============================================================================
// Handlers
// ============================================================================

pub fn handle_artifacts_list(request: &RequestFrame) -> OcResponseFrame {
    let limit = request
        .params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(100);
    let root = artifacts_root();
    if !root.exists() {
        return OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({ "artifacts": [], "root": root.display().to_string() }),
        );
    }
    let artifacts = list_artifacts(&root, limit);
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "artifacts": artifacts, "root": root.display().to_string() }),
    )
}

fn artifact_id_param(request: &RequestFrame) -> Result<String, OcResponseFrame> {
    request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            OcResponseFrame::error(
                request.id.clone(),
                "Missing 'id' param".to_string(),
                Some(-32602),
            )
        })
}

pub fn handle_artifacts_get(request: &RequestFrame) -> OcResponseFrame {
    let id = match artifact_id_param(request) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let root = artifacts_root();
    match resolve_artifact_path(&root, &id) {
        Ok(path) => match artifact_entry(&root.canonicalize().unwrap_or(root.clone()), &path) {
            Some(entry) => OcResponseFrame::success(request.id.clone(), entry),
            None => OcResponseFrame::error(
                request.id.clone(),
                format!("artifact not found: {id}"),
                Some(-32600),
            ),
        },
        Err(e) => OcResponseFrame::error(request.id.clone(), e, Some(-32602)),
    }
}

pub fn handle_artifacts_download(request: &RequestFrame) -> OcResponseFrame {
    let id = match artifact_id_param(request) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let root = artifacts_root();
    let path = match resolve_artifact_path(&root, &id) {
        Ok(p) => p,
        Err(e) => return OcResponseFrame::error(request.id.clone(), e, Some(-32602)),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("cannot stat artifact: {e}"),
                Some(-32603),
            )
        }
    };
    if meta.len() > MAX_ARTIFACT_DOWNLOAD_BYTES {
        return OcResponseFrame::error(
            request.id.clone(),
            format!(
                "artifact is {} bytes; exceeds inline download limit of {} bytes",
                meta.len(),
                MAX_ARTIFACT_DOWNLOAD_BYTES
            ),
            Some(-32600),
        );
    }
    match std::fs::read(&path) {
        Ok(bytes) => OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({
                "id": id,
                "sizeBytes": bytes.len(),
                "contentBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            }),
        ),
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("failed to read artifact: {e}"),
            Some(-32603),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_rejects_traversal_and_absolute() {
        let dir = TempDir::new().unwrap();
        assert!(resolve_artifact_path(dir.path(), "../etc/passwd").is_err());
        assert!(resolve_artifact_path(dir.path(), "/etc/passwd").is_err());
        assert!(resolve_artifact_path(dir.path(), "a/../../b").is_err());
        assert!(resolve_artifact_path(dir.path(), "").is_err());
        assert!(resolve_artifact_path(dir.path(), "   ").is_err());
    }

    #[test]
    fn resolve_accepts_contained_file() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("run-1")).unwrap();
        fs::write(dir.path().join("run-1/out.txt"), "hello").unwrap();
        let p = resolve_artifact_path(dir.path(), "run-1/out.txt").unwrap();
        assert!(p.ends_with("run-1/out.txt"));
    }

    #[test]
    fn resolve_missing_file_errors() {
        let dir = TempDir::new().unwrap();
        let err = resolve_artifact_path(dir.path(), "nope.bin").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let err = resolve_artifact_path(dir.path(), "link.txt").unwrap_err();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn list_is_bounded_and_sorted() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            fs::write(dir.path().join(format!("a{i}.txt")), "x").unwrap();
        }
        let all = list_artifacts(dir.path(), 100);
        assert_eq!(all.len(), 5);
        let capped = list_artifacts(dir.path(), 2);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn list_skips_directories_and_recurses() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/deep.txt"), "x").unwrap();
        let all = list_artifacts(dir.path(), 100);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["id"], "nested/deep.txt");
    }
}
