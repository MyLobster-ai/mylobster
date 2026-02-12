use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

/// Bash/shell command execution tool.
pub struct BashTool;

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "system.run".to_string(),
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

        let mut cmd = Command::new(shell);
        cmd.arg(shell_flag)
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
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
