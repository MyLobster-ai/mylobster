//! Skill manifest parsing.
//!
//! Parses YAML frontmatter from `SKILL.md` files. The frontmatter is
//! delimited by `---` markers at the top of the file, followed by
//! markdown content describing the skill.
//!
//! Example SKILL.md:
//! ```markdown
//! ---
//! name: gmail
//! description: Gmail integration for reading and sending emails
//! homepage: https://github.com/mylobster/skill-gmail
//! metadata:
//!   openclaw:
//!     requires:
//!       env:
//!         - GOOGLE_CLIENT_ID
//!         - GOOGLE_CLIENT_SECRET
//! primaryEnv: GOOGLE_CLIENT_ID
//! ---
//!
//! # Gmail Skill
//!
//! This skill provides Gmail integration...
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::debug;

/// Parsed skill manifest from SKILL.md frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillManifest {
    /// Unique skill name (e.g., "gmail", "calendar").
    pub name: String,

    /// Human-readable description of the skill.
    #[serde(default)]
    pub description: String,

    /// Homepage or repository URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    /// Nested metadata including env requirements.
    #[serde(default)]
    pub metadata: SkillMetadata,

    /// The primary environment variable for this skill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_env: Option<String>,
}

/// Metadata section of the skill manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillMetadata {
    /// OpenClaw-specific metadata.
    #[serde(default)]
    pub openclaw: OpenClawMetadata,
}

/// OpenClaw-specific skill metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenClawMetadata {
    /// Requirements for the skill.
    #[serde(default)]
    pub requires: SkillRequirements,
}

/// Skill requirements (environment variables, etc.).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRequirements {
    /// Environment variables required by this skill.
    #[serde(default)]
    pub env: Vec<String>,
}

/// Extract YAML frontmatter from a SKILL.md file content string.
///
/// Returns (frontmatter_yaml, markdown_body). The frontmatter is the text
/// between the first pair of `---` delimiters. Returns None if no valid
/// frontmatter is found.
fn extract_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    // Skip the opening `---` and any trailing whitespace on that line
    let after_opening = &trimmed[3..];
    let after_opening = after_opening.trim_start_matches([' ', '\t']);
    let after_opening = if after_opening.starts_with('\n') {
        &after_opening[1..]
    } else if after_opening.starts_with("\r\n") {
        &after_opening[2..]
    } else {
        after_opening
    };

    // Find the closing `---`
    if let Some(end_pos) = after_opening.find("\n---") {
        let yaml = &after_opening[..end_pos];
        let rest_start = end_pos + 4; // skip "\n---"
        let body = if rest_start < after_opening.len() {
            after_opening[rest_start..].trim_start_matches([' ', '\t', '\r', '\n'])
        } else {
            ""
        };
        Some((yaml, body))
    } else {
        None
    }
}

/// Parse a skill manifest from SKILL.md file content.
///
/// Extracts the YAML frontmatter and deserializes it into a `SkillManifest`.
pub fn parse_skill_manifest(content: &str) -> Result<SkillManifest> {
    let (yaml, _body) = extract_frontmatter(content)
        .context("SKILL.md missing YAML frontmatter (expected --- delimiters)")?;

    debug!(yaml_len = yaml.len(), "Parsing skill manifest frontmatter");

    let manifest: SkillManifest =
        serde_yaml::from_str(yaml).context("Failed to parse SKILL.md YAML frontmatter")?;

    Ok(manifest)
}

/// Parse a skill manifest from a SKILL.md file on disk.
pub async fn parse_skill_manifest_file(path: &Path) -> Result<SkillManifest> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read SKILL.md at {}", path.display()))?;

    parse_skill_manifest(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let content = r#"---
name: gmail
description: Gmail integration
---

# Gmail Skill
"#;
        let manifest = parse_skill_manifest(content).unwrap();
        assert_eq!(manifest.name, "gmail");
        assert_eq!(manifest.description, "Gmail integration");
        assert!(manifest.homepage.is_none());
        assert!(manifest.metadata.openclaw.requires.env.is_empty());
    }

    #[test]
    fn parse_full_manifest() {
        let content = r#"---
name: gmail
description: Gmail integration for reading and sending emails
homepage: https://github.com/mylobster/skill-gmail
metadata:
  openclaw:
    requires:
      env:
        - GOOGLE_CLIENT_ID
        - GOOGLE_CLIENT_SECRET
primaryEnv: GOOGLE_CLIENT_ID
---

# Gmail Skill

This skill provides Gmail integration.
"#;
        let manifest = parse_skill_manifest(content).unwrap();
        assert_eq!(manifest.name, "gmail");
        assert_eq!(
            manifest.description,
            "Gmail integration for reading and sending emails"
        );
        assert_eq!(
            manifest.homepage.as_deref(),
            Some("https://github.com/mylobster/skill-gmail")
        );
        assert_eq!(manifest.metadata.openclaw.requires.env.len(), 2);
        assert_eq!(
            manifest.metadata.openclaw.requires.env[0],
            "GOOGLE_CLIENT_ID"
        );
        assert_eq!(
            manifest.metadata.openclaw.requires.env[1],
            "GOOGLE_CLIENT_SECRET"
        );
        assert_eq!(manifest.primary_env.as_deref(), Some("GOOGLE_CLIENT_ID"));
    }

    #[test]
    fn missing_frontmatter_returns_error() {
        let content = "# No frontmatter here\nJust markdown.";
        assert!(parse_skill_manifest(content).is_err());
    }

    #[test]
    fn extract_frontmatter_basic() {
        let content = "---\nname: test\n---\n# Body";
        let (yaml, body) = extract_frontmatter(content).unwrap();
        assert_eq!(yaml, "name: test");
        assert_eq!(body, "# Body");
    }

    #[test]
    fn extract_frontmatter_no_body() {
        let content = "---\nname: test\n---";
        let (yaml, body) = extract_frontmatter(content).unwrap();
        assert_eq!(yaml, "name: test");
        assert_eq!(body, "");
    }
}
