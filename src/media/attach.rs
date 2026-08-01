//! `MEDIA:` attachment reference parsing (v2026.7.1 parity).
//!
//! Ports the local-path subset of upstream `src/media/media-reference.ts`:
//! normalizes the `MEDIA:` prefix, classifies the source scheme, and
//! resolves local file paths — including **home-relative `MEDIA:~/...`
//! paths** — under the existing file-read policy (no `..` escapes, no
//! unsupported schemes, no null bytes).

use std::path::{Path, PathBuf};

/// Strip a leading `MEDIA:` marker (case-insensitive, tolerant of interior
/// whitespace) from an attachment reference.
pub fn normalize_media_reference_source(source: &str) -> String {
    let trimmed = source.trim();
    let lower = trimmed.to_ascii_lowercase();
    let Some(idx) = lower.find("media") else {
        return trimmed.to_string();
    };
    // Only treat it as a marker when it is the leading token followed by ':'.
    if lower[..idx].trim().is_empty() {
        let after = &trimmed[idx + "media".len()..];
        let after_trim = after.trim_start();
        if let Some(rest) = after_trim.strip_prefix(':') {
            // `media://` is the media-store scheme, not the MEDIA: marker.
            if !after_trim.starts_with("://") {
                return rest.trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Classification of a media reference source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSourceKind {
    /// `http://` / `https://`.
    HttpUrl,
    /// `data:` URL.
    DataUrl,
    /// `file:` URL.
    FileUrl,
    /// `media://` store URI.
    MediaStoreUrl,
    /// Local path: absolute, home-relative (`~/...`), or Windows drive path.
    LocalPath,
    /// Anything else with a scheme (`ftp:`, `gopher:`, ...).
    UnsupportedScheme,
    /// Relative path (resolved against a base directory by the caller).
    Relative,
}

/// Classify a normalized media source string.
pub fn classify_media_source(source: &str) -> MediaSourceKind {
    let looks_like_windows_drive = source.len() >= 3
        && source.as_bytes()[0].is_ascii_alphabetic()
        && source.as_bytes()[1] == b':'
        && (source.as_bytes()[2] == b'\\' || source.as_bytes()[2] == b'/');
    if looks_like_windows_drive {
        return MediaSourceKind::LocalPath;
    }
    let lower = source.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return MediaSourceKind::HttpUrl;
    }
    if lower.starts_with("data:") {
        return MediaSourceKind::DataUrl;
    }
    if lower.starts_with("file:") {
        return MediaSourceKind::FileUrl;
    }
    if lower.starts_with("media://") {
        return MediaSourceKind::MediaStoreUrl;
    }
    let has_scheme = source
        .split_once(':')
        .map(|(scheme, _)| {
            !scheme.is_empty()
                && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
        })
        .unwrap_or(false);
    if has_scheme {
        return MediaSourceKind::UnsupportedScheme;
    }
    if source.starts_with('~') || Path::new(source).is_absolute() {
        return MediaSourceKind::LocalPath;
    }
    MediaSourceKind::Relative
}

/// Expand a home-relative path (`~` or `~/...`) against the user's home
/// directory. `~user/...` forms are rejected (no passwd lookups).
pub fn expand_home_path(source: &str, home: &Path) -> Option<PathBuf> {
    if source == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = source.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    if let Some(rest) = source.strip_prefix("~\\") {
        return Some(home.join(rest));
    }
    None
}

/// Errors from local media path resolution.
#[derive(Debug, PartialEq, Eq)]
pub enum MediaPathError {
    /// The source uses a scheme media attachments cannot read.
    UnsupportedScheme,
    /// The path escapes the read policy (null bytes, `~user`, `..` escape).
    PathNotAllowed,
}

/// Resolve a normalized media source into a local filesystem path under the
/// existing file-read policy.
///
/// Accepts absolute paths and home-relative `~/...` paths (v2026.7.1: the
/// upstream media pipeline resolves `MEDIA:~/...` via `resolveUserPath`
/// before file-read checks). Relative paths resolve against `base_dir` and
/// must not escape it via `..`. Returns `Ok(None)` for non-local sources
/// (http/data/media-store URLs) which the caller handles separately.
pub fn resolve_local_media_path(
    source: &str,
    home: &Path,
    base_dir: Option<&Path>,
) -> Result<Option<PathBuf>, MediaPathError> {
    if source.contains('\0') {
        return Err(MediaPathError::PathNotAllowed);
    }
    match classify_media_source(source) {
        MediaSourceKind::HttpUrl | MediaSourceKind::DataUrl | MediaSourceKind::MediaStoreUrl => {
            Ok(None)
        }
        MediaSourceKind::UnsupportedScheme => Err(MediaPathError::UnsupportedScheme),
        MediaSourceKind::FileUrl => {
            let stripped = source
                .strip_prefix("file://")
                .or_else(|| source.strip_prefix("FILE://"))
                .ok_or(MediaPathError::UnsupportedScheme)?;
            if stripped.is_empty() || !stripped.starts_with('/') {
                return Err(MediaPathError::PathNotAllowed);
            }
            Ok(Some(PathBuf::from(stripped)))
        }
        MediaSourceKind::LocalPath => {
            if source.starts_with('~') {
                match expand_home_path(source, home) {
                    Some(path) => Ok(Some(path)),
                    // `~user/...` (or bare `~xyz`) is not allowed.
                    None => Err(MediaPathError::PathNotAllowed),
                }
            } else {
                Ok(Some(PathBuf::from(source)))
            }
        }
        MediaSourceKind::Relative => {
            let Some(base) = base_dir else {
                return Err(MediaPathError::PathNotAllowed);
            };
            if relative_path_escapes_base(source) {
                return Err(MediaPathError::PathNotAllowed);
            }
            Ok(Some(base.join(source)))
        }
    }
}

/// True when a relative path would escape its base directory.
pub fn relative_path_escapes_base(relative: &str) -> bool {
    if relative == ".." {
        return true;
    }
    if relative.starts_with("../") || relative.starts_with("..\\") {
        return true;
    }
    // Interior `..` components also escape after normalization when they
    // outnumber preceding components.
    let mut depth: i32 = 0;
    for component in relative.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_prefix_normalization() {
        assert_eq!(normalize_media_reference_source("MEDIA:/tmp/a.png"), "/tmp/a.png");
        assert_eq!(normalize_media_reference_source("media: ~/pics/a.png"), "~/pics/a.png");
        assert_eq!(normalize_media_reference_source("  MEDIA :  /x.png"), "/x.png");
        // media:// is the store scheme, not the marker.
        assert_eq!(
            normalize_media_reference_source("media://inbound/abc"),
            "media://inbound/abc"
        );
        assert_eq!(normalize_media_reference_source("/plain/path.png"), "/plain/path.png");
    }

    #[test]
    fn classification() {
        assert_eq!(classify_media_source("https://x.example/a.png"), MediaSourceKind::HttpUrl);
        assert_eq!(classify_media_source("data:image/png;base64,xxx"), MediaSourceKind::DataUrl);
        assert_eq!(classify_media_source("file:///tmp/a.png"), MediaSourceKind::FileUrl);
        assert_eq!(classify_media_source("media://inbound/abc"), MediaSourceKind::MediaStoreUrl);
        assert_eq!(classify_media_source("~/pics/a.png"), MediaSourceKind::LocalPath);
        assert_eq!(classify_media_source("/abs/a.png"), MediaSourceKind::LocalPath);
        assert_eq!(classify_media_source("C:\\pics\\a.png"), MediaSourceKind::LocalPath);
        assert_eq!(classify_media_source("ftp://host/a.png"), MediaSourceKind::UnsupportedScheme);
        assert_eq!(classify_media_source("rel/a.png"), MediaSourceKind::Relative);
    }

    #[test]
    fn home_relative_media_paths_are_accepted() {
        let home = Path::new("/home/tester");
        let resolved = resolve_local_media_path("~/pics/cat.png", home, None)
            .unwrap()
            .unwrap();
        assert_eq!(resolved, PathBuf::from("/home/tester/pics/cat.png"));
        // Bare `~` resolves to home itself.
        let resolved = resolve_local_media_path("~", home, None).unwrap().unwrap();
        assert_eq!(resolved, PathBuf::from("/home/tester"));
    }

    #[test]
    fn full_media_marker_with_home_path_round_trips() {
        let home = Path::new("/home/tester");
        let source = normalize_media_reference_source("MEDIA:~/voice/note.ogg");
        let resolved = resolve_local_media_path(&source, home, None).unwrap().unwrap();
        assert_eq!(resolved, PathBuf::from("/home/tester/voice/note.ogg"));
    }

    #[test]
    fn tilde_user_forms_are_rejected() {
        let home = Path::new("/home/tester");
        assert_eq!(
            resolve_local_media_path("~root/secret", home, None),
            Err(MediaPathError::PathNotAllowed)
        );
    }

    #[test]
    fn unsupported_schemes_are_rejected() {
        let home = Path::new("/home/tester");
        assert_eq!(
            resolve_local_media_path("ftp://host/a.png", home, None),
            Err(MediaPathError::UnsupportedScheme)
        );
        assert_eq!(
            resolve_local_media_path("gopher://host/a", home, None),
            Err(MediaPathError::UnsupportedScheme)
        );
    }

    #[test]
    fn remote_sources_resolve_to_none() {
        let home = Path::new("/home/tester");
        assert_eq!(resolve_local_media_path("https://x/a.png", home, None), Ok(None));
        assert_eq!(resolve_local_media_path("data:image/png;base64,x", home, None), Ok(None));
        assert_eq!(resolve_local_media_path("media://inbound/abc", home, None), Ok(None));
    }

    #[test]
    fn relative_paths_stay_inside_base() {
        let home = Path::new("/home/tester");
        let base = Path::new("/workspace");
        assert_eq!(
            resolve_local_media_path("sub/a.png", home, Some(base)).unwrap().unwrap(),
            PathBuf::from("/workspace/sub/a.png")
        );
        assert_eq!(
            resolve_local_media_path("../etc/passwd", home, Some(base)),
            Err(MediaPathError::PathNotAllowed)
        );
        assert_eq!(
            resolve_local_media_path("a/../../etc", home, Some(base)),
            Err(MediaPathError::PathNotAllowed)
        );
        // No base dir → relative paths are not allowed.
        assert_eq!(
            resolve_local_media_path("rel.png", home, None),
            Err(MediaPathError::PathNotAllowed)
        );
    }

    #[test]
    fn null_bytes_are_rejected() {
        let home = Path::new("/home/tester");
        assert_eq!(
            resolve_local_media_path("/tmp/a\0.png", home, None),
            Err(MediaPathError::PathNotAllowed)
        );
    }

    #[test]
    fn escape_detection() {
        assert!(relative_path_escapes_base(".."));
        assert!(relative_path_escapes_base("../x"));
        assert!(relative_path_escapes_base("a/../../x"));
        assert!(!relative_path_escapes_base("a/b"));
        assert!(!relative_path_escapes_base("a/../b"));
        assert!(!relative_path_escapes_base("./a"));
    }
}
