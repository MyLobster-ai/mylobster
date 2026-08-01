//! Durable Telegram update spool + getUpdates offset store.
//!
//! Port of the OpenClaw durable-ingress behavior (v2026.7.1
//! `telegram-ingress-spool.ts`, `update-offset-store.ts`, `bot-message.ts`
//! turn adoption): inbound updates are persisted to a local SQLite spool
//! before dispatch so they survive event-loop stalls and restarts; a spooled
//! update is only completed after the handling turn **adopts** it; updates
//! that keep failing are tombstoned (dead-letter) instead of poisoning the
//! lane; the getUpdates offset is persisted after dispatch and keyed by the
//! bot-token fingerprint so token rotation discards stale offsets.

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Attempts before a poison update is tombstoned (dead-letter).
pub const TELEGRAM_SPOOL_MAX_ATTEMPTS: u32 = 3;

/// Spooled update lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoolStatus {
    Queued,
    Adopted,
    Tombstoned,
}

impl SpoolStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Adopted => "adopted",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// A spooled Telegram update awaiting adoption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpooledUpdate {
    pub rowid: i64,
    pub update_id: i64,
    pub payload: String,
    pub attempts: u32,
}

/// SQLite-backed spool. `:memory:` paths are supported for tests; production
/// callers pass a path under the agent state directory.
pub struct TelegramUpdateSpool {
    conn: Mutex<Connection>,
}

impl TelegramUpdateSpool {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS telegram_spool (
                rowid INTEGER PRIMARY KEY AUTOINCREMENT,
                update_id INTEGER NOT NULL UNIQUE,
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'queued',
                enqueued_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_telegram_spool_status
                ON telegram_spool(status);
            CREATE TABLE IF NOT EXISTS telegram_offsets (
                token_fingerprint TEXT PRIMARY KEY,
                next_offset INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS telegram_reply_context (
                chat_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                text TEXT,
                media_paths TEXT,
                recorded_at INTEGER NOT NULL,
                PRIMARY KEY (chat_id, message_id)
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Enqueues an update; duplicate `update_id`s (re-polled after a crash
    /// before offset persistence) are ignored. Returns `true` when newly
    /// spooled.
    pub fn enqueue(&self, update_id: i64, payload: &str) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO telegram_spool
             (update_id, payload, attempts, status, enqueued_at, updated_at)
             VALUES (?1, ?2, 0, 'queued', ?3, ?3)",
            rusqlite::params![update_id, payload, now],
        )?;
        Ok(inserted > 0)
    }

    /// Returns up to `limit` queued updates in arrival order.
    pub fn next_batch(&self, limit: usize) -> anyhow::Result<Vec<SpooledUpdate>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rowid, update_id, payload, attempts FROM telegram_spool
             WHERE status = 'queued' ORDER BY update_id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(SpooledUpdate {
                rowid: row.get(0)?,
                update_id: row.get(1)?,
                payload: row.get(2)?,
                attempts: row.get::<_, i64>(3)? as u32,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Turn adoption: marks the spooled update complete. Only called after
    /// the handling turn has durably adopted the update.
    pub fn mark_adopted(&self, rowid: i64) -> anyhow::Result<()> {
        self.set_status(rowid, SpoolStatus::Adopted)
    }

    /// Records a failed handling attempt. Returns the new status — poison
    /// updates are tombstoned (dead-letter) after
    /// [`TELEGRAM_SPOOL_MAX_ATTEMPTS`] so they cannot wedge the lane.
    pub fn record_failed_attempt(&self, rowid: i64) -> anyhow::Result<SpoolStatus> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE telegram_spool SET attempts = attempts + 1, updated_at = ?2
             WHERE rowid = ?1",
            rusqlite::params![rowid, now],
        )?;
        let attempts: u32 = conn.query_row(
            "SELECT attempts FROM telegram_spool WHERE rowid = ?1",
            [rowid],
            |row| row.get::<_, i64>(0).map(|v| v as u32),
        )?;
        if attempts >= TELEGRAM_SPOOL_MAX_ATTEMPTS {
            conn.execute(
                "UPDATE telegram_spool SET status = 'tombstoned', updated_at = ?2
                 WHERE rowid = ?1",
                rusqlite::params![rowid, now],
            )?;
            Ok(SpoolStatus::Tombstoned)
        } else {
            Ok(SpoolStatus::Queued)
        }
    }

    fn set_status(&self, rowid: i64, status: SpoolStatus) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE telegram_spool SET status = ?2, updated_at = ?3 WHERE rowid = ?1",
            rusqlite::params![rowid, status.as_str(), Self::now_ms()],
        )?;
        Ok(())
    }

