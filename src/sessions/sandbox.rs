//! Sandbox-safe atomic file replacement (v2026.5.2 parity).
//!
//! Upstream fix: sandbox workspace edits performed via temp-file + rename were
//! collapsing existing file modes to the tempfile default (0600). Atomic
//! replacement must preserve the mode already on the destination file, and
//! newly created files default to 0644 rather than the private temp mode.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Mode applied to files that did not previously exist (Unix only).
pub const DEFAULT_NEW_FILE_MODE: u32 = 0o644;

/// Atomically replace `path` with `contents`, preserving the existing file
/// mode when the destination already exists (0644 for new files — never the
/// 0600 tempfile default).
pub fn atomic_replace_preserving_mode(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = parent_dir(path);
    std::fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(contents)?;
    tmp.flush()?;
    persist_preserving_mode(tmp, path)
}

/// Persist an already-written temp file over `dest`, preserving `dest`'s
/// existing mode (or applying [`DEFAULT_NEW_FILE_MODE`] when `dest` is new).
///
/// Shared by [`atomic_replace_preserving_mode`] and the streaming transcript
/// rewrite path in [`crate::sessions::transcript`].
pub fn persist_preserving_mode(
    tmp: tempfile::NamedTempFile,
    dest: &Path,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = existing_file_mode(dest).unwrap_or(DEFAULT_NEW_FILE_MODE);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
    }
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

/// The mode bits of an existing file, if it exists (Unix only).
pub fn existing_file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.permissions().mode() & 0o7777);
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn preserves_existing_0644_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"old").unwrap();
        set_mode(&path, 0o644);

        atomic_replace_preserving_mode(&path, b"new contents").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new contents");
        assert_eq!(mode_of(&path), 0o644, "0644 must not collapse to 0600");
    }

    #[test]
    #[cfg(unix)]
    fn preserves_existing_custom_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        set_mode(&path, 0o755);

        atomic_replace_preserving_mode(&path, b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(mode_of(&path), 0o755);
    }

    #[test]
    #[cfg(unix)]
    fn preserves_private_0600_when_already_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"s").unwrap();
        set_mode(&path, 0o600);

        atomic_replace_preserving_mode(&path, b"s2").unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn new_files_get_default_0644_not_tempfile_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brand-new.txt");

        atomic_replace_preserving_mode(&path, b"hello").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(mode_of(&path), DEFAULT_NEW_FILE_MODE);
    }

    #[test]
    fn replace_is_atomic_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deep/file.jsonl");
        atomic_replace_preserving_mode(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }
}
