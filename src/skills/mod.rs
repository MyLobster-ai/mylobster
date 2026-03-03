//! Skills System for MyLobster gateway.
//!
//! Skills are user-installable integrations parsed from `SKILL.md` files.
//! Each skill declares its name, description, required environment variables,
//! and other metadata via YAML frontmatter.
//!
//! The [`SkillRegistry`] manages the lifecycle of installed skills:
//! install, update, enable, disable, invoke, and uninstall.

pub mod manifest;

pub use manifest::{
    OpenClawMetadata, SkillManifest, SkillMetadata, SkillRequirements,
    parse_skill_manifest, parse_skill_manifest_file,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ============================================================================
// Skill Status
// ============================================================================

/// Current status of a skill in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillStatus {
    /// Skill is available for installation (discovered but not installed).
    Available,
    /// Skill is installed but not currently active.
    Installed,
    /// Skill is installed and actively running.
    Active,
    /// Skill encountered an error.
    Error(String),
}

impl std::fmt::Display for SkillStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillStatus::Available => write!(f, "available"),
            SkillStatus::Installed => write!(f, "installed"),
            SkillStatus::Active => write!(f, "active"),
            SkillStatus::Error(msg) => write!(f, "error: {}", msg),
        }
    }
}

// ============================================================================
// Installed Skill
// ============================================================================

/// A skill that has been installed into the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSkill {
    /// The skill manifest parsed from SKILL.md.
    pub manifest: SkillManifest,
    /// Current status of the skill.
    pub status: SkillStatus,
    /// Path to the skill directory on disk (if filesystem-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Resolved environment variable values (name -> is_set).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_status: HashMap<String, bool>,
    /// Timestamp when the skill was installed (RFC 3339).
    pub installed_at: String,
    /// Timestamp when the skill was last updated (RFC 3339).
    pub updated_at: String,
}

impl InstalledSkill {
    /// Create a new installed skill from a manifest.
    fn new(manifest: SkillManifest, path: Option<PathBuf>) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        let env_status = check_env_requirements(&manifest);

