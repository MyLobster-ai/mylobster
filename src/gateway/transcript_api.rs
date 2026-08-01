//! Gateway-side transcript read contracts (v2026.5.2 parity shim).
//!
//! HANDOFF NOTE (sessions cluster): the durable transcript store
//! (`src/sessions/transcript.rs`) is owned by the sessions cluster. This shim
//! defines the call contracts the gateway consumes:
//!
//! - `read_transcript_bounded(store, session_key, window)` — bounded async
//!   transcript read (v2026.5.2: "Bounded async transcript reads").
//! - `stream_transcript_bounded(store, session_key, window, chunk_size)` —
//!   streamed bounded reads for session detail/history/artifacts/compaction
//!   (v2026.5.2).
//! - `bound_display_window(requested, max)` — chat-history display-window
//!   bounding (v2026.5.2: "Bound chat-history transcript reads to requested
//!   display window").
//!
//! The current implementation delegates to the in-memory `SessionStore`.
//! When `src/sessions/transcript.rs` lands, the sessions cluster should keep
//! these signatures and re-point the internals at the durable store.

use crate::providers::ProviderMessage;
use crate::sessions::SessionStore;
use tokio::sync::mpsc;

/// Default display window (entries) for chat-history reads.
pub const DEFAULT_DISPLAY_WINDOW: usize = 200;

/// Hard cap on any transcript display window.
pub const MAX_DISPLAY_WINDOW: usize = 1_000;

/// A bounded transcript page.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub entries: Vec<ProviderMessage>,
    /// Total entries in the transcript (before windowing).
    pub total: usize,
    /// True when entries were dropped to honor the window.
    pub truncated: bool,
}

/// Requested read window over a transcript. `offset` counts from the *end*
/// (most recent entries first come last in `entries`), matching how chat
/// history displays a tail window.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptWindow {
    pub offset: usize,
    pub limit: usize,
}

impl Default for TranscriptWindow {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: DEFAULT_DISPLAY_WINDOW,
        }
    }
}

/// Clamp a requested display window to the bounded range.
pub fn bound_display_window(requested: Option<u64>, max: usize) -> usize {
    match requested {
        None | Some(0) => DEFAULT_DISPLAY_WINDOW.min(max),
        Some(n) => (n as usize).min(max),
    }
}

/// Compute the tail slice bounds for a window over `total` entries.
/// Returns `(start, end)` indices into the full transcript.
pub fn tail_window_bounds(total: usize, window: TranscriptWindow) -> (usize, usize) {
    let end = total.saturating_sub(window.offset);
    let start = end.saturating_sub(window.limit);
    (start, end)
}

/// Bounded async transcript read (v2026.5.2 contract).
pub async fn read_transcript_bounded(
    store: &SessionStore,
    session_key: &str,
    window: TranscriptWindow,
) -> TranscriptPage {
    let history = store
        .get_session_handle(session_key)
        .map(|h| h.get_history())
        .unwrap_or_default();
    let total = history.len();
    let (start, end) = tail_window_bounds(total, window);
    TranscriptPage {
        entries: history[start..end].to_vec(),
        total,
        truncated: start > 0 || end < total,
    }
}

/// Streamed bounded transcript read (v2026.5.2 contract). Entries are sent
/// through the returned channel in `chunk_size` batches so large transcripts
/// never materialize a single oversized frame.
pub fn stream_transcript_bounded(
    store: &SessionStore,
    session_key: &str,
    window: TranscriptWindow,
    chunk_size: usize,
) -> mpsc::Receiver<Vec<ProviderMessage>> {
    let (tx, rx) = mpsc::channel(4);
    let history = store
        .get_session_handle(session_key)
        .map(|h| h.get_history())
        .unwrap_or_default();
    let (start, end) = tail_window_bounds(history.len(), window);
    let entries: Vec<ProviderMessage> = history[start..end].to_vec();
    let chunk = chunk_size.max(1);
    tokio::spawn(async move {
        for batch in entries.chunks(chunk) {
            if tx.send(batch.to_vec()).await.is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn store_with_history(key: &str, n: usize) -> SessionStore {
        let config = Config::default();
        let store = SessionStore::new(&config);
        let handle = store.get_or_create_session(key, &config);
        for i in 0..n {
            handle.add_message(ProviderMessage {
                role: "user".to_string(),
                content: serde_json::json!(format!("msg-{i}")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }
        store
    }

    #[test]
    fn display_window_bounds() {
        assert_eq!(bound_display_window(None, MAX_DISPLAY_WINDOW), 200);
        assert_eq!(bound_display_window(Some(0), MAX_DISPLAY_WINDOW), 200);
        assert_eq!(bound_display_window(Some(50), MAX_DISPLAY_WINDOW), 50);
        assert_eq!(bound_display_window(Some(99_999), MAX_DISPLAY_WINDOW), 1_000);
        // A smaller max wins even over the default
        assert_eq!(bound_display_window(None, 100), 100);
    }

    #[test]
    fn tail_window_math() {
        let w = TranscriptWindow { offset: 0, limit: 3 };
        assert_eq!(tail_window_bounds(10, w), (7, 10));
        let w = TranscriptWindow { offset: 3, limit: 3 };
        assert_eq!(tail_window_bounds(10, w), (4, 7));
        // Window larger than transcript
        let w = TranscriptWindow { offset: 0, limit: 50 };
        assert_eq!(tail_window_bounds(10, w), (0, 10));
        // Offset past the start
        let w = TranscriptWindow { offset: 50, limit: 5 };
        assert_eq!(tail_window_bounds(10, w), (0, 0));
    }

    #[tokio::test]
    async fn bounded_read_returns_tail_with_truncation_metadata() {
        let store = store_with_history("s1", 10);
        let page = read_transcript_bounded(
            &store,
            "s1",
            TranscriptWindow { offset: 0, limit: 4 },
        )
        .await;
        assert_eq!(page.entries.len(), 4);
        assert_eq!(page.total, 10);
        assert!(page.truncated);
        assert_eq!(page.entries[3].content, serde_json::json!("msg-9"));

        let all = read_transcript_bounded(
            &store,
            "s1",
            TranscriptWindow { offset: 0, limit: 100 },
        )
        .await;
        assert_eq!(all.entries.len(), 10);
        assert!(!all.truncated);
    }

    #[tokio::test]
    async fn bounded_read_missing_session_is_empty() {
        let store = SessionStore::new(&Config::default());
        let page = read_transcript_bounded(&store, "nope", TranscriptWindow::default()).await;
        assert_eq!(page.total, 0);
        assert!(page.entries.is_empty());
        assert!(!page.truncated);
    }

    #[tokio::test]
    async fn streamed_read_chunks_batches() {
        let store = store_with_history("s1", 10);
        let mut rx = stream_transcript_bounded(
            &store,
            "s1",
            TranscriptWindow { offset: 0, limit: 7 },
            3,
        );
        let mut batches = Vec::new();
        while let Some(batch) = rx.recv().await {
            batches.push(batch.len());
        }
        assert_eq!(batches, vec![3, 3, 1]);
    }
}