    pub fn queued_count(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM telegram_spool WHERE status = 'queued'",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    pub fn tombstoned_count(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM telegram_spool WHERE status = 'tombstoned'",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    // ------------------------------------------------------------------
    // getUpdates offset persistence (token-fingerprint keyed)
    // ------------------------------------------------------------------

    /// Loads the persisted next offset for a token fingerprint. A rotated
    /// token has a different fingerprint and therefore starts fresh
    /// (token-rotation offset discard).
    pub fn load_offset(&self, token_fingerprint: &str) -> anyhow::Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT next_offset FROM telegram_offsets WHERE token_fingerprint = ?1")?;
        let mut rows = stmt.query([token_fingerprint])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// Persists the next offset AFTER dispatch, so a crash between poll and
    /// dispatch re-polls (the spool dedupes by update_id).
    pub fn store_offset(&self, token_fingerprint: &str, next_offset: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO telegram_offsets (token_fingerprint, next_offset, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(token_fingerprint)
             DO UPDATE SET next_offset = ?2, updated_at = ?3",
            rusqlite::params![token_fingerprint, next_offset, Self::now_ms()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Persisted reply-context cache (reply-chain hydration)
    // ------------------------------------------------------------------

    /// Records message context so replies can hydrate the quoted message even
    /// across restarts. The cache is bounded (oldest rows evicted past 5000).
    pub fn record_reply_context(
        &self,
        chat_id: &str,
        message_id: i64,
        text: Option<&str>,
        media_paths: &[String],
    ) -> anyhow::Result<()> {
        let media_json = serde_json::to_string(media_paths)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO telegram_reply_context
             (chat_id, message_id, text, media_paths, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(chat_id, message_id)
             DO UPDATE SET text = ?3, media_paths = ?4, recorded_at = ?5",
            rusqlite::params![chat_id, message_id, text, media_json, Self::now_ms()],
        )?;
        conn.execute(
            "DELETE FROM telegram_reply_context WHERE (chat_id, message_id) IN (
                SELECT chat_id, message_id FROM telegram_reply_context
                ORDER BY recorded_at DESC LIMIT -1 OFFSET 5000)",
            [],
        )?;
        Ok(())
    }

    /// Hydrates the context of a replied-to message, when cached.
    pub fn hydrate_reply_context(
        &self,
        chat_id: &str,
        message_id: i64,
    ) -> anyhow::Result<Option<(Option<String>, Vec<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT text, media_paths FROM telegram_reply_context
             WHERE chat_id = ?1 AND message_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![chat_id, message_id])?;
        Ok(match rows.next()? {
            Some(row) => {
                let text: Option<String> = row.get(0)?;
                let media_json: Option<String> = row.get(1)?;
                let media = media_json
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default();
                Some((text, media))
            }
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_dedupes_by_update_id() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        assert!(spool.enqueue(100, "{\"a\":1}").unwrap());
        assert!(!spool.enqueue(100, "{\"a\":1}").unwrap());
        assert_eq!(spool.queued_count().unwrap(), 1);
    }

    #[test]
    fn adoption_completes_update() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        spool.enqueue(1, "{}").unwrap();
        let batch = spool.next_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        spool.mark_adopted(batch[0].rowid).unwrap();
        assert_eq!(spool.queued_count().unwrap(), 0);
        assert!(spool.next_batch(10).unwrap().is_empty());
    }

    #[test]
    fn poison_update_tombstoned_after_max_attempts() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        spool.enqueue(7, "{}").unwrap();
        let entry = &spool.next_batch(1).unwrap()[0];
        for attempt in 1..=TELEGRAM_SPOOL_MAX_ATTEMPTS {
            let status = spool.record_failed_attempt(entry.rowid).unwrap();
            if attempt < TELEGRAM_SPOOL_MAX_ATTEMPTS {
                assert_eq!(status, SpoolStatus::Queued);
            } else {
                assert_eq!(status, SpoolStatus::Tombstoned);
            }
        }
        assert_eq!(spool.queued_count().unwrap(), 0);
        assert_eq!(spool.tombstoned_count().unwrap(), 1);
    }

    #[test]
    fn batch_ordered_by_update_id() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        spool.enqueue(5, "{}").unwrap();
        spool.enqueue(3, "{}").unwrap();
        spool.enqueue(4, "{}").unwrap();
        let ids: Vec<i64> = spool
            .next_batch(10)
            .unwrap()
            .iter()
            .map(|u| u.update_id)
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn offset_persisted_per_fingerprint() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        assert_eq!(spool.load_offset("fp-a").unwrap(), None);
        spool.store_offset("fp-a", 42).unwrap();
        spool.store_offset("fp-a", 43).unwrap();
        assert_eq!(spool.load_offset("fp-a").unwrap(), Some(43));
        // Token rotation → different fingerprint → fresh offset.
        assert_eq!(spool.load_offset("fp-b").unwrap(), None);
    }

    #[test]
    fn reply_context_roundtrip() {
        let spool = TelegramUpdateSpool::open_in_memory().unwrap();
        spool
            .record_reply_context("123", 9, Some("hello"), &["/tmp/a.jpg".to_string()])
            .unwrap();
        let (text, media) = spool.hydrate_reply_context("123", 9).unwrap().unwrap();
        assert_eq!(text.as_deref(), Some("hello"));
        assert_eq!(media, vec!["/tmp/a.jpg".to_string()]);
        assert!(spool.hydrate_reply_context("123", 10).unwrap().is_none());
    }
}
