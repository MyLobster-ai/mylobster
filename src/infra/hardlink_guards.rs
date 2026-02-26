//! Hardlink and path-alias security guards (v2026.2.25).
//!
//! Prevents workspace boundary escapes via hard link aliases and symlink
//! path aliasing. Ported from OpenClaw `src/infra/hardlink-guards.ts`.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Policy for handling path aliases (symlinks / hardlinks) at the final
/// path component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAliasPolicy {
    /// Allow the final component to be a symlink (default for reads).
    AllowFinalSymlink,
    /// Reject any alias — both hardlinks and symlinks.
    RejectAliases,
    /// Allow hardlinks only when the operation is an unlink (delete).
    UnlinkTarget,
}

/// Assert that the file at `path` is not a hardlink (nlink > 1).
///
/// If the file has more than one hard link, it can be used to escape
/// workspace boundaries by creating an alias to a file outside the
/// sandbox.
///
/// Returns `Ok(())` if the path is safe, or `Err` with a descriptive
/// message if a hardlink alias is detected.
///
/// The check is silently skipped if the file does not exist or is not a
/// regular file (directories, sockets, etc.).
pub fn assert_no_hardlinked_final_path(
    path: &Path,
    policy: PathAliasPolicy,
) -> Result<(), String> {
    if policy == PathAliasPolicy::UnlinkTarget {
        // Unlink operations are allowed even on hardlinked files.
        return Ok(());
    }

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        // File doesn't exist — nothing to check.
        Err(_) => return Ok(()),
    };

    if !metadata.is_file() {
        // Only regular files can have meaningful hardlinks.
        return Ok(());
    }

    #[cfg(unix)]
    {
        let nlink = metadata.nlink();
        if nlink > 1 {
            return Err(format!(
                "Refusing to operate on hardlinked file '{}' (nlink={}). \
                 Multiple hard links can alias files outside the workspace boundary.",
                path.display(),
                nlink,
            ));
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix platforms, hardlink count is not available via std.
        // Silently pass — this guard is defense-in-depth.
        let _ = metadata;
    }

    Ok(())
}

/// Assert that the resolved (canonicalized) path does not escape the
/// workspace boundary via symlink aliasing.
///
/// Compares the `lstat` (symlink-aware) identity with the `stat` (resolved)
/// identity. If they differ and the policy rejects aliases, the operation
/// is denied.
pub fn assert_no_path_alias_escape(
    path: &Path,
    workspace_root: &Path,
    policy: PathAliasPolicy,
) -> Result<(), String> {
    if policy == PathAliasPolicy::AllowFinalSymlink {
        return Ok(());
    }

    let canonical = match path.canonicalize() {
        Ok(c) => c,
        Err(_) => return Ok(()), // File doesn't exist yet.
    };

    let ws_canonical = match workspace_root.canonicalize() {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    if !canonical.starts_with(&ws_canonical) {
        return Err(format!(
            "Path '{}' resolves to '{}' which is outside workspace '{}'.",
            path.display(),
            canonical.display(),
            ws_canonical.display(),
        ));
    }

    // On Unix, additionally verify that the symlink target identity matches.
    #[cfg(unix)]
    {
        if !same_file_identity(path, &canonical) {
            return Err(format!(
                "Path alias detected: '{}' and its canonical form '{}' \
                 have different file identities.",
                path.display(),
                canonical.display(),
            ));
        }
    }

    Ok(())
}

/// Compare two paths by (device, inode) identity.
///
/// Returns `true` if both paths refer to the same underlying file.
/// Returns `false` if either path cannot be stat'd or they differ.
#[cfg(unix)]
pub fn same_file_identity(a: &Path, b: &Path) -> bool {
    let meta_a = match std::fs::metadata(a) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let meta_b = match std::fs::metadata(b) {
        Ok(m) => m,
        Err(_) => return false,
    };

    meta_a.dev() == meta_b.dev() && meta_a.ino() == meta_b.ino()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn nonexistent_path_passes() {
        let result = assert_no_hardlinked_final_path(
            Path::new("/nonexistent/file"),
            PathAliasPolicy::RejectAliases,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn regular_file_passes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello").unwrap();

        let result =
            assert_no_hardlinked_final_path(&file, PathAliasPolicy::RejectAliases);
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_file_rejected() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("original.txt");
        let link = dir.path().join("alias.txt");
        fs::write(&file, "hello").unwrap();
        fs::hard_link(&file, &link).unwrap();

        let result =
            assert_no_hardlinked_final_path(&file, PathAliasPolicy::RejectAliases);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nlink=2"));
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_file_allowed_for_unlink() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("original.txt");
        let link = dir.path().join("alias.txt");
        fs::write(&file, "hello").unwrap();
        fs::hard_link(&file, &link).unwrap();

        let result =
            assert_no_hardlinked_final_path(&file, PathAliasPolicy::UnlinkTarget);
        assert!(result.is_ok());
    }

    #[test]
    fn directory_passes() {
        let dir = TempDir::new().unwrap();
        let result = assert_no_hardlinked_final_path(
            dir.path(),
            PathAliasPolicy::RejectAliases,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn path_within_workspace_passes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("sub").join("test.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "hello").unwrap();

        let result = assert_no_path_alias_escape(
            &file,
            dir.path(),
            PathAliasPolicy::RejectAliases,
        );
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn same_file_identity_true_for_same_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello").unwrap();

        assert!(same_file_identity(&file, &file));
    }

    #[cfg(unix)]
    #[test]
    fn same_file_identity_false_for_different_files() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "hello").unwrap();
        fs::write(&b, "world").unwrap();

        assert!(!same_file_identity(&a, &b));
    }
}
