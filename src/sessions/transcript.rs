//! File-backed session transcripts (JSONL) — v2026.5.2 / v2026.7.1 parity.
//!
//! - Bounded async transcript reads; serialized parent-linked writes for hot
//!   transcript paths (v2026.5.2).
//! - Streamed bounded transcript reads for session detail / history /
//!   artifacts / compaction (v2026.5.2) — callers pass a read purpose-sized
//!   [`TranscriptReadOptions`] and get back a bounded, truncation-flagged
//!   result instead of a whole-file load.
//! - Maintenance rewrites never reopen large Pi transcript files through a
//!   synchronous whole-file read: rewrites stream line-by-line into a temp
//!   file and atomically replace (mode-preserving), and
//!   [`requires_streaming_rewrite`] lets maintenance defer large files to the
//!   async writer (v2026.5.2).

use crate::sessions::sandbox;

use dashmap::DashMap;
use serde_json::Value;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};

/// Default byte budget for a bounded transcript read.
pub const DEFAULT_MAX_READ_BYTES: u64 = 5 * 1024 * 1024;
/// Default entry budget for a bounded transcript read.
pub const DEFAULT_MAX_READ_ENTRIES: usize = 1_000;
/// Chunk size used by the reverse (tail) reader.
const TAIL_CHUNK_BYTES: u64 = 64 * 1024;
/// Transcripts at or above this size must use the streaming rewrite path
/// (never a synchronous whole-file reopen) for maintenance rewrites.
pub const LARGE_TRANSCRIPT_STREAMING_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Bounds for a transcript read.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptReadOptions {
    pub max_bytes: u64,
    pub max_entries: usize,
}

impl Default for TranscriptReadOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_READ_BYTES,
            max_entries: DEFAULT_MAX_READ_ENTRIES,
        }
    }
}

impl TranscriptReadOptions {
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            max_entries,
            ..Self::default()
        }
    }
}

/// Result of a bounded transcript read.
#[derive(Debug, Clone, Default)]
pub struct BoundedRead {
    /// Parsed entries, in transcript order (oldest first).
    pub entries: Vec<Value>,
    /// True when the read stopped at a bound before covering the whole file.
    pub truncated: bool,
    /// Bytes consumed from disk for this read.
    pub bytes_read: u64,
    /// Lines that failed to parse as JSON (skipped, not fatal).
    pub malformed_lines: usize,
}

/// Streamed, bounded forward read (session detail / history / artifacts /
/// compaction). Reads at most `max_bytes` / `max_entries` from the start of
/// the transcript without ever buffering the whole file.
pub async fn read_transcript_head(
    path: &Path,
    opts: TranscriptReadOptions,
) -> std::io::Result<BoundedRead> {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BoundedRead::default()),
        Err(e) => return Err(e),
    };
    let file_len = file.metadata().await?.len();
    let mut reader = BufReader::new(file);
    let mut out = BoundedRead::default();
    let mut line = String::new();

    loop {
        if out.entries.len() >= opts.max_entries {
            out.truncated = out.bytes_read < file_len;
            break;
        }
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        out.bytes_read += n as u64;
        push_line(&mut out, &line);
        if out.bytes_read >= opts.max_bytes {
            out.truncated = out.bytes_read < file_len;
            break;
        }
    }
    Ok(out)
}

/// Bounded reverse (tail) read: the newest entries of the transcript, read
/// backwards in fixed-size chunks so large files are never fully loaded.
/// Entries are returned oldest-first within the tail window.
pub async fn read_transcript_tail(
    path: &Path,
    opts: TranscriptReadOptions,
) -> std::io::Result<BoundedRead> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BoundedRead::default()),
        Err(e) => return Err(e),
    };
    let file_len = file.metadata().await?.len();
    if file_len == 0 {
        return Ok(BoundedRead::default());
    }

    let mut buffer: Vec<u8> = Vec::new();
    let mut pos = file_len;
    let byte_budget = opts.max_bytes.min(file_len);

    while pos > 0 && (buffer.len() as u64) < byte_budget {
        let chunk = TAIL_CHUNK_BYTES.min(pos);
        pos -= chunk;
        file.seek(std::io::SeekFrom::Start(pos)).await?;
        let mut chunk_buf = vec![0u8; chunk as usize];
        file.read_exact(&mut chunk_buf).await?;
        chunk_buf.extend_from_slice(&buffer);
        buffer = chunk_buf;
    }

    let reached_start = pos == 0;
    let mut out = BoundedRead {
        bytes_read: buffer.len() as u64,
        ..Default::default()
    };

    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<&str> = text.lines().collect();
    if !reached_start && !lines.is_empty() {
        // First line of the window is (likely) a partial line — drop it.
        lines.remove(0);
        out.truncated = true;
    }
    // Keep only the newest `max_entries` parseable lines.
    let mut parsed: Vec<Value> = Vec::new();
    for line in lines.iter().rev() {
        if parsed.len() >= opts.max_entries {
            out.truncated = true;
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => parsed.push(v),
            Err(_) => out.malformed_lines += 1,
        }
    }
    parsed.reverse();
    out.entries = parsed;
    if !reached_start {
        out.truncated = true;
    }
    Ok(out)
}

