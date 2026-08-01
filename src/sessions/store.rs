//! SQLite-backed session-state store (v2026.7.1 parity, sessions scope).
//!
//! Upstream's SQLite-first state consolidation moves session metadata into a
//! shared state DB. This module implements the *sessions* slice of that
//! consolidation on rusqlite:
//!
//! - session metadata rows (key, agent, model, lifecycle, epochs, labels);
//! - agent-scoped lookups (multi-agent leak fix, v2026.7.1);
//! - phantom rows hidden from listings; malformed persisted rows quarantined
//!   for doctor surfacing instead of poisoning loads (v2026.7.1);
//! - dead-main-session recreation without stale metadata (v2026.7.1);
//! - mixed-case session-key migration at startup (v2026.7.1);
//! - terminal lifecycle preservation when final run metadata persists from a
//!   stale snapshot (v2026.5.2, via [`crate::sessions::lifecycle`]);
//! - `skillsSnapshot` persistence that strips the runtime-only
//!   `resolvedSkills` array — rehydrated from disk on cold resume
//!   (v2026.5.2 Skills row).

use crate::sessions::lifecycle::{
    self, ApplyOutcome, FinalRunMetadata, LifecycleRecord, LifecycleState,
};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;

/// A persisted session row.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedSession {
    pub session_key: String,
    pub id: String,
    pub agent_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub lifecycle: LifecycleRecord,
    pub durable_external_pointer: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub metadata: Value,
}

impl PersistedSession {
    pub fn new(session_key: &str, id: &str, agent_id: &str, now_ms: i64) -> Self {
        Self {
            session_key: session_key.to_string(),
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            title: None,
            model: None,
            thinking: None,
            lifecycle: LifecycleRecord::new(LifecycleState::Active, 0, now_ms),
            durable_external_pointer: crate::sessions::maintenance::is_durable_external_pointer(
                session_key,
            ),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            metadata: Value::Object(Default::default()),
        }
    }
}

/// A row moved to quarantine because it could not be loaded safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedRow {
    pub session_key: String,
    pub reason: String,
}

/// Strip the runtime-only `resolvedSkills` array from a skills snapshot
/// before persistence (v2026.5.2): it is rehydrated from disk on cold
/// resume, so persisting it only bloats the store and goes stale.
pub fn strip_runtime_skills_snapshot(snapshot: &Value) -> Value {
    let mut out = snapshot.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.remove("resolvedSkills");
    }
    out
}

/// SQLite-backed session-state store.
pub struct SqliteSessionStore {
    conn: parking_lot::Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Open (or create) the store at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let conn = Connection::open(path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Self::init(conn)
    }

