use anyhow::Result;
use rusqlite::Connection;
use tracing::debug;

/// Current schema version.  Increment when adding new migrations.
const SCHEMA_VERSION: u32 = 1;

/// Apply all pending migrations to `conn`.
///
/// Migrations are idempotent — tables are created with `IF NOT EXISTS` and the
/// `meta` table tracks which version has been applied so we only run new ones.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Enable WAL mode for better concurrent read performance.
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;

    // ------------------------------------------------------------------
    // meta — tracks schema version and arbitrary key/value pairs.
    // ------------------------------------------------------------------
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;

    let current_version = get_schema_version(conn);

    if current_version >= SCHEMA_VERSION {
        debug!(version = current_version, "memory schema up to date");
        return Ok(());
    }

    if current_version < 1 {
        migrate_v1(conn)?;
    }

    set_schema_version(conn, SCHEMA_VERSION)?;
    debug!(version = SCHEMA_VERSION, "memory schema migrated");
    Ok(())
}

// ---------------------------------------------------------------------------
// v1 — initial tables
// ---------------------------------------------------------------------------

fn migrate_v1(conn: &Connection) -> Result<()> {
    // ------------------------------------------------------------------
    // files — tracks indexed source files and their modification times.
    // ------------------------------------------------------------------
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            path         TEXT    NOT NULL UNIQUE,
            hash         TEXT    NOT NULL,
            modified_at  TEXT    NOT NULL,
            indexed_at   TEXT    NOT NULL,
            chunk_count  INTEGER NOT NULL DEFAULT 0
        );",
    )?;

    // ------------------------------------------------------------------
    // chunks — individual text chunks derived from files.
    // ------------------------------------------------------------------
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chunks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            text        TEXT    NOT NULL,
            start_line  INTEGER NOT NULL DEFAULT 0,
            end_line    INTEGER NOT NULL DEFAULT 0,
            token_count INTEGER NOT NULL DEFAULT 0,
            embedding   BLOB,
            UNIQUE(file_id, chunk_index)
        );",
    )?;

    // Index for efficient file-based lookups and joins.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_chunks_file_id ON chunks(file_id);",
    )?;

    // ------------------------------------------------------------------
    // embedding_cache — caches raw embedding vectors keyed by content
    // hash so we avoid redundant API calls for unchanged content.
    // ------------------------------------------------------------------
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embedding_cache (
            hash       TEXT PRIMARY KEY,
            model      TEXT NOT NULL,
            dimensions INTEGER NOT NULL,
            embedding  BLOB NOT NULL,
            created_at TEXT NOT NULL
        );",
    )?;

    // ------------------------------------------------------------------
    // chunks_fts — FTS5 virtual table for BM25 full-text search.
    // ------------------------------------------------------------------
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            content='chunks',
            content_rowid='id',
            tokenize='porter unicode61'
        );",
    )?;

    // Triggers to keep the FTS index in sync with the chunks table.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
            INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
        END;",
    )?;
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
        END;",
    )?;
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
            INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
        END;",
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_schema_version(conn: &Connection) -> u32 {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| {
            let v: String = row.get(0)?;
            Ok(v.parse::<u32>().unwrap_or(0))
        },
    )
    .unwrap_or(0)
}

fn set_schema_version(conn: &Connection, version: u32) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        [version.to_string()],
    )?;
    Ok(())
}
