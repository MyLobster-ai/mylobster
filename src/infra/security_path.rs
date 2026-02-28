//! Security path canonicalization (v2026.2.26).
//!
//! Multi-pass URI decoding to prevent path traversal attacks via encoded
//! sequences. Ported from OpenClaw `src/infra/security-path.ts`.
//!
//! The core defense is iterative decoding: we decode the URI repeatedly until
//! the output stabilizes, detecting double-encoded traversal sequences like
//! `%252e%252e%252f` → `%2e%2e%2f` → `../`.

use std::borrow::Cow;

/// Maximum number of decode passes before we consider the input malicious.
const MAX_DECODE_PASSES: usize = 10;

/// Protected path prefixes that require gateway authentication.
const PROTECTED_PREFIXES: &[&str] = &[
    "/api/channels",
    "/api/plugins",
    "/api/hooks",
];

/// Result of path canonicalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalizationResult {
    /// Path is safe to use after canonicalization.
    Ok(String),
    /// Path contains malformed percent-encoding — reject (fail-closed).
    MalformedEncoding(String),
    /// Path did not stabilize within MAX_DECODE_PASSES — likely attack.
    UnstableEncoding,
    /// Path contains null bytes after decoding — reject.
    NullByte,
}

/// Decode a single percent-encoded byte (e.g., `%2F` → `/`).
/// Returns `None` if the encoding is malformed.
fn decode_percent_byte(hi: u8, lo: u8) -> Option<u8> {
    let h = match hi {
        b'0'..=b'9' => hi - b'0',
        b'A'..=b'F' => hi - b'A' + 10,
        b'a'..=b'f' => hi - b'a' + 10,
        _ => return None,
    };
    let l = match lo {
        b'0'..=b'9' => lo - b'0',
        b'A'..=b'F' => lo - b'A' + 10,
        b'a'..=b'f' => lo - b'a' + 10,
        _ => return None,
    };
    Some(h * 16 + l)
}

/// Perform a single pass of URI percent-decoding.
///
/// Returns `Cow::Borrowed` if no decoding was needed (optimization),
/// `Cow::Owned` with the decoded string otherwise, or `Err` if malformed
/// percent-encoding was detected.
fn decode_uri_once(input: &str) -> Result<Cow<'_, str>, String> {
    if !input.contains('%') {
        return Ok(Cow::Borrowed(input));
    }

    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(format!(
                    "Truncated percent-encoding at position {i} in '{input}'"
                ));
            }
            match decode_percent_byte(bytes[i + 1], bytes[i + 2]) {
                Some(decoded) => {
                    output.push(decoded);
                    i += 3;
                }
                None => {
                    return Err(format!(
                        "Malformed percent-encoding '%{}{}' at position {i}",
                        bytes[i + 1] as char,
                        bytes[i + 2] as char,
                    ));
                }
            }
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }

    // Convert decoded bytes back to string — if invalid UTF-8, reject.
    match String::from_utf8(output) {
        Ok(s) => Ok(Cow::Owned(s)),
        Err(_) => Err(format!("Non-UTF-8 bytes after decoding '{input}'")),
    }
}

/// Iteratively decode a URI path until it stabilizes.
///
/// This catches multi-layered encoding attacks:
/// - `%252e%252e%252f` → `%2e%2e%2f` → `../`
/// - `%25252e` → `%252e` → `%2e` → `.`
pub fn canonicalize_path(input: &str) -> CanonicalizationResult {
    let mut current = input.to_string();

    for _ in 0..MAX_DECODE_PASSES {
        match decode_uri_once(&current) {
            Ok(decoded) => {
                // Check for null bytes in decoded output.
                if decoded.contains('\0') {
                    return CanonicalizationResult::NullByte;
                }

                if decoded.as_ref() == current.as_str() {
                    // Stable — no more decoding needed.
                    let normalized = normalize_path(&current);
                    return CanonicalizationResult::Ok(normalized);
                }
                current = decoded.into_owned();
            }
            Err(msg) => {
                return CanonicalizationResult::MalformedEncoding(msg);
            }
        }
    }

    CanonicalizationResult::UnstableEncoding
}