fn push_line(out: &mut BoundedRead, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => out.entries.push(v),
        Err(_) => out.malformed_lines += 1,
    }
}

// ============================================================================
// Serialized writes
// ============================================================================

fn write_locks() -> &'static DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>> {
    static LOCKS: OnceLock<DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> = OnceLock::new();
    LOCKS.get_or_init(DashMap::new)
}

fn path_write_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    write_locks()
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn append_line_unlocked(path: &Path, entry: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            tokio::fs::create_dir_all(dir).await?;
        }
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, line.as_bytes()).await?;
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(())
}

/// Append one entry to a transcript, serialized per path.
pub async fn append_entry(path: &Path, entry: &Value) -> std::io::Result<()> {
    let lock = path_write_lock(path);
    let _guard = lock.lock().await;
    append_line_unlocked(path, entry).await
}

/// Append a child entry plus its parent-link entry, serialized through the
/// parent's write lock so concurrent children linking into the same hot
/// parent transcript are ordered (v2026.5.2 "serialized parent-linked
/// writes").
pub async fn append_parent_linked(
    parent_path: &Path,
    child_path: &Path,
    parent_entry: &Value,
    child_entry: &Value,
) -> std::io::Result<()> {
    let parent_lock = path_write_lock(parent_path);
    let _parent_guard = parent_lock.lock().await;
    if parent_path != child_path {
        let child_lock = path_write_lock(child_path);
        let _child_guard = child_lock.lock().await;
        append_line_unlocked(child_path, child_entry).await?;
    } else {
        append_line_unlocked(child_path, child_entry).await?;
    }
    append_line_unlocked(parent_path, parent_entry).await
}

// ============================================================================
// Maintenance rewrites (streaming, never whole-file synchronous)
// ============================================================================

/// Whether a transcript of `len` bytes must go through the streaming rewrite
/// path (and be deferred to the async writer) rather than any synchronous
/// whole-file manager path.
pub fn requires_streaming_rewrite(len: u64) -> bool {
    len >= LARGE_TRANSCRIPT_STREAMING_THRESHOLD_BYTES
}

/// Outcome of a maintenance rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewriteOutcome {
    pub kept: usize,
    pub dropped: usize,
    pub malformed_dropped: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Rewrite a transcript keeping only entries for which `keep` returns true.