    /// In-memory store (tests, ephemeral runs).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT 'default',
                title TEXT,
                model TEXT,
                thinking TEXT,
                lifecycle_state TEXT NOT NULL DEFAULT 'active',
                reset_epoch INTEGER NOT NULL DEFAULT 0,
                lifecycle_updated_at_ms INTEGER NOT NULL DEFAULT 0,
                durable_external_pointer INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                metadata TEXT NOT NULL DEFAULT '{}',
                skills_snapshot TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent_id);
            CREATE TABLE IF NOT EXISTS sessions_quarantine (
                session_key TEXT NOT NULL,
                raw TEXT NOT NULL,
                reason TEXT NOT NULL,
                quarantined_at_ms INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    /// Insert or update a session row.
    pub fn upsert(&self, session: &PersistedSession) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sessions (
                session_key, id, agent_id, title, model, thinking,
                lifecycle_state, reset_epoch, lifecycle_updated_at_ms,
                durable_external_pointer, created_at_ms, updated_at_ms, metadata
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
            ON CONFLICT(session_key) DO UPDATE SET
                id=excluded.id, agent_id=excluded.agent_id, title=excluded.title,
                model=excluded.model, thinking=excluded.thinking,
                lifecycle_state=excluded.lifecycle_state,
                reset_epoch=excluded.reset_epoch,
                lifecycle_updated_at_ms=excluded.lifecycle_updated_at_ms,
                durable_external_pointer=excluded.durable_external_pointer,
                updated_at_ms=excluded.updated_at_ms,
                metadata=excluded.metadata",
            params![
                session.session_key,
                session.id,
                session.agent_id,
                session.title,
                session.model,
                session.thinking,
                session.lifecycle.state.as_str(),
                session.lifecycle.reset_epoch as i64,
                session.lifecycle.updated_at_ms,
                session.durable_external_pointer as i64,
                session.created_at_ms,
                session.updated_at_ms,
                serde_json::to_string(&session.metadata).unwrap_or_else(|_| "{}".into()),
            ],
        )?;
        Ok(())
    }

    fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<PersistedSession, String>> {
        let session_key: String = row.get("session_key")?;
        let metadata_raw: String = row.get("metadata")?;
        let metadata: Value = match serde_json::from_str(&metadata_raw) {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("malformed metadata JSON: {e}"))),
        };
        let state_raw: String = row.get("lifecycle_state")?;
        let Some(state) = lifecycle::parse_lifecycle_state(&state_raw) else {
            return Ok(Err(format!("unknown lifecycle state '{state_raw}'")));
        };
        let reset_epoch: i64 = row.get("reset_epoch")?;
        Ok(Ok(PersistedSession {
            session_key,
            id: row.get("id")?,
            agent_id: row.get("agent_id")?,
            title: row.get("title")?,
            model: row.get("model")?,
            thinking: row.get("thinking")?,
            lifecycle: LifecycleRecord::new(
                state,
                reset_epoch.max(0) as u64,
                row.get("lifecycle_updated_at_ms")?,
            ),
            durable_external_pointer: row.get::<_, i64>("durable_external_pointer")? != 0,
            created_at_ms: row.get("created_at_ms")?,
            updated_at_ms: row.get("updated_at_ms")?,
            metadata,
        }))
    }

    /// Get one session (phantom rows return `None`).
    pub fn get(&self, session_key: &str) -> rusqlite::Result<Option<PersistedSession>> {
        let conn = self.conn.lock();
        let parsed = conn
            .query_row(
                "SELECT * FROM sessions WHERE session_key = ?1 AND session_key <> '' AND id <> ''",
                params![session_key],
                Self::row_to_session,
            )
            .optional()?;
        Ok(parsed.and_then(|r| r.ok()))
    }

    /// List all well-formed sessions. Phantom rows (empty key or id) are
    /// hidden; malformed rows are skipped (quarantine them with
    /// [`Self::quarantine_malformed`]).
    pub fn list(&self) -> rusqlite::Result<Vec<PersistedSession>> {
        self.list_where("1=1", params![])
    }

    /// Agent-scoped lookup (v2026.7.1 multi-agent leak fix): only sessions
    /// owned by `agent_id` are returned.
    pub fn list_for_agent(&self, agent_id: &str) -> rusqlite::Result<Vec<PersistedSession>> {
        self.list_where("agent_id = ?1", params![agent_id])
    }

    fn list_where(
        &self,
        where_clause: &str,
        params: impl rusqlite::Params,
    ) -> rusqlite::Result<Vec<PersistedSession>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT * FROM sessions
             WHERE session_key <> '' AND id <> '' AND ({where_clause})
             ORDER BY updated_at_ms DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params, Self::row_to_session)?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(session) = row? {
                out.push(session);
            }
        }
        Ok(out)
    }

    /// Delete a session row.
    pub fn delete(&self, session_key: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM sessions WHERE session_key = ?1",
            params![session_key],
        )?;
        Ok(n > 0)
    }

    /// Move malformed rows (bad metadata JSON / unknown lifecycle state /
    /// phantom identity) into the quarantine table for doctor surfacing.
    /// Returns the quarantined rows.
    pub fn quarantine_malformed(&self, now_ms: i64) -> rusqlite::Result<Vec<QuarantinedRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT * FROM sessions")?;
        let mut bad: Vec<(String, String)> = Vec::new();
        {
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get("session_key")?;
                let id: String = row.get("id")?;
                if key.is_empty() || id.is_empty() {
                    bad.push((key, "phantom row (empty session key or id)".into()));
                    continue;
                }
                if let Err(reason) = Self::row_to_session(row)? {
                    bad.push((key, reason));
                }
            }
        }
        drop(stmt);
        let mut out = Vec::new();
        for (key, reason) in bad {
            let raw: String = conn
                .query_row(
                    "SELECT COALESCE(metadata,'') FROM sessions WHERE session_key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            conn.execute(
                "INSERT INTO sessions_quarantine (session_key, raw, reason, quarantined_at_ms)
                 VALUES (?1,?2,?3,?4)",
                params![key, raw, reason, now_ms],
            )?;
            conn.execute(
                "DELETE FROM sessions WHERE session_key = ?1",
                params![key],
            )?;
            out.push(QuarantinedRow {
                session_key: key,
                reason,
            });
        }
        Ok(out)
    }

    /// Quarantined rows (doctor surface).
    pub fn quarantined(&self) -> rusqlite::Result<Vec<QuarantinedRow>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT session_key, reason FROM sessions_quarantine ORDER BY rowid")?;
        let rows = stmt.query_map([], |row| {
            Ok(QuarantinedRow {
                session_key: row.get(0)?,
                reason: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// Recreate a dead main session with a fresh identity, carrying **no**
    /// stale metadata from the previous row (v2026.7.1).
    pub fn recreate_main_session(
        &self,
        session_key: &str,
        agent_id: &str,
        new_id: &str,
        now_ms: i64,
    ) -> rusqlite::Result<PersistedSession> {
        self.delete(session_key)?;
        let fresh = PersistedSession::new(session_key, new_id, agent_id, now_ms);
        self.upsert(&fresh)?;
        Ok(fresh)
    }

    /// Startup migration: lowercase mixed-case session keys. When the
    /// lowercase key already exists, the newer row wins and the older row is
    /// quarantined. Returns the number of migrated rows.
    pub fn migrate_mixed_case_keys(&self, now_ms: i64) -> rusqlite::Result<usize> {
        let mixed: Vec<(String, i64)> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT session_key, updated_at_ms FROM sessions
                 WHERE session_key <> lower(session_key)",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut migrated = 0usize;
        for (key, updated_at) in mixed {
            let lower = key.to_lowercase();
            let conn = self.conn.lock();
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT updated_at_ms FROM sessions WHERE session_key = ?1",
                    params![lower],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                None => {
                    conn.execute(
                        "UPDATE sessions SET session_key = ?1 WHERE session_key = ?2",
                        params![lower, key],
                    )?;
                    migrated += 1;
                }
                Some(existing_updated) if updated_at > existing_updated => {
                    // Mixed-case row is newer: quarantine the stale lowercase
                    // row, then take its key.
                    conn.execute(
                        "INSERT INTO sessions_quarantine (session_key, raw, reason, quarantined_at_ms)
                         SELECT session_key, metadata,
                                'superseded by newer mixed-case row during key migration', ?2
                         FROM sessions WHERE session_key = ?1",
                        params![lower, now_ms],
                    )?;
                    conn.execute(
                        "DELETE FROM sessions WHERE session_key = ?1",
                        params![lower],
                    )?;
                    conn.execute(
                        "UPDATE sessions SET session_key = ?1 WHERE session_key = ?2",
                        params![lower, key],
                    )?;
                    migrated += 1;
                }
                Some(_) => {
                    // Lowercase row is newer: quarantine the mixed-case dupe.
                    conn.execute(
                        "INSERT INTO sessions_quarantine (session_key, raw, reason, quarantined_at_ms)
                         SELECT session_key, metadata,
                                'duplicate mixed-case key during migration', ?2
                         FROM sessions WHERE session_key = ?1",
                        params![key, now_ms],
                    )?;
                    conn.execute(
                        "DELETE FROM sessions WHERE session_key = ?1",
                        params![key],
                    )?;
                }
            }
        }
        Ok(migrated)
    }

    // ------------------------------------------------------------------
    // Lifecycle persistence
    // ------------------------------------------------------------------

    /// Persist final run metadata, preserving terminal lifecycle state when
    /// the snapshot is stale (v2026.5.2). Returns how the merge resolved.
    pub fn persist_final_run_metadata(
        &self,
        session_key: &str,
        snapshot: &FinalRunMetadata,
    ) -> rusqlite::Result<Option<ApplyOutcome>> {
        let Some(mut session) = self.get(session_key)? else {
            return Ok(None);
        };
        let outcome = lifecycle::merge_final_run_metadata(&mut session.lifecycle, snapshot);
        if outcome == ApplyOutcome::Applied {
            session.updated_at_ms = session.updated_at_ms.max(snapshot.snapshot_at_ms);
            self.upsert(&session)?;
        }
        Ok(Some(outcome))
    }

    // ------------------------------------------------------------------
    // Skills snapshot (v2026.5.2)
    // ------------------------------------------------------------------

    /// Persist a session's skills snapshot with the runtime-only
    /// `resolvedSkills` array stripped.
    pub fn persist_skills_snapshot(
        &self,
        session_key: &str,
        snapshot: &Value,
    ) -> rusqlite::Result<()> {
        let stripped = strip_runtime_skills_snapshot(snapshot);
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET skills_snapshot = ?2 WHERE session_key = ?1",
            params![
                session_key,
                serde_json::to_string(&stripped).unwrap_or_else(|_| "{}".into())
            ],
        )?;
        Ok(())
    }

    /// Load a session's persisted skills snapshot (never contains
    /// `resolvedSkills` — that is rehydrated from disk on cold resume).
    pub fn skills_snapshot(&self, session_key: &str) -> rusqlite::Result<Option<Value>> {
        let conn = self.conn.lock();
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT skills_snapshot FROM sessions WHERE session_key = ?1",
                params![session_key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> SqliteSessionStore {
        SqliteSessionStore::open_in_memory().unwrap()
    }

    fn session(key: &str, agent: &str, now: i64) -> PersistedSession {
        PersistedSession::new(key, &format!("id-{key}"), agent, now)
    }

    #[test]
    fn upsert_get_roundtrip() {
        let s = store();
        let mut sess = session("main", "default", 100);
        sess.title = Some("Hello".into());
        sess.model = Some("claude-fable-5".into());
        s.upsert(&sess).unwrap();

        let loaded = s.get("main").unwrap().unwrap();
        assert_eq!(loaded, sess);
        assert!(s.get("missing").unwrap().is_none());
    }

    #[test]
    fn file_backed_store_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/sessions.db");
        {
            let s = SqliteSessionStore::open(&path).unwrap();
            s.upsert(&session("main", "default", 1)).unwrap();
        }
        let s = SqliteSessionStore::open(&path).unwrap();
        assert!(s.get("main").unwrap().is_some());
    }

    #[test]
    fn agent_scoped_lookups_do_not_leak_other_agents() {
        let s = store();
        s.upsert(&session("a:main", "agent-a", 1)).unwrap();
        s.upsert(&session("b:main", "agent-b", 2)).unwrap();

        let a = s.list_for_agent("agent-a").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].session_key, "a:main");
        assert!(s.list_for_agent("agent-c").unwrap().is_empty());
        assert_eq!(s.list().unwrap().len(), 2);
    }

    #[test]
    fn phantom_rows_are_hidden_from_lists_and_gets() {
        let s = store();
        s.upsert(&session("real", "default", 1)).unwrap();
        // Phantom: empty id.
        let mut phantom = session("ghost", "default", 2);
        phantom.id = String::new();
        s.upsert(&phantom).unwrap();

        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_key, "real");
        assert!(s.get("ghost").unwrap().is_none());
    }

    #[test]
    fn malformed_rows_are_quarantined_for_doctor() {
        let s = store();
        s.upsert(&session("good", "default", 1)).unwrap();
        // Corrupt a row's metadata directly.
        {
            let conn = s.conn.lock();
            conn.execute(
                "INSERT INTO sessions (session_key, id, agent_id, created_at_ms, updated_at_ms, metadata)
                 VALUES ('bad', 'id-bad', 'default', 1, 1, 'not-json')",
                [],
            )
            .unwrap();
        }
        let quarantined = s.quarantine_malformed(500).unwrap();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].session_key, "bad");
        assert!(quarantined[0].reason.contains("malformed metadata"));

        // Store loads cleanly afterwards; doctor sees the quarantined row.
        assert_eq!(s.list().unwrap().len(), 1);
        assert_eq!(s.quarantined().unwrap().len(), 1);
        assert!(s.get("bad").unwrap().is_none());
    }

    #[test]
    fn recreate_main_session_carries_no_stale_metadata() {
        let s = store();
        let mut dead = session("main", "default", 100);
        dead.title = Some("old title".into());
        dead.model = Some("old-model".into());
        dead.lifecycle = LifecycleRecord::new(LifecycleState::Errored, 7, 150);
        dead.metadata = json!({"stale": true});
        s.upsert(&dead).unwrap();

        let fresh = s
            .recreate_main_session("main", "default", "id-new", 200)
            .unwrap();
        assert_eq!(fresh.title, None);
        assert_eq!(fresh.model, None);
        assert_eq!(fresh.lifecycle.state, LifecycleState::Active);
        assert_eq!(fresh.lifecycle.reset_epoch, 0);
        assert_eq!(fresh.metadata, json!({}));

        let loaded = s.get("main").unwrap().unwrap();
        assert_eq!(loaded.id, "id-new");
        assert_eq!(loaded.metadata, json!({}));
    }

    #[test]
    fn mixed_case_keys_migrate_to_lowercase_at_startup() {
        let s = store();
        s.upsert(&session("Telegram:DM:42", "default", 10)).unwrap();
        let migrated = s.migrate_mixed_case_keys(99).unwrap();
        assert_eq!(migrated, 1);
        assert!(s.get("telegram:dm:42").unwrap().is_some());
        assert!(s.get("Telegram:DM:42").unwrap().is_none());
    }

    #[test]
    fn mixed_case_migration_keeps_newer_row_on_collision() {
        let s = store();
        let mut lower = session("telegram:dm:42", "default", 10);
        lower.title = Some("older-lower".into());
        s.upsert(&lower).unwrap();
        let mut mixed = session("Telegram:DM:42", "default", 20);
        mixed.title = Some("newer-mixed".into());
        mixed.updated_at_ms = 20;
        s.upsert(&mixed).unwrap();

        s.migrate_mixed_case_keys(99).unwrap();
        let kept = s.get("telegram:dm:42").unwrap().unwrap();
        assert_eq!(kept.title.as_deref(), Some("newer-mixed"));
        assert!(s.get("Telegram:DM:42").unwrap().is_none());
        assert_eq!(s.quarantined().unwrap().len(), 1);
    }

    #[test]
    fn terminal_state_preserved_against_stale_snapshot_persist() {
        let s = store();
        let mut sess = session("main", "default", 100);
        sess.lifecycle = LifecycleRecord::new(LifecycleState::Completed, 1, 200);
        s.upsert(&sess).unwrap();

        // Stale in-memory snapshot captured mid-run flushes late.
        let outcome = s
            .persist_final_run_metadata(
                "main",
                &FinalRunMetadata {
                    state: LifecycleState::Active,
                    reset_epoch: 1,
                    snapshot_at_ms: 150,
                    run_id: Some("run-1".into()),
                },
            )
            .unwrap();
        assert_eq!(outcome, Some(ApplyOutcome::PreservedTerminal));
        let loaded = s.get("main").unwrap().unwrap();
        assert_eq!(loaded.lifecycle.state, LifecycleState::Completed);
    }

    #[test]
    fn skills_snapshot_strips_runtime_only_resolved_skills() {
        let s = store();
        s.upsert(&session("main", "default", 1)).unwrap();
        let snapshot = json!({
            "configHash": "abc123",
            "allowed": ["web", "pdf"],
            "resolvedSkills": [{"name": "web", "path": "/skills/web"}],
        });
        s.persist_skills_snapshot("main", &snapshot).unwrap();

        let loaded = s.skills_snapshot("main").unwrap().unwrap();
        assert!(loaded.get("resolvedSkills").is_none(), "runtime-only array must not persist");
        assert_eq!(loaded["configHash"], "abc123");
        assert_eq!(loaded["allowed"], json!(["web", "pdf"]));
    }

    #[test]
    fn strip_runtime_skills_snapshot_is_pure() {
        let snapshot = json!({"resolvedSkills": [], "keep": 1});
        let stripped = strip_runtime_skills_snapshot(&snapshot);
        assert_eq!(stripped, json!({"keep": 1}));
        // Original untouched.
        assert!(snapshot.get("resolvedSkills").is_some());
    }

    #[test]
    fn delete_removes_rows() {
        let s = store();
        s.upsert(&session("gone", "default", 1)).unwrap();
        assert!(s.delete("gone").unwrap());
        assert!(!s.delete("gone").unwrap());
    }
}
