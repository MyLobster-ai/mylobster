//! External command secret provider.
//!
//! Resolves `$EXEC{command args}` references by executing the command
//! and capturing stdout. Uses the same security controls as `bash.rs`.

use super::types::{SecretProvider, SecretRefKind, SecretResolution};
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;
use tracing::warn;

/// Maximum execution time for secret-resolving commands.
const EXEC_TIMEOUT_SECS: u64 = 30;

/// Maximum output size from a secret command (1 MB).
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Resolves secrets by executing external commands.
pub struct ExecSecretProvider {
    /// Working directory for command execution.
    cwd: Option<String>,
}

impl ExecSecretProvider {
    pub fn new(cwd: Option<String>) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl SecretProvider for ExecSecretProvider {
    fn kind(&self) -> SecretRefKind {
        SecretRefKind::Exec
    }

    fn name(&self) -> &str {
        "exec"
    }

    async fn resolve(&self, key: &str) -> SecretResolution {
        // Parse the command string into program and args.
        // Security: we use explicit arg splitting, not shell interpretation.
        let parts: Vec<&str> = key.split_whitespace().collect();
        if parts.is_empty() {
            return SecretResolution::Failed("Empty command".to_string());
        }

        let program = parts[0];
        let args = &parts[1..];

        // Security: reject obviously dangerous commands.
        if is_dangerous_command(program) {
            return SecretResolution::Failed(format!(
                "Command '{}' is not allowed for secret resolution",
                program
            ));
        }

        let mut cmd = Command::new(program);
        cmd.args(args);

        // Set working directory if configured.
        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // Don't inherit stdin — prevent hanging on interactive commands.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Execute with timeout.
        let result = tokio::time::timeout(
            Duration::from_secs(EXEC_TIMEOUT_SECS),
            cmd.output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(
                        "Secret exec command '{}' failed with status {}: {}",
                        program,
                        output.status,
                        stderr.trim()
                    );
                    return SecretResolution::Failed(format!(
                        "Command exited with status {}: {}",
                        output.status,
                        stderr.trim()
                    ));
                }

                if output.stdout.len() > MAX_OUTPUT_BYTES {
                    return SecretResolution::Failed(format!(
                        "Command output exceeds {} bytes",
                        MAX_OUTPUT_BYTES
                    ));
                }

                let value = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .to_string();

                if value.is_empty() {
                    SecretResolution::NotFound(format!(
                        "Command '{}' produced empty output",
                        program
                    ))
                } else {
                    SecretResolution::Resolved(value)
                }
            }
            Ok(Err(e)) => SecretResolution::Failed(format!(
                "Failed to execute '{}': {}",
                program, e
            )),
            Err(_) => SecretResolution::Failed(format!(
                "Command '{}' timed out after {}s",
                program, EXEC_TIMEOUT_SECS
            )),
        }
    }
}

/// Check if a command is too dangerous for secret resolution.
fn is_dangerous_command(program: &str) -> bool {
    let basename = program
        .rsplit('/')
        .next()
        .unwrap_or(program);

    matches!(
        basename,
        "rm" | "rmdir" | "mkfs" | "dd" | "shutdown" | "reboot"
            | "halt" | "poweroff" | "kill" | "killall" | "pkill"
            | "format" | "fdisk" | "parted"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_echo_command() {
        let provider = ExecSecretProvider::new(None);
        let result = provider.resolve("echo hello-secret").await;
        assert!(result.is_resolved());
        assert_eq!(result.value(), Some("hello-secret"));
    }

    #[tokio::test]
    async fn resolve_failing_command() {
        let provider = ExecSecretProvider::new(None);
        let result = provider.resolve("false").await;
        assert!(!result.is_resolved());
    }

    #[tokio::test]
    async fn reject_dangerous_command() {
        let provider = ExecSecretProvider::new(None);
        let result = provider.resolve("rm -rf /").await;
        assert!(!result.is_resolved());
        assert!(result.error_message().unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn empty_command_fails() {
        let provider = ExecSecretProvider::new(None);
        let result = provider.resolve("").await;
        assert!(!result.is_resolved());
    }

    #[test]
    fn dangerous_command_detection() {
        assert!(is_dangerous_command("rm"));
        assert!(is_dangerous_command("/bin/rm"));
        assert!(is_dangerous_command("kill"));
        assert!(!is_dangerous_command("echo"));
        assert!(!is_dangerous_command("vault"));
        assert!(!is_dangerous_command("aws"));
    }
}
