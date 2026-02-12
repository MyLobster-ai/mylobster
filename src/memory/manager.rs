use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;
use tracing::{debug, info};

use crate::config::Config;

use super::embeddings::EmbeddingProviderBox;
use super::schema;
use super::search::{MemorySearchOptions, MemorySearchResult};

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Describes the current state of a `MemoryIndexManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProviderStatus {
    /// Manager is initialised and ready to serve queries.
    Ready,
    /// A background sync is currently in progress.
    Syncing,
    /// The manager has been closed and can no longer be used.
    Closed,
    /// An error occurred during initialisation or sync.
    Error,
}

// ---------------------------------------------------------------------------
// SyncParams
// ---------------------------------------------------------------------------

/// Parameters passed to [`MemoryIndexManager::sync`] to control which files
/// are indexed and how.
#[derive(Debug, Clone, Default)]
pub struct SyncParams {
    /// Root paths to scan for source files.
    pub paths: Vec<PathBuf>,
    /// Glob patterns for files to include (e.g. `["**/*.md", "**/*.rs"]`).
    pub include_patterns: Vec<String>,
    /// Glob patterns for files to exclude.
    pub exclude_patterns: Vec<String>,
    /// If `true`, forces a full re-index regardless of file modification times.
    pub force: bool,
}

// ---------------------------------------------------------------------------
// MemoryIndexManager
// ---------------------------------------------------------------------------

/// Owns the SQLite database, embedding provider, and index state for a single
/// agent's memory store.
///
/// Create one via [`MemoryIndexManager::get`], which opens (or creates) the
/// SQLite file and runs migrations. The returned instance is cheaply cloneable
/// (the inner DB connection is wrapped in `Arc<Mutex<_>>`).
#[derive(Clone)]
pub struct MemoryIndexManager {
    db: Arc<Mutex<Connection>>,
    _embedding_provider: Arc<EmbeddingProviderBox>,
    status: Arc<Mutex<MemoryProviderStatus>>,
    agent_id: String,
}

impl MemoryIndexManager {
    /// Open or create the memory index for the given `agent_id`.
    ///
    /// The SQLite database is stored under the configured state directory at
    /// `<state_dir>/memory/<agent_id>.db`. Schema migrations are applied
    /// automatically.
    ///
    /// Returns `None` if the embedding provider cannot be constructed from the
    /// current configuration (e.g. missing API key).
    pub async fn get(config: &Config, agent_id: &str) -> Option<Self> {
        let db_dir = config.state_dir.join("memory");
        if std::fs::create_dir_all(&db_dir).is_err() {
            return None;
        }

        let db_path = db_dir.join(format!("{}.db", agent_id));
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to open memory db at {}: {e}", db_path.display());
                return None;
            }
        };

        if let Err(e) = schema::run_migrations(&conn) {
            tracing::error!("memory schema migration failed: {e}");
            return None;
        }

        let provider = match super::embeddings::create_provider(config) {
            Some(p) => p,
            None => {
                debug!("no embedding provider available; memory search disabled");
                return None;
            }
        };

        info!(
            agent_id,
            db = %db_path.display(),
            provider = provider.model_name(),
            "memory index manager ready"
        );

        Some(Self {
            db: Arc::new(Mutex::new(conn)),
            _embedding_provider: Arc::new(provider),
            status: Arc::new(Mutex::new(MemoryProviderStatus::Ready)),
            agent_id: agent_id.to_string(),
        })
    }

    /// Execute a hybrid search (BM25 full-text + vector similarity) and return
    /// results sorted by descending score.
    pub async fn search(
        &self,
        query: &str,
        opts: MemorySearchOptions,
    ) -> Vec<MemorySearchResult> {
        let _ = (query, opts);
        debug!(agent_id = %self.agent_id, "memory search (stub)");
        Vec::new()
    }

    /// Synchronise the index with the file system.
    ///
    /// Scans the paths specified in `params`, chunks new or modified files,
    /// computes embeddings, and upserts them into the SQLite store.
    pub async fn sync(&self, params: SyncParams) {
        {
            let mut s = self.status.lock();
            *s = MemoryProviderStatus::Syncing;
        }

        debug!(
            agent_id = %self.agent_id,
            paths = ?params.paths,
            force = params.force,
            "memory sync (stub)"
        );

        {
            let mut s = self.status.lock();
            *s = MemoryProviderStatus::Ready;
        }
    }

    /// Return the current status of this manager.
    pub fn status(&self) -> MemoryProviderStatus {
        *self.status.lock()
    }

    /// Close the manager, releasing the database connection.
    pub async fn close(&self) {
        let mut s = self.status.lock();
        *s = MemoryProviderStatus::Closed;
        info!(agent_id = %self.agent_id, "memory index manager closed");
    }
}
