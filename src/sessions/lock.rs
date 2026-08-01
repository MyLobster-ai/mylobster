//! Session transcript write locks (v2026.5.2 / v2026.7.1 parity).
//!
//! - `session.writeLock.acquireTimeoutMs` (default 60s) bounds how long a
//!   writer waits for a transcript lock (v2026.5.2).
//! - Max-hold reclaim at acquisition: a lock held past `maxHoldMs` is treated
//!   as stale, reclaimed by the next acquirer, and reported (v2026.7.1).
//! - Guards release on drop, so throw/manual-abort/timeout/teardown paths all
//!   release the lock (v2026.7.1 "release on fence-read throw/manual
//!   abort/timeout/teardown").
//! - Lock-file identity: cleaned transcript locks are matched by exact lock
//!   path first, then by canonical session fallback so topic-suffixed
//!   transcripts resume after a restart (v2026.5.2).

use crate::config::SessionWriteLockConfig;

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

pub const DEFAULT_ACQUIRE_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_MAX_HOLD_MS: u64 = 300_000;

/// Resolved write-lock settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteLockSettings {
    pub acquire_timeout: Duration,
    pub max_hold: Duration,
}

impl Default for WriteLockSettings {
    fn default() -> Self {
        Self {
            acquire_timeout: Duration::from_millis(DEFAULT_ACQUIRE_TIMEOUT_MS),
            max_hold: Duration::from_millis(DEFAULT_MAX_HOLD_MS),
        }
    }
}

impl WriteLockSettings {
    /// Resolve from `session.writeLock` config; unset fields use defaults.
    /// Zero values are clamped to the defaults (a 0ms acquire timeout would
    /// make every write fail; a 0ms max hold would reclaim live locks).
    pub fn from_config(cfg: Option<&SessionWriteLockConfig>) -> Self {
        let defaults = Self::default();
        let Some(cfg) = cfg else { return defaults };
        let acquire = cfg
            .acquire_timeout_ms
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(defaults.acquire_timeout);
        let max_hold = cfg
            .max_hold_ms
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(defaults.max_hold);
        Self {
            acquire_timeout: acquire,
            max_hold,
        }
    }
}

/// Error returned when a lock acquisition times out.
#[derive(Debug, thiserror::Error)]
#[error(
    "timed out acquiring session write lock for {session_key} after {waited_ms}ms \
     (holder: {holder:?}, held for {held_ms:?}ms)"
)]
pub struct LockAcquireTimeout {
    pub session_key: String,
    pub waited_ms: u64,
    pub holder: Option<String>,
    pub held_ms: Option<u64>,
}

/// Report emitted when a stale (max-hold-exceeded) lock is reclaimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleLockReport {
    pub session_key: String,
    pub holder: Option<String>,
    pub held_ms: u64,
}

#[derive(Debug, Default)]
struct HoldState {
    holder: Option<String>,
    held_since: Option<Instant>,
    /// Bumped on every reclaim; guards from an earlier generation must not
    /// release a permit into the reclaimed semaphore.
    generation: u64,
}

struct LockCell {
    sem: Arc<Semaphore>,
    state: parking_lot::Mutex<HoldState>,
}

impl LockCell {
    fn new() -> Self {
        Self {
            sem: Arc::new(Semaphore::new(1)),
            state: parking_lot::Mutex::new(HoldState::default()),
        }
    }
}

/// RAII guard for a held session write lock. Dropping releases the lock,
/// which covers error/abort/timeout/teardown paths uniformly.
pub struct SessionWriteLockGuard {
    cell: Arc<LockCell>,
    permit: Option<OwnedSemaphorePermit>,
    generation: u64,
}

impl std::fmt::Debug for SessionWriteLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWriteLockGuard")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Drop for SessionWriteLockGuard {
    fn drop(&mut self) {
        let mut state = self.cell.state.lock();
        if state.generation == self.generation {
            state.holder = None;
            state.held_since = None;
            // permit drops normally, releasing the lock
        } else if let Some(permit) = self.permit.take() {
            // This lock was reclaimed as stale while we held it. The
            // semaphore was re-armed for the new holder; swallowing our
            // permit keeps it from over-counting.
            permit.forget();
        }
    }
}