/// Normalize a decoded path: collapse consecutive slashes, remove trailing
/// slashes, and resolve `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    // Split into segments, skipping empty segments (from `//`)
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Check if a canonicalized path falls under a protected prefix.
///
/// Protected paths require gateway-level authentication before the request
/// is forwarded to channel/plugin handlers.
pub fn is_protected_path(path: &str) -> bool {
    let canonical = match canonicalize_path(path) {
        CanonicalizationResult::Ok(p) => p,
        // Malformed or unstable paths are always treated as protected
        // (fail-closed — they will be rejected by the auth layer).
        _ => return true,
    };

    let lower = canonical.to_ascii_lowercase();
    PROTECTED_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Check if a raw path attempts to bypass protected route detection via
/// path manipulation (double encoding, traversal, etc.).
///
/// Returns `true` if the path is safe, `false` if it should be rejected.
pub fn validate_request_path(path: &str) -> Result<String, String> {
    match canonicalize_path(path) {
        CanonicalizationResult::Ok(canonical) => Ok(canonical),
        CanonicalizationResult::MalformedEncoding(msg) => {
            Err(format!("Rejected request path: {msg}"))
        }
        CanonicalizationResult::UnstableEncoding => {
            Err(format!(
                "Rejected request path '{path}': encoding did not stabilize after {MAX_DECODE_PASSES} passes"
            ))
        }
        CanonicalizationResult::NullByte => {
            Err(format!("Rejected request path '{path}': contains null byte"))
        }
    }
}

// ============================================================================
// Symlink rebind detection (extends hardlink_guards.rs)
// ============================================================================

/// Check if a path has been rebound via symlink between lstat and open.
///
/// This is a TOCTOU defense: after opening a file, verify that the opened
/// file descriptor still points to the same inode as the original path.
#[cfg(unix)]
pub fn detect_symlink_rebind(
    path: &std::path::Path,
    expected_dev: u64,
    expected_ino: u64,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return Err(format!("Cannot stat '{}': {}", path.display(), e)),
    };

    if meta.dev() != expected_dev || meta.ino() != expected_ino {
        return Err(format!(
            "Symlink rebind detected on '{}': expected dev={}/ino={}, \
             got dev={}/ino={}",
            path.display(),
            expected_dev,
            expected_ino,
            meta.dev(),
            meta.ino(),
        ));
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // decode_uri_once
    // ====================================================================

    #[test]
    fn decode_no_encoding() {
        assert_eq!(
            decode_uri_once("/api/health").unwrap(),
            Cow::Borrowed("/api/health")
        );
    }

    #[test]
    fn decode_simple_encoding() {
        let result = decode_uri_once("/api/channels%2Fstatus").unwrap();
        assert_eq!(result.as_ref(), "/api/channels/status");
    }

    #[test]
    fn decode_double_encoded_percent() {
        // %252F should decode to %2F (just one layer)
        let result = decode_uri_once("/api%252Fchannels").unwrap();
        assert_eq!(result.as_ref(), "/api%2Fchannels");
    }

    #[test]
    fn decode_malformed_truncated() {
        assert!(decode_uri_once("/api%2").is_err());
    }

    #[test]
    fn decode_malformed_invalid_hex() {
        assert!(decode_uri_once("/api%GG").is_err());
    }

    // ====================================================================
    // canonicalize_path
    // ====================================================================

    #[test]
    fn canonical_plain_path() {
        assert_eq!(
            canonicalize_path("/api/health"),
            CanonicalizationResult::Ok("/api/health".to_string())
        );
    }

    #[test]
    fn canonical_single_encoded_traversal() {
        let result = canonicalize_path("/api/channels%2F..%2Fsecret");
        match result {
            CanonicalizationResult::Ok(p) => assert_eq!(p, "/api/secret"),
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn canonical_double_encoded_traversal() {
        // %252e%252e = double-encoded ".."
        let result = canonicalize_path("/api/%252e%252e/secret");
        match result {
            CanonicalizationResult::Ok(p) => assert_eq!(p, "/secret"),
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn canonical_null_byte_rejected() {
        let result = canonicalize_path("/api/channels%00/status");
        assert_eq!(result, CanonicalizationResult::NullByte);
    }

    #[test]
    fn canonical_collapses_slashes() {
        let result = canonicalize_path("/api///channels////status");
        match result {
            CanonicalizationResult::Ok(p) => assert_eq!(p, "/api/channels/status"),
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn canonical_dot_segments() {
        let result = canonicalize_path("/api/./channels/../tools");
        match result {
            CanonicalizationResult::Ok(p) => assert_eq!(p, "/api/tools"),
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn canonical_empty_to_root() {
        let result = canonicalize_path("/");
        match result {
            CanonicalizationResult::Ok(p) => assert_eq!(p, "/"),
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    // ====================================================================
    // is_protected_path
    // ====================================================================

    #[test]
    fn protected_channels_path() {
        assert!(is_protected_path("/api/channels/status"));
        assert!(is_protected_path("/api/channels"));
    }

    #[test]
    fn protected_plugins_path() {
        assert!(is_protected_path("/api/plugins/my-plugin/route"));
    }

    #[test]
    fn protected_hooks_path() {
        assert!(is_protected_path("/api/hooks/webhook"));
    }

    #[test]
    fn not_protected_health() {
        assert!(!is_protected_path("/api/health"));
    }

    #[test]
    fn protected_encoded_bypass_attempt() {
        // Trying to sneak past with encoding
        assert!(is_protected_path("/api%2Fchannels/status"));
    }

    #[test]
    fn protected_double_encoded_bypass() {
        assert!(is_protected_path("/api%252Fchannels/status"));
    }

    #[test]
    fn protected_traversal_bypass() {
        // /api/health/../channels/status → /api/channels/status
        assert!(is_protected_path("/api/health/../channels/status"));
    }

    #[test]
    fn protected_case_insensitive() {
        assert!(is_protected_path("/API/CHANNELS/status"));
        assert!(is_protected_path("/Api/Channels"));
    }

    #[test]
    fn malformed_encoding_treated_as_protected() {
        // Fail-closed: malformed encoding is treated as protected
        assert!(is_protected_path("/api/channels%GG"));
    }

    // ====================================================================
    // validate_request_path
    // ====================================================================

    #[test]
    fn validate_clean_path() {
        assert!(validate_request_path("/api/health").is_ok());
    }

    #[test]
    fn validate_malformed_rejected() {
        assert!(validate_request_path("/api%2").is_err());
    }

    #[test]
    fn validate_null_byte_rejected() {
        assert!(validate_request_path("/api%00").is_err());
    }

    // ====================================================================
    // normalize_path edge cases
    // ====================================================================

    #[test]
    fn normalize_root_only() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_traversal_past_root() {
        // Can't go above root
        assert_eq!(normalize_path("/../../../etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn normalize_complex_traversal() {
        assert_eq!(
            normalize_path("/a/b/c/../../d/./e/../f"),
            "/a/d/f"
        );
    }

    // ====================================================================
    // Symlink rebind (Unix only)
    // ====================================================================

    #[cfg(unix)]
    #[test]
    fn rebind_detection_same_file() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let meta = std::fs::metadata(&file).unwrap();
        assert!(detect_symlink_rebind(&file, meta.dev(), meta.ino()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rebind_detection_wrong_inode() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        // Wrong inode — should detect rebind
        assert!(detect_symlink_rebind(&file, 0, 99999999).is_err());
    }
}
