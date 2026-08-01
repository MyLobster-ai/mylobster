//! Dedicated in-process session writer (v2026.5.2 parity).
//!
//! Upstream routes all session-store mutations — Gateway transcript writes,
//! CLI cleanup, and agent-delete purges — through one dedicated in-process
//! writer so they serialize instead of racing each other (and instead of the
//! CLI mutating store files behind a live gateway's back).

use crate::sessions::{lock, transcript};

use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

/// Outcome of a cleanup pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub removed_transcripts: usize,
    pub removed_locks: usize,
}

/// Outcome of an agent purge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub removed_files: usize,
}

enum WriterCommand {
    Append {
        path: PathBuf,
        entry: Value,
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    AppendParentLinked {
        parent_path: PathBuf,
        child_path: PathBuf,
        parent_entry: Value,
        child_entry: Value,
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    Cleanup {
        transcripts: Vec<PathBuf>,
        lock_dir: Option<PathBuf>,
        ack: oneshot::Sender<std::io::Result<CleanupOutcome>>,
    },
    PurgeAgent {
        sessions_root: PathBuf,
        agent_id: String,
        ack: oneshot::Sender<std::io::Result<PurgeOutcome>>,
    },
}

/// Handle to the dedicated session writer task. Cheap to clone; all commands
/// funnel into one queue and execute strictly in order.
#[derive(Clone)]
pub struct SessionWriterHandle {
    tx: mpsc::UnboundedSender<WriterCommand>,
}

fn closed_err<T>() -> std::io::Result<T> {
    Err(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "session writer task is gone",
    ))
}

impl SessionWriterHandle {
    /// Spawn the dedicated writer task. The task exits when every handle is
    /// dropped.
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<WriterCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    WriterCommand::Append { path, entry, ack } => {
                        let _ = ack.send(transcript::append_entry(&path, &entry).await);
                    }
                    WriterCommand::AppendParentLinked {
                        parent_path,
                        child_path,
                        parent_entry,
                        child_entry,
                        ack,
                    } => {
                        let _ = ack.send(
                            transcript::append_parent_linked(
                                &parent_path,
                                &child_path,
                                &parent_entry,
                                &child_entry,
                            )
                            .await,
                        );
                    }
                    WriterCommand::Cleanup {
                        transcripts,
                        lock_dir,
                        ack,
                    } => {
                        let _ = ack.send(run_cleanup(&transcripts, lock_dir.as_deref()).await);
                    }
                    WriterCommand::PurgeAgent {
                        sessions_root,
                        agent_id,
                        ack,
                    } => {
                        let _ = ack.send(run_purge_agent(&sessions_root, &agent_id).await);
                    }
                }
            }
        });
        Self { tx }
    }

    /// Gateway transcript write, routed through the dedicated writer.
    pub async fn append(&self, path: PathBuf, entry: Value) -> std::io::Result<()> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(WriterCommand::Append { path, entry, ack })
            .is_err()
        {
            return closed_err();
        }
        rx.await.unwrap_or_else(|_| closed_err())
    }

    /// Parent-linked gateway write (child entry + parent link), serialized
    /// with everything else in the writer queue.
    pub async fn append_parent_linked(
        &self,
        parent_path: PathBuf,
        child_path: PathBuf,
        parent_entry: Value,
        child_entry: Value,
    ) -> std::io::Result<()> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(WriterCommand::AppendParentLinked {
                parent_path,
                child_path,
                parent_entry,
                child_entry,
                ack,
            })
            .is_err()
        {
            return closed_err();
        }
        rx.await.unwrap_or_else(|_| closed_err())
    }

    /// CLI cleanup: remove the given transcripts and any lock files that
    /// belong to them (exact lock-path match plus canonical session
    /// fallback). `lock_dir` defaults to each transcript's directory.
    pub async fn cleanup(
        &self,
        transcripts: Vec<PathBuf>,
        lock_dir: Option<PathBuf>,
    ) -> std::io::Result<CleanupOutcome> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(WriterCommand::Cleanup {
                transcripts,
                lock_dir,
                ack,
            })
            .is_err()
        {
            return closed_err();
        }
        rx.await.unwrap_or_else(|_| closed_err())
    }

    /// Agent-delete purge: remove every session file for `agent_id` under
    /// `sessions_root`, serialized behind in-flight writes.
    pub async fn purge_agent(
        &self,
        sessions_root: PathBuf,
        agent_id: String,
    ) -> std::io::Result<PurgeOutcome> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(WriterCommand::PurgeAgent {
                sessions_root,
                agent_id,
                ack,
            })
            .is_err()
        {
            return closed_err();
        }
        rx.await.unwrap_or_else(|_| closed_err())
    }
}

