use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use std::collections::HashSet;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

/// Environment variable names that must never be passed to child processes.
/// These can be used for code injection, credential theft, or privilege escalation.
const DANGEROUS_ENV_VARS: &[&str] = &[
    "NODE_OPTIONS",
    "BASH_ENV",
    "SHELLOPTS",
    "PS4",
    "SSLKEYLOGFILE",
    "ENV",
    "BASH_FUNC_%%",
    "PROMPT_COMMAND",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "NODE_PATH",
    "CDPATH",
    "GLOBIGNORE",
    "HISTFILE",
    "HISTFILESIZE",
    "HISTCONTROL",
    "COMP_WORDBREAKS",
    "MAILPATH",
    "FPATH",
    "GIT_EXEC_PATH",
];

/// Environment variable prefixes that indicate dangerous variables.
const DANGEROUS_ENV_PREFIXES: &[&str] = &["DYLD_", "LD_", "BASH_FUNC_"];

/// Environment variables that cannot be overridden via tool parameters.
const DANGEROUS_ENV_OVERRIDES: &[&str] = &["HOME", "ZDOTDIR"];

/// Check if an environment variable name is dangerous and should be blocked.
fn is_dangerous_env_var(name: &str) -> bool {
    if DANGEROUS_ENV_VARS.contains(&name) {
        return true;
    }
    for prefix in DANGEROUS_ENV_PREFIXES {
        if name.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Validate a command against safe-bin profile constraints.
fn validate_safe_bin(
    command_name: &str,
    args: &[&str],
    profile: &crate::config::SafeBinProfile,
) -> Option<String> {
    // Check max positional args
    if let Some(max) = profile.max_positional {
        let positional_count = args.iter().filter(|a| !a.starts_with('-')).count();
        if positional_count > max as usize {
            return Some(format!(
                "Safe-bin '{}': too many positional arguments ({}, max {})",
                command_name, positional_count, max
            ));
        }
    }

    // Check denied flags
    for arg in args {
        if arg.starts_with('-') {
            for denied in &profile.denied_flags {
                if *arg == denied.as_str()
                    || (arg.starts_with("--") && arg.split('=').next() == Some(denied.as_str()))
                {
                    return Some(format!(
                        "Safe-bin '{}': denied flag '{}' used",
                        command_name, denied
                    ));
                }
            }
        }
    }

    None
}

// ============================================================================
// Exec approval with argv identity binding (v2026.2.25)
// ============================================================================

/// A record of an exec approval, binding the approved command to its full
/// execution context. Used to verify that the command being run matches
/// what was originally approved by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecApprovalRecord {
    /// The full command text that was approved.
    pub command: String,
    /// The parsed argument vector.
    pub argv: Vec<String>,
    /// Working directory at approval time.
    pub cwd: Option<String>,
    /// Agent that requested the execution.
    pub agent_id: String,
    /// Session in which the approval was granted.
    pub session_key: String,
    /// Device that requested the approval.
    pub requested_by_device_id: Option<String>,
    /// The approval decision: "allow" or "deny".
    pub decision: String,
}

use serde::{Deserialize, Serialize};

/// Validate that an exec approval record matches the current system.run
/// request. All bound fields must match exactly.
pub fn approval_matches_system_run_request(
    approval: &ExecApprovalRecord,
    command: &str,
    cwd: Option<&str>,
    agent_id: &str,
    session_key: &str,
) -> bool {
    if approval.decision != "allow" {
        return false;
    }
    if approval.command != command {
        return false;
    }
    if approval.agent_id != agent_id {
        return false;
    }
    if approval.session_key != session_key {
        return false;
    }
    // CWD must match if it was bound in the approval.
    if let Some(ref approved_cwd) = approval.cwd {
        match cwd {
            Some(req_cwd) if req_cwd == approved_cwd => {}
            None if approved_cwd.is_empty() => {}
            _ => return false,
        }
    }
    true
}

/// Harden the execution environment when an approval is bound.
///
/// Validates that the CWD exists, is a directory, and is not a symlink.
/// When `approval_bound` is true, also canonicalizes the executable path
/// to prevent TOCTOU races between approval and execution.
pub fn harden_approved_execution_paths(
    cwd: Option<&str>,
    approval_bound: bool,
) -> Result<Option<std::path::PathBuf>, String> {
    if let Some(dir) = cwd {
        let path = std::path::Path::new(dir);

        // Check existence.
        if !path.exists() {
            return Err(format!("CWD does not exist: {}", dir));
        }

        // Must be a directory.
        if !path.is_dir() {
            return Err(format!("CWD is not a directory: {}", dir));
        }

        // When approval-bound, reject symlinked CWDs.
        if approval_bound {
            let canonical = path
                .canonicalize()
                .map_err(|e| format!("Failed to canonicalize CWD '{}': {}", dir, e))?;
            if canonical != path {
                return Err(format!(
                    "CWD '{}' contains symlinks (canonical: '{}'); \
                     rejected for approval-bound execution.",
                    dir,
                    canonical.display()
                ));
            }
            return Ok(Some(canonical));
        }
    }
    Ok(None)
}

/// Bash/shell command execution tool.
pub struct BashTool;

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "system_run".to_string(),
            description: "Execute a shell command".to_string(),
            category: "system".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds" },
                    "background": { "type": "boolean", "default": false }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing command parameter"))?;

        let cwd = params.get("cwd").and_then(|v| v.as_str());
        let timeout_secs = params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        // Security: check exec policy
        let exec_config = context.config.tools.exec.as_ref();
        let security = exec_config
            .and_then(|e| e.security.as_deref())
            .unwrap_or("full");

        if security == "deny" {
            return Ok(ToolResult::error(
                "Shell execution is disabled by security policy",
            ));
        }

        debug!("Executing command: {}", command);

        let shell = if cfg!(target_os = "windows") {
            "cmd"
        } else {
            "/bin/bash"
        };

        let shell_flag = if cfg!(target_os = "windows") {
            "/C"
        } else {
            "-c"
        };

        // Safe-bin profile validation: extract the first word of the command as the binary name
        if let Some(ref exec) = context.config.tools.exec {
            if let Some(ref profiles) = exec.safe_bin_profiles {
                let cmd_name = command.split_whitespace().next().unwrap_or("");
                // Strip path prefix to get bare binary name
                let bare_name = cmd_name.rsplit('/').next().unwrap_or(cmd_name);
                if let Some(profile) = profiles.get(bare_name) {
                    let args: Vec<&str> = command.split_whitespace().skip(1).collect();
                    if let Some(err) = validate_safe_bin(bare_name, &args, profile) {
                        return Ok(ToolResult::error(err));
                    }
                }
            }
        }

        let mut cmd = Command::new(shell);
        cmd.arg(shell_flag)
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        // Security: clear all env vars and selectively re-add only safe ones
        cmd.env_clear();
        for (key, value) in std::env::vars() {
            if !is_dangerous_env_var(&key) {
                cmd.env(&key, &value);
            }
        }

        // Apply PATH prepends
        if let Some(ref exec) = context.config.tools.exec {
            if !exec.path_prepend.is_empty() {
                let current_path = std::env::var("PATH").unwrap_or_default();
                let new_path = exec.path_prepend.join(":") + ":" + &current_path;
                cmd.env("PATH", new_path);
            }
        }

        let timeout = tokio::time::Duration::from_secs(timeout_secs);

        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                let mut result_text = String::new();
                if !stdout.is_empty() {
                    result_text.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    result_text.push_str("STDERR:\n");
                    result_text.push_str(&stderr);
                }

                Ok(ToolResult::json(serde_json::json!({
                    "exitCode": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "output": result_text
                })))
            }
            Ok(Err(e)) => Ok(ToolResult::error(format!("Command failed: {}", e))),
            Err(_) => Ok(ToolResult::error(format!(
                "Command timed out after {} seconds",
                timeout_secs
            ))),
        }
    }
}