/// In-process write-lock manager, one lock per session key.
pub struct SessionWriteLocks {
    settings: WriteLockSettings,
    cells: DashMap<String, Arc<LockCell>>,
    stale_reports: parking_lot::Mutex<Vec<StaleLockReport>>,
}

impl SessionWriteLocks {
    pub fn new(settings: WriteLockSettings) -> Self {
        Self {
            settings,
            cells: DashMap::new(),
            stale_reports: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn settings(&self) -> WriteLockSettings {
        self.settings
    }

    /// Drain stale-lock reports accumulated by max-hold reclaims.
    pub fn take_stale_reports(&self) -> Vec<StaleLockReport> {
        std::mem::take(&mut self.stale_reports.lock())
    }

    fn cell(&self, key: &str) -> Arc<LockCell> {
        self.cells
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(LockCell::new()))
            .clone()
    }

    /// Acquire the write lock for `session_key`, waiting up to the configured
    /// `acquireTimeoutMs`. A holder that exceeded `maxHoldMs` is reclaimed at
    /// acquisition time (stale-lock reporting included).
    pub async fn acquire(
        &self,
        session_key: &str,
        holder: &str,
    ) -> Result<SessionWriteLockGuard, LockAcquireTimeout> {
        let cell = self.cell(session_key);
        let started = Instant::now();

        // Max-hold reclaim at acquisition: if the current holder has exceeded
        // the hold budget, reclaim before waiting the full acquire timeout.
        if let Some(guard) = self.try_reclaim_stale(session_key, &cell, holder) {
            return Ok(guard);
        }

        match tokio::time::timeout(
            self.settings.acquire_timeout,
            cell.sem.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(self.register_holder(&cell, permit, holder)),
            Ok(Err(_closed)) => unreachable!("session write-lock semaphore is never closed"),
            Err(_elapsed) => {
                // One more reclaim check: the holder may have crossed the
                // max-hold threshold while we were waiting.
                if let Some(guard) = self.try_reclaim_stale(session_key, &cell, holder) {
                    return Ok(guard);
                }
                let state = cell.state.lock();
                Err(LockAcquireTimeout {
                    session_key: session_key.to_string(),
                    waited_ms: started.elapsed().as_millis() as u64,
                    holder: state.holder.clone(),
                    held_ms: state.held_since.map(|s| s.elapsed().as_millis() as u64),
                })
            }
        }
    }

    fn register_holder(
        &self,
        cell: &Arc<LockCell>,
        permit: OwnedSemaphorePermit,
        holder: &str,
    ) -> SessionWriteLockGuard {
        let mut state = cell.state.lock();
        state.holder = Some(holder.to_string());
        state.held_since = Some(Instant::now());
        SessionWriteLockGuard {
            cell: cell.clone(),
            permit: Some(permit),
            generation: state.generation,
        }
    }

    /// If the current holder exceeded max-hold, reclaim the lock for
    /// `new_holder` and record a stale-lock report.
    fn try_reclaim_stale(
        &self,
        session_key: &str,
        cell: &Arc<LockCell>,
        new_holder: &str,
    ) -> Option<SessionWriteLockGuard> {
        let mut state = cell.state.lock();
        let held_since = state.held_since?;
        if held_since.elapsed() < self.settings.max_hold {
            return None;
        }
        let report = StaleLockReport {
            session_key: session_key.to_string(),
            holder: state.holder.clone(),
            held_ms: held_since.elapsed().as_millis() as u64,
        };
        // Invalidate the stale guard's generation and re-arm the semaphore
        // with one permit for the new holder.
        state.generation += 1;
        state.holder = Some(new_holder.to_string());
        state.held_since = Some(Instant::now());
        let generation = state.generation;
        drop(state);
        self.stale_reports.lock().push(report);

        cell.sem.add_permits(1);
        let permit = cell
            .sem
            .clone()
            .try_acquire_owned()
            .expect("permit just added for reclaim must be acquirable");
        Some(SessionWriteLockGuard {
            cell: cell.clone(),
            permit: Some(permit),
            generation,
        })
    }
}

// ============================================================================
// Lock-file identity (cleanup matching)
// ============================================================================

const LOCK_SUFFIX: &str = ".lock";

/// Lock file path for a transcript path: `<transcript>.lock`.
pub fn lock_path_for_transcript(transcript: &Path) -> PathBuf {
    let mut os = transcript.as_os_str().to_os_string();
    os.push(LOCK_SUFFIX);
    PathBuf::from(os)
}

/// Canonical transcript path for a possibly topic-suffixed transcript file.
///
/// Topic-scoped transcripts are stored as `<session>.topic-<id>.jsonl` (or
/// `<session>-topic-<id>.jsonl`); their canonical session file is
/// `<session>.jsonl`. Non-suffixed paths are returned unchanged.
pub fn canonical_transcript_path(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name().and_then(|f| f.to_str()) else {
        return path.to_path_buf();
    };
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (file_name, None),
    };
    let canonical_stem = strip_topic_suffix(stem);
    if canonical_stem == stem {
        return path.to_path_buf();
    }
    let canonical_name = match ext {
        Some(ext) => format!("{canonical_stem}.{ext}"),
        None => canonical_stem.to_string(),
    };
    path.with_file_name(canonical_name)
}