async fn run_cleanup(
    transcripts: &[PathBuf],
    lock_dir: Option<&Path>,
) -> std::io::Result<CleanupOutcome> {
    let mut outcome = CleanupOutcome::default();
    let mut lock_dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = lock_dir {
        lock_dirs.push(dir.to_path_buf());
    }

    for transcript in transcripts {
        match tokio::fs::remove_file(transcript).await {
            Ok(()) => outcome.removed_transcripts += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if lock_dir.is_none() {
            if let Some(parent) = transcript.parent() {
                let parent = parent.to_path_buf();
                if !lock_dirs.contains(&parent) {
                    lock_dirs.push(parent);
                }
            }
        }
    }

    // Sweep lock files belonging to the cleaned transcripts: exact lock-path
    // matches plus the canonical-session fallback so topic-suffixed
    // transcripts resume after restart.
    for dir in lock_dirs {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(dirent) = entries.next_entry().await? {
            let path = dirent.path();
            let is_lock = path
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.ends_with(".lock"));
            if !is_lock {
                continue;
            }
            if lock::matches_cleaned_lock(&path, transcripts) {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => outcome.removed_locks += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(outcome)
}

async fn run_purge_agent(sessions_root: &Path, agent_id: &str) -> std::io::Result<PurgeOutcome> {
    // Agent session files live under `<root>/<agent_id>/`.
    let agent_dir = sessions_root.join(agent_id);
    let mut outcome = PurgeOutcome::default();
    let mut entries = match tokio::fs::read_dir(&agent_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(e),
    };
    while let Some(dirent) = entries.next_entry().await? {
        if dirent.file_type().await?.is_file() {
            outcome.removed_files += 1;
        }
    }
    drop(entries);
    tokio::fs::remove_dir_all(&agent_dir).await?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn writer_serializes_concurrent_gateway_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let writer = SessionWriterHandle::spawn();

        let mut handles = Vec::new();
        for i in 0..24usize {
            let writer = writer.clone();
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                writer.append(path, json!({ "seq": i })).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let read =
            transcript::read_transcript_head(&path, transcript::TranscriptReadOptions::default())
                .await
                .unwrap();
        assert_eq!(read.entries.len(), 24);
        assert_eq!(read.malformed_lines, 0);
    }

    #[tokio::test]
    async fn cleanup_removes_transcripts_and_exact_locks() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("sess-1.jsonl");
        let lock_file = dir.path().join("sess-1.jsonl.lock");
        let unrelated_lock = dir.path().join("sess-2.jsonl.lock");
        tokio::fs::write(&t, "{}\n").await.unwrap();
        tokio::fs::write(&lock_file, "pid:1").await.unwrap();
        tokio::fs::write(&unrelated_lock, "pid:2").await.unwrap();

        let writer = SessionWriterHandle::spawn();
        let outcome = writer.cleanup(vec![t.clone()], None).await.unwrap();

        assert_eq!(outcome.removed_transcripts, 1);
        assert_eq!(outcome.removed_locks, 1);
        assert!(!t.exists());
        assert!(!lock_file.exists());
        assert!(unrelated_lock.exists(), "unrelated locks stay untouched");
    }

    #[tokio::test]
    async fn cleanup_matches_topic_suffixed_locks_via_canonical_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("sess-1.jsonl");
        let topic_lock = dir.path().join("sess-1.topic-42.jsonl.lock");
        tokio::fs::write(&canonical, "{}\n").await.unwrap();
        tokio::fs::write(&topic_lock, "pid:1").await.unwrap();

        let writer = SessionWriterHandle::spawn();
        let outcome = writer.cleanup(vec![canonical.clone()], None).await.unwrap();

        assert_eq!(outcome.removed_locks, 1);
        assert!(!topic_lock.exists(), "topic-suffixed lock cleaned so the session resumes");
    }

    #[tokio::test]
    async fn cleanup_of_missing_transcript_is_benign() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SessionWriterHandle::spawn();
        let outcome = writer
            .cleanup(vec![dir.path().join("nope.jsonl")], None)
            .await
            .unwrap();
        assert_eq!(outcome, CleanupOutcome::default());
    }

    #[tokio::test]
    async fn purge_agent_removes_only_that_agents_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("agent-a");
        let b = dir.path().join("agent-b");
        tokio::fs::create_dir_all(&a).await.unwrap();
        tokio::fs::create_dir_all(&b).await.unwrap();
        tokio::fs::write(a.join("s1.jsonl"), "{}\n").await.unwrap();
        tokio::fs::write(a.join("s1.jsonl.lock"), "pid").await.unwrap();
        tokio::fs::write(b.join("s2.jsonl"), "{}\n").await.unwrap();

        let writer = SessionWriterHandle::spawn();
        let outcome = writer
            .purge_agent(dir.path().to_path_buf(), "agent-a".to_string())
            .await
            .unwrap();

        assert_eq!(outcome.removed_files, 2);
        assert!(!a.exists());
        assert!(b.join("s2.jsonl").exists());
    }

    #[tokio::test]
    async fn purge_of_unknown_agent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SessionWriterHandle::spawn();
        let outcome = writer
            .purge_agent(dir.path().to_path_buf(), "ghost".to_string())
            .await
            .unwrap();
        assert_eq!(outcome, PurgeOutcome::default());
    }
}