///
/// Streams line-by-line into a temp file in the same directory and atomically
/// replaces the original (preserving its file mode). The source file is never
/// loaded wholesale, so large Pi transcripts don't get reopened synchronously
/// for maintenance rewrites.
pub async fn rewrite_transcript_streaming<F>(
    path: &Path,
    mut keep: F,
) -> std::io::Result<RewriteOutcome>
where
    F: FnMut(&Value) -> bool + Send,
{
    let lock = path_write_lock(path);
    let _guard = lock.lock().await;

    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RewriteOutcome::default())
        }
        Err(e) => return Err(e),
    };
    let bytes_before = file.metadata().await?.len();
    let mut reader = BufReader::new(file);

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;

    let mut outcome = RewriteOutcome {
        bytes_before,
        ..Default::default()
    };
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(entry) => {
                if keep(&entry) {
                    tmp.write_all(trimmed.as_bytes())?;
                    tmp.write_all(b"\n")?;
                    outcome.kept += 1;
                    outcome.bytes_after += trimmed.len() as u64 + 1;
                } else {
                    outcome.dropped += 1;
                }
            }
            Err(_) => {
                outcome.malformed_dropped += 1;
            }
        }
    }
    tmp.flush()?;
    sandbox::persist_preserving_mode(tmp, path)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(i: usize) -> Value {
        json!({ "seq": i, "role": if i % 2 == 0 { "user" } else { "assistant" }, "text": format!("message {i}") })
    }

    async fn write_synthetic_transcript(path: &Path, count: usize) {
        let mut body = String::new();
        for i in 0..count {
            body.push_str(&serde_json::to_string(&entry(i)).unwrap());
            body.push('\n');
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Bounded forward reads
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn head_read_respects_entry_cap_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 100).await;

        let read = read_transcript_head(&path, TranscriptReadOptions::with_max_entries(10))
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 10);
        assert!(read.truncated);
        assert_eq!(read.entries[0]["seq"], 0);
        assert_eq!(read.entries[9]["seq"], 9);
    }

    #[tokio::test]
    async fn head_read_respects_byte_cap_on_synthetic_large_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jsonl");
        // ~200KB transcript; cap the read at 8KB.
        write_synthetic_transcript(&path, 3_000).await;
        let opts = TranscriptReadOptions {
            max_bytes: 8 * 1024,
            max_entries: usize::MAX,
        };
        let read = read_transcript_head(&path, opts).await.unwrap();
        assert!(read.truncated);
        assert!(read.bytes_read <= 8 * 1024 + 256, "read {} bytes", read.bytes_read);
        assert!(!read.entries.is_empty());
        assert!(read.entries.len() < 3_000);
    }

    #[tokio::test]
    async fn head_read_of_whole_small_file_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 5).await;
        let read = read_transcript_head(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 5);
        assert!(!read.truncated);
    }

    #[tokio::test]
    async fn missing_transcript_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.jsonl");
        let head = read_transcript_head(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert!(head.entries.is_empty() && !head.truncated);
        let tail = read_transcript_tail(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert!(tail.entries.is_empty() && !tail.truncated);
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        tokio::fs::write(&path, "{\"a\":1}\nnot json\n{\"b\":2}\n")
            .await
            .unwrap();
        let read = read_transcript_head(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 2);
        assert_eq!(read.malformed_lines, 1);
    }

    // ------------------------------------------------------------------
    // Bounded tail reads
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tail_read_returns_newest_entries_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 50).await;

        let read = read_transcript_tail(&path, TranscriptReadOptions::with_max_entries(5))
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 5);
        assert!(read.truncated);
        let seqs: Vec<u64> = read
            .entries
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![45, 46, 47, 48, 49]);
    }

    #[tokio::test]
    async fn tail_read_bounds_bytes_on_synthetic_large_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jsonl");
        write_synthetic_transcript(&path, 5_000).await;
        let file_len = tokio::fs::metadata(&path).await.unwrap().len();

        let opts = TranscriptReadOptions {
            max_bytes: 16 * 1024,
            max_entries: usize::MAX,
        };
        let read = read_transcript_tail(&path, opts).await.unwrap();
        assert!(read.truncated);
        assert!(read.bytes_read < file_len);
        assert!(read.bytes_read <= 80 * 1024, "chunked window stays bounded");
        // Newest entry must be present.
        let last = read.entries.last().unwrap();
        assert_eq!(last["seq"].as_u64().unwrap(), 4_999);
    }

    #[tokio::test]
    async fn tail_read_of_whole_small_file_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 3).await;
        let read = read_transcript_tail(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 3);
        assert!(!read.truncated);
        assert_eq!(read.entries[0]["seq"], 0);
    }

    // ------------------------------------------------------------------
    // Serialized writes
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_appends_serialize_into_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.jsonl");
        let mut handles = Vec::new();
        for i in 0..32usize {
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                append_entry(&path, &entry(i)).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let read = read_transcript_head(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 32);
        assert_eq!(read.malformed_lines, 0);
    }

    #[tokio::test]
    async fn parent_linked_writes_keep_child_before_parent_link() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent.jsonl");
        let child = dir.path().join("child.jsonl");
        let mut handles = Vec::new();
        for i in 0..16usize {
            let parent = parent.clone();
            let child = child.clone();
            handles.push(tokio::spawn(async move {
                append_parent_linked(
                    &parent,
                    &child,
                    &json!({"link": i}),
                    &json!({"childEntry": i}),
                )
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let parent_read = read_transcript_head(&parent, TranscriptReadOptions::default())
            .await
            .unwrap();
        let child_read = read_transcript_head(&child, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(parent_read.entries.len(), 16);
        assert_eq!(child_read.entries.len(), 16);
        assert_eq!(parent_read.malformed_lines + child_read.malformed_lines, 0);
    }

    // ------------------------------------------------------------------
    // Streaming maintenance rewrites
    // ------------------------------------------------------------------

    #[test]
    fn large_transcripts_require_streaming_rewrite() {
        assert!(!requires_streaming_rewrite(1024));
        assert!(!requires_streaming_rewrite(
            LARGE_TRANSCRIPT_STREAMING_THRESHOLD_BYTES - 1
        ));
        assert!(requires_streaming_rewrite(
            LARGE_TRANSCRIPT_STREAMING_THRESHOLD_BYTES
        ));
    }

    #[tokio::test]
    async fn streaming_rewrite_filters_entries_and_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 20).await;

        let outcome = rewrite_transcript_streaming(&path, |e| {
            e["seq"].as_u64().unwrap() % 2 == 0
        })
        .await
        .unwrap();
        assert_eq!(outcome.kept, 10);
        assert_eq!(outcome.dropped, 10);

        let read = read_transcript_head(&path, TranscriptReadOptions::default())
            .await
            .unwrap();
        assert_eq!(read.entries.len(), 10);
        assert!(read.entries.iter().all(|e| e["seq"].as_u64().unwrap() % 2 == 0));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn streaming_rewrite_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_synthetic_transcript(&path, 4).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();

        rewrite_transcript_streaming(&path, |_| true).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o664);
    }

    #[tokio::test]
    async fn streaming_rewrite_of_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.jsonl");
        let outcome = rewrite_transcript_streaming(&path, |_| true).await.unwrap();
        assert_eq!(outcome, RewriteOutcome::default());
        assert!(!path.exists());
    }
}