fn strip_topic_suffix(stem: &str) -> &str {
    for sep in [".topic-", "-topic-"] {
        if let Some(idx) = stem.rfind(sep) {
            let suffix = &stem[idx + sep.len()..];
            // Only treat it as a topic suffix when something follows the
            // marker (avoid mangling names that merely end in "-topic-").
            if !suffix.is_empty() {
                return &stem[..idx];
            }
        }
    }
    stem
}

/// Canonical form of a lock path (strip `.lock`, canonicalize the transcript
/// identity, re-append `.lock`).
pub fn canonical_lock_path(lock_path: &Path) -> PathBuf {
    let Some(name) = lock_path.file_name().and_then(|f| f.to_str()) else {
        return lock_path.to_path_buf();
    };
    let Some(transcript_name) = name.strip_suffix(LOCK_SUFFIX) else {
        return lock_path.to_path_buf();
    };
    let transcript = lock_path.with_file_name(transcript_name);
    lock_path_for_transcript(&canonical_transcript_path(&transcript))
}

/// Whether `lock_path` belongs to any of the cleaned transcripts.
///
/// Matches by exact lock path first, then falls back to canonical-session
/// identity so locks left by topic-suffixed transcripts are cleaned when the
/// canonical session's transcripts are removed — letting topic-suffixed
/// sessions resume after a restart instead of wedging on an orphaned lock.
pub fn matches_cleaned_lock(lock_path: &Path, cleaned_transcripts: &[PathBuf]) -> bool {
    // Exact lock-path match.
    if cleaned_transcripts
        .iter()
        .any(|t| lock_path_for_transcript(t) == lock_path)
    {
        return true;
    }
    // Canonical session fallback.
    let canonical = canonical_lock_path(lock_path);
    cleaned_transcripts
        .iter()
        .any(|t| lock_path_for_transcript(&canonical_transcript_path(t)) == canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(acquire_ms: u64, max_hold_ms: u64) -> WriteLockSettings {
        WriteLockSettings {
            acquire_timeout: Duration::from_millis(acquire_ms),
            max_hold: Duration::from_millis(max_hold_ms),
        }
    }

    // ------------------------------------------------------------------
    // Settings / config resolution
    // ------------------------------------------------------------------

    #[test]
    fn settings_default_to_60s_acquire_timeout() {
        let s = WriteLockSettings::from_config(None);
        assert_eq!(s.acquire_timeout, Duration::from_millis(60_000));
        assert_eq!(s.max_hold, Duration::from_millis(300_000));
    }

    #[test]
    fn settings_honor_configured_values() {
        let cfg = SessionWriteLockConfig {
            acquire_timeout_ms: Some(1_500),
            max_hold_ms: Some(10_000),
        };
        let s = WriteLockSettings::from_config(Some(&cfg));
        assert_eq!(s.acquire_timeout, Duration::from_millis(1_500));
        assert_eq!(s.max_hold, Duration::from_millis(10_000));
    }

    #[test]
    fn settings_clamp_zero_to_defaults() {
        let cfg = SessionWriteLockConfig {
            acquire_timeout_ms: Some(0),
            max_hold_ms: Some(0),
        };
        let s = WriteLockSettings::from_config(Some(&cfg));
        assert_eq!(s, WriteLockSettings::default());
    }

    // ------------------------------------------------------------------
    // Acquire timeout behavior (tokio virtual time)
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn uncontended_acquire_succeeds_immediately() {
        let locks = SessionWriteLocks::new(WriteLockSettings::default());
        let guard = locks.acquire("sess-a", "writer-1").await.unwrap();
        drop(guard);
        // Re-acquire after release works.
        let _guard = locks.acquire("sess-a", "writer-2").await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_times_out_after_configured_window() {
        let locks = Arc::new(SessionWriteLocks::new(settings(60_000, 300_000)));
        let _held = locks.acquire("sess-a", "holder").await.unwrap();

        let started = Instant::now();
        let err = locks.acquire("sess-a", "waiter").await.unwrap_err();
        let waited = started.elapsed();

        assert!(
            waited >= Duration::from_millis(60_000),
            "must wait the full acquire timeout (waited {waited:?})"
        );
        assert_eq!(err.session_key, "sess-a");
        assert_eq!(err.holder.as_deref(), Some("holder"));
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_gets_lock_when_holder_releases_within_timeout() {
        let locks = Arc::new(SessionWriteLocks::new(settings(60_000, 300_000)));
        let held = locks.acquire("sess-a", "holder").await.unwrap();

        let locks2 = locks.clone();
        let waiter = tokio::spawn(async move { locks2.acquire("sess-a", "waiter").await });

        tokio::time::sleep(Duration::from_millis(5_000)).await;
        drop(held);

        let guard = waiter.await.unwrap().expect("waiter should acquire");
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn independent_session_keys_do_not_contend() {
        let locks = SessionWriteLocks::new(settings(1_000, 300_000));
        let _a = locks.acquire("sess-a", "w").await.unwrap();
        let _b = locks.acquire("sess-b", "w").await.unwrap();
    }

    // ------------------------------------------------------------------
    // Max-hold reclaim + stale reporting
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn stale_holder_is_reclaimed_at_acquisition() {
        let locks = SessionWriteLocks::new(settings(60_000, 300_000));
        let stale = locks.acquire("sess-a", "wedged").await.unwrap();

        // Holder exceeds max-hold.
        tokio::time::sleep(Duration::from_millis(300_001)).await;

        // New acquirer reclaims immediately (no 60s wait).
        let started = Instant::now();
        let guard = locks.acquire("sess-a", "fresh").await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1_000),
            "reclaim must happen at acquisition, not after the acquire timeout"
        );

        let reports = locks.take_stale_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].session_key, "sess-a");
        assert_eq!(reports[0].holder.as_deref(), Some("wedged"));
        assert!(reports[0].held_ms >= 300_000);

        // Dropping the stale guard afterwards must not double-release: the
        // next acquirer still has to wait for the fresh guard.
        drop(stale);
        drop(guard);
        let _next = locks.acquire("sess-a", "next").await.unwrap();
        assert!(locks.take_stale_reports().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_guard_drop_after_reclaim_does_not_leak_permit() {
        let locks = Arc::new(SessionWriteLocks::new(settings(2_000, 10_000)));
        let stale = locks.acquire("sess-a", "wedged").await.unwrap();
        tokio::time::sleep(Duration::from_millis(10_001)).await;

        let fresh = locks.acquire("sess-a", "fresh").await.unwrap();
        drop(stale); // permit must be forgotten, not released

        // While "fresh" holds the lock, another acquire must time out.
        let err = locks.acquire("sess-a", "waiter").await.unwrap_err();
        assert_eq!(err.holder.as_deref(), Some("fresh"));
        drop(fresh);
        let _ok = locks.acquire("sess-a", "after").await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_holder_is_not_reclaimed() {
        let locks = SessionWriteLocks::new(settings(1_000, 300_000));
        let _held = locks.acquire("sess-a", "holder").await.unwrap();
        tokio::time::sleep(Duration::from_millis(5_000)).await; // < max hold

        let err = locks.acquire("sess-a", "waiter").await.unwrap_err();
        assert_eq!(err.holder.as_deref(), Some("holder"));
        assert!(locks.take_stale_reports().is_empty());
    }

    // ------------------------------------------------------------------
    // Guard release on drop (error paths)
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn guard_released_when_holding_task_aborts() {
        let locks = Arc::new(SessionWriteLocks::new(settings(60_000, 300_000)));
        let locks2 = locks.clone();
        let holder = tokio::spawn(async move {
            let _guard = locks2.acquire("sess-a", "doomed").await.unwrap();
            // Simulate a wedged fence read.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        tokio::task::yield_now().await;
        holder.abort();
        let _ = holder.await;

        // Teardown released the lock.
        let _guard = locks.acquire("sess-a", "next").await.unwrap();
    }

    // ------------------------------------------------------------------
    // Lock-file identity / cleanup matching
    // ------------------------------------------------------------------

    #[test]
    fn lock_path_appends_lock_suffix() {
        assert_eq!(
            lock_path_for_transcript(Path::new("/s/abc.jsonl")),
            PathBuf::from("/s/abc.jsonl.lock")
        );
    }

    #[test]
    fn canonical_transcript_strips_topic_suffix() {
        assert_eq!(
            canonical_transcript_path(Path::new("/s/sess-1.topic-42.jsonl")),
            PathBuf::from("/s/sess-1.jsonl")
        );
        assert_eq!(
            canonical_transcript_path(Path::new("/s/sess-1-topic-42.jsonl")),
            PathBuf::from("/s/sess-1.jsonl")
        );
        assert_eq!(
            canonical_transcript_path(Path::new("/s/sess-1.jsonl")),
            PathBuf::from("/s/sess-1.jsonl")
        );
    }

    #[test]
    fn matches_cleaned_lock_by_exact_path() {
        let cleaned = vec![PathBuf::from("/s/sess-1.jsonl")];
        assert!(matches_cleaned_lock(
            Path::new("/s/sess-1.jsonl.lock"),
            &cleaned
        ));
        assert!(!matches_cleaned_lock(
            Path::new("/s/other.jsonl.lock"),
            &cleaned
        ));
    }

    #[test]
    fn matches_cleaned_lock_by_canonical_session_fallback() {
        // Topic-suffixed transcript cleaned → its canonical session lock
        // matches, so the session can resume after restart.
        let cleaned = vec![PathBuf::from("/s/sess-1.topic-42.jsonl")];
        assert!(matches_cleaned_lock(
            Path::new("/s/sess-1.jsonl.lock"),
            &cleaned
        ));
        // And the reverse: canonical transcript cleaned matches a
        // topic-suffixed lock left behind.
        let cleaned = vec![PathBuf::from("/s/sess-1.jsonl")];
        assert!(matches_cleaned_lock(
            Path::new("/s/sess-1.topic-7.jsonl.lock"),
            &cleaned
        ));
    }

    #[test]
    fn unrelated_sessions_never_match_via_canonical_fallback() {
        let cleaned = vec![PathBuf::from("/s/sess-2.topic-42.jsonl")];
        assert!(!matches_cleaned_lock(
            Path::new("/s/sess-1.jsonl.lock"),
            &cleaned
        ));
    }
}