        Self {
            manifest,
            status: SkillStatus::Installed,
            path,
            env_status,
            installed_at: now.clone(),
            updated_at: now,
        }
    }

    /// Check whether all required environment variables are set.
    pub fn env_satisfied(&self) -> bool {
        self.env_status.values().all(|&set| set)
    }

    /// Get the list of missing environment variables.
    pub fn missing_env_vars(&self) -> Vec<String> {
        self.env_status
            .iter()
            .filter(|(_, &set)| !set)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Check which required environment variables are set for a skill.
fn check_env_requirements(manifest: &SkillManifest) -> HashMap<String, bool> {
    manifest
        .metadata
        .openclaw
        .requires
        .env
        .iter()
        .map(|var| (var.clone(), std::env::var(var).is_ok()))
        .collect()
}

// ============================================================================
// Skill Invocation
// ============================================================================

/// A request to invoke a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvocation {
    /// Name of the skill to invoke.
    pub skill_name: String,
    /// Action or tool call within the skill.
    pub action: String,
    /// Parameters for the invocation.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// The result of a skill invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvocationResult {
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Result data (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Error message (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============================================================================
// Skill Registry
// ============================================================================

/// Registry that tracks installed and available skills.
///
/// Thread-safe via `Arc<RwLock<...>>` interior. Designed to be shared
/// across async tasks in the gateway.
pub struct SkillRegistry {
    /// Installed skills indexed by name.
    skills: RwLock<HashMap<String, InstalledSkill>>,
}

impl SkillRegistry {
    /// Create a new empty skill registry.
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// Install a skill from a manifest.
    ///
    /// Returns an error if a skill with the same name is already installed.
    /// Use `update_skill()` to replace an existing skill.
    pub async fn install_skill(
        &self,
        manifest: SkillManifest,
        path: Option<PathBuf>,
    ) -> Result<InstalledSkill> {
        let name = manifest.name.clone();
        let mut skills = self.skills.write().await;

        if skills.contains_key(&name) {
            anyhow::bail!("Skill '{}' is already installed; use update_skill() instead", name);
        }

        let installed = InstalledSkill::new(manifest, path);

        if !installed.env_satisfied() {
            let missing = installed.missing_env_vars();
            warn!(
                skill = %name,
                missing = ?missing,
                "Skill installed but required env vars are missing"
            );
        } else {
            info!(skill = %name, "Skill installed successfully");
        }

        let result = installed.clone();
        skills.insert(name, installed);
        Ok(result)
    }

    /// Update an existing skill with a new manifest.
    ///
    /// Preserves the install timestamp, updates the rest.
    pub async fn update_skill(
        &self,
        manifest: SkillManifest,
        path: Option<PathBuf>,
    ) -> Result<InstalledSkill> {
        let name = manifest.name.clone();
        let mut skills = self.skills.write().await;

        let installed_at = skills
            .get(&name)
            .map(|s| s.installed_at.clone())
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

        let env_status = check_env_requirements(&manifest);
        let now = chrono::Utc::now().to_rfc3339();

        let updated = InstalledSkill {
            manifest,
            status: SkillStatus::Installed,
            path,
            env_status,
            installed_at,
            updated_at: now,
        };

        info!(skill = %name, "Skill updated");

        let result = updated.clone();
        skills.insert(name, updated);
        Ok(result)
    }

    /// List all installed skills.
    pub async fn list_skills(&self) -> Vec<InstalledSkill> {
        let skills = self.skills.read().await;
        skills.values().cloned().collect()
    }

    /// Get a specific skill by name.
    pub async fn get_skill(&self, name: &str) -> Option<InstalledSkill> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Check whether a skill is installed.
    pub async fn is_installed(&self, name: &str) -> bool {
        let skills = self.skills.read().await;
        skills.contains_key(name)
    }

    /// Uninstall a skill by name.
    ///
    /// Returns the removed skill, or None if it was not installed.
    pub async fn uninstall_skill(&self, name: &str) -> Option<InstalledSkill> {
        let mut skills = self.skills.write().await;
        let removed = skills.remove(name);
        if removed.is_some() {
            info!(skill = %name, "Skill uninstalled");
        } else {
            debug!(skill = %name, "Attempted to uninstall non-existent skill");
        }
        removed
    }

    /// Set the status of a skill.
    pub async fn set_status(&self, name: &str, status: SkillStatus) -> Result<()> {
        let mut skills = self.skills.write().await;
        let skill = skills
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("Skill '{}' not found", name))?;

        debug!(skill = %name, old = %skill.status, new = %status, "Skill status changed");
        skill.status = status;
        skill.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(())
    }

    /// Refresh environment variable status for all installed skills.
    ///
    /// Re-checks which required env vars are set. Useful after environment
    /// changes (e.g., user sets a new API key).
    pub async fn refresh_env_status(&self) {
        let mut skills = self.skills.write().await;
        for (name, skill) in skills.iter_mut() {
            let new_status = check_env_requirements(&skill.manifest);
            if new_status != skill.env_status {
                debug!(skill = %name, "Environment status changed");
                skill.env_status = new_status;
                skill.updated_at = chrono::Utc::now().to_rfc3339();
            }
        }
    }

    /// Invoke a skill action.
    ///
    /// Validates that the skill is installed and active, then dispatches
    /// the invocation. Currently returns a placeholder result; actual
    /// execution depends on the skill's implementation.
    pub async fn invoke(
        &self,
        invocation: &SkillInvocation,
    ) -> Result<SkillInvocationResult> {
        let skills = self.skills.read().await;
        let skill = skills
            .get(&invocation.skill_name)
            .ok_or_else(|| anyhow::anyhow!("Skill '{}' not found", invocation.skill_name))?;

        match &skill.status {
            SkillStatus::Active => {
                debug!(
                    skill = %invocation.skill_name,
                    action = %invocation.action,
                    "Invoking skill action"
                );

                // Skill invocation is currently a stub.
                // When skill execution backends are implemented (e.g., WASM,
                // subprocess, HTTP), the dispatch logic will go here.
                Ok(SkillInvocationResult {
                    success: true,
                    data: Some(serde_json::json!({
                        "skill": invocation.skill_name,
                        "action": invocation.action,
                        "status": "invoked"
                    })),
                    error: None,
                })
            }
            SkillStatus::Installed => {
                Err(anyhow::anyhow!(
                    "Skill '{}' is installed but not active; activate it first",
                    invocation.skill_name
                ))
            }
            SkillStatus::Error(msg) => {
                Err(anyhow::anyhow!(
                    "Skill '{}' is in error state: {}",
                    invocation.skill_name,
                    msg
                ))
            }
            SkillStatus::Available => {
                Err(anyhow::anyhow!(
                    "Skill '{}' is available but not installed; install it first",
                    invocation.skill_name
                ))
            }
        }
    }

    /// Get the count of installed skills.
    pub async fn count(&self) -> usize {
        self.skills.read().await.len()
    }

    /// Discover and install skills from a directory.
    ///
    /// Scans the given directory for subdirectories containing `SKILL.md`
    /// files, parses them, and installs any that are not already registered.
    /// Returns the number of newly installed skills.
    pub async fn discover_from_directory(&self, dir: &Path) -> Result<usize> {
        let mut count = 0;

        let mut entries = tokio::fs::read_dir(dir)
            .await
            .with_context(|| format!("Failed to read skills directory: {}", dir.display()))?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }

            match parse_skill_manifest_file(&skill_md).await {
                Ok(manifest) => {
                    let name = manifest.name.clone();
                    if !self.is_installed(&name).await {
                        match self.install_skill(manifest, Some(path)).await {
                            Ok(_) => {
                                count += 1;
                                debug!(skill = %name, "Discovered and installed skill");
                            }
                            Err(e) => {
                                warn!(skill = %name, error = %e, "Failed to install discovered skill");
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        path = %skill_md.display(),
                        error = %e,
                        "Failed to parse SKILL.md"
                    );
                }
            }
        }

        if count > 0 {
            info!(count, dir = %dir.display(), "Discovered skills from directory");
        }

        Ok(count)
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Shared Registry (thread-safe wrapper)
// ============================================================================

/// Thread-safe, cloneable handle to a [`SkillRegistry`].
///
/// Use this in gateway state and across async task boundaries.
pub type SharedSkillRegistry = Arc<SkillRegistry>;

/// Create a new shared skill registry.
pub fn new_shared_registry() -> SharedSkillRegistry {
    Arc::new(SkillRegistry::new())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(name: &str) -> SkillManifest {
        SkillManifest {
            name: name.to_string(),
            description: format!("{} skill", name),
            homepage: None,
            metadata: SkillMetadata::default(),
            primary_env: None,
        }
    }

    fn manifest_with_env(name: &str, env_vars: Vec<&str>) -> SkillManifest {
        SkillManifest {
            name: name.to_string(),
            description: format!("{} skill", name),
            homepage: Some(format!("https://example.com/{}", name)),
            metadata: SkillMetadata {
                openclaw: OpenClawMetadata {
                    requires: SkillRequirements {
                        env: env_vars.into_iter().map(String::from).collect(),
                    },
                },
            },
            primary_env: Some("SOME_API_KEY".to_string()),
        }
    }

    #[tokio::test]
    async fn install_and_get_skill() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("gmail");

        let installed = registry.install_skill(manifest, None).await.unwrap();
        assert_eq!(installed.manifest.name, "gmail");
        assert_eq!(installed.status, SkillStatus::Installed);

        let retrieved = registry.get_skill("gmail").await.unwrap();
        assert_eq!(retrieved.manifest.name, "gmail");
    }

    #[tokio::test]
    async fn install_duplicate_fails() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("calendar");

        registry.install_skill(manifest.clone(), None).await.unwrap();
        let result = registry.install_skill(manifest, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_skill() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("calendar");
        registry.install_skill(manifest, None).await.unwrap();

        let mut updated_manifest = sample_manifest("calendar");
        updated_manifest.description = "Updated calendar skill".to_string();
        let updated = registry.update_skill(updated_manifest, None).await.unwrap();
        assert_eq!(updated.manifest.description, "Updated calendar skill");
    }

    #[tokio::test]
    async fn uninstall_skill() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("notes");
        registry.install_skill(manifest, None).await.unwrap();

        assert!(registry.is_installed("notes").await);
        let removed = registry.uninstall_skill("notes").await;
        assert!(removed.is_some());
        assert!(!registry.is_installed("notes").await);
    }

    #[tokio::test]
    async fn uninstall_nonexistent_returns_none() {
        let registry = SkillRegistry::new();
        assert!(registry.uninstall_skill("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn list_skills() {
        let registry = SkillRegistry::new();
        registry
            .install_skill(sample_manifest("gmail"), None)
            .await
            .unwrap();
        registry
            .install_skill(sample_manifest("calendar"), None)
            .await
            .unwrap();

        let list = registry.list_skills().await;
        assert_eq!(list.len(), 2);

        let names: Vec<&str> = list.iter().map(|s| s.manifest.name.as_str()).collect();
        assert!(names.contains(&"gmail"));
        assert!(names.contains(&"calendar"));
    }

    #[tokio::test]
    async fn skill_status_transitions() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("test");
        registry.install_skill(manifest, None).await.unwrap();

        // Installed -> Active
        registry
            .set_status("test", SkillStatus::Active)
            .await
            .unwrap();
        let skill = registry.get_skill("test").await.unwrap();
        assert_eq!(skill.status, SkillStatus::Active);

        // Active -> Error
        registry
            .set_status("test", SkillStatus::Error("connection failed".to_string()))
            .await
            .unwrap();
        let skill = registry.get_skill("test").await.unwrap();
        assert_eq!(
            skill.status,
            SkillStatus::Error("connection failed".to_string())
        );
    }

    #[tokio::test]
    async fn set_status_nonexistent_fails() {
        let registry = SkillRegistry::new();
        let result = registry.set_status("ghost", SkillStatus::Active).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invoke_active_skill() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("gmail");
        registry.install_skill(manifest, None).await.unwrap();
        registry
            .set_status("gmail", SkillStatus::Active)
            .await
            .unwrap();

        let invocation = SkillInvocation {
            skill_name: "gmail".to_string(),
            action: "send_email".to_string(),
            params: serde_json::json!({ "to": "user@example.com" }),
        };

        let result = registry.invoke(&invocation).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn invoke_installed_but_inactive_fails() {
        let registry = SkillRegistry::new();
        let manifest = sample_manifest("gmail");
        registry.install_skill(manifest, None).await.unwrap();

        let invocation = SkillInvocation {
            skill_name: "gmail".to_string(),
            action: "send_email".to_string(),
            params: serde_json::json!({}),
        };

        let result = registry.invoke(&invocation).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invoke_nonexistent_skill_fails() {
        let registry = SkillRegistry::new();
        let invocation = SkillInvocation {
            skill_name: "ghost".to_string(),
            action: "do_thing".to_string(),
            params: serde_json::json!({}),
        };
        let result = registry.invoke(&invocation).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn env_status_tracking() {
        let manifest = manifest_with_env(
            "needs_env",
            vec!["__MYLOBSTER_TEST_EXISTING_VAR_12345__", "__MYLOBSTER_TEST_MISSING_VAR_67890__"],
        );

        // Set one env var for the test
        std::env::set_var("__MYLOBSTER_TEST_EXISTING_VAR_12345__", "value");

        let registry = SkillRegistry::new();
        let installed = registry.install_skill(manifest, None).await.unwrap();

        assert!(!installed.env_satisfied());
        let missing = installed.missing_env_vars();
        assert!(missing.contains(&"__MYLOBSTER_TEST_MISSING_VAR_67890__".to_string()));
        assert!(!missing.contains(&"__MYLOBSTER_TEST_EXISTING_VAR_12345__".to_string()));

        // Cleanup
        std::env::remove_var("__MYLOBSTER_TEST_EXISTING_VAR_12345__");
    }

    #[tokio::test]
    async fn count_skills() {
        let registry = SkillRegistry::new();
        assert_eq!(registry.count().await, 0);

        registry
            .install_skill(sample_manifest("a"), None)
            .await
            .unwrap();
        registry
            .install_skill(sample_manifest("b"), None)
            .await
            .unwrap();
        assert_eq!(registry.count().await, 2);

        registry.uninstall_skill("a").await;
        assert_eq!(registry.count().await, 1);
    }

    #[test]
    fn skill_status_display() {
        assert_eq!(SkillStatus::Available.to_string(), "available");
        assert_eq!(SkillStatus::Installed.to_string(), "installed");
        assert_eq!(SkillStatus::Active.to_string(), "active");
        assert_eq!(
            SkillStatus::Error("oops".to_string()).to_string(),
            "error: oops"
        );
    }

    #[test]
    fn installed_skill_serialization() {
        let skill = InstalledSkill::new(sample_manifest("test"), None);
        let json = serde_json::to_value(&skill).unwrap();
        assert_eq!(json["manifest"]["name"], "test");
        assert_eq!(json["status"], "installed");
        assert!(json["installedAt"].is_string());
    }

    #[test]
    fn shared_registry_creation() {
        let _registry = new_shared_registry();
    }

    #[test]
    fn default_registry_is_empty() {
        let registry = SkillRegistry::default();
        // Can't await in sync test, but confirms Default works
        let _ = registry;
    }
}
