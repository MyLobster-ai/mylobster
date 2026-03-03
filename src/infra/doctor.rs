use std::fmt;
use std::path::PathBuf;

use anyhow::Result;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Outcome status for a single diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Ok,
    Warning,
    Error,
    Skipped,
}

impl fmt::Display for DiagnosticStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Warning => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
            Self::Skipped => write!(f, "SKIP"),
        }
    }
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone)]
pub struct DiagnosticResult {
    pub check_name: String,
    pub status: DiagnosticStatus,
    pub message: String,
    pub details: Option<String>,
}

impl fmt::Display for DiagnosticResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.status, self.check_name, self.message)?;
        if let Some(ref d) = self.details {
            write!(f, " ({})", d)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run all diagnostic checks and return the collected results.
pub async fn run_diagnostics() -> Result<Vec<DiagnosticResult>> {
    tracing::info!("Running system diagnostics...");

    let mut results = Vec::new();

    results.push(check_config_file().await);
    results.push(check_env_anthropic_key().await);
    results.push(check_env_openai_key().await);
    results.push(check_channel_telegram().await);
    results.push(check_channel_discord().await);
    results.push(check_channel_slack().await);
    results.push(check_memory_sqlite().await);
    results.push(check_database_url().await);
    results.push(check_browser_binary().await);
    results.push(check_ffmpeg().await);
    results.push(check_disk_space().await);
    results.push(check_network_anthropic().await);

    let ok = results.iter().filter(|r| r.status == DiagnosticStatus::Ok).count();
    let warn = results.iter().filter(|r| r.status == DiagnosticStatus::Warning).count();
    let err = results.iter().filter(|r| r.status == DiagnosticStatus::Error).count();
    let skip = results.iter().filter(|r| r.status == DiagnosticStatus::Skipped).count();

    tracing::info!(
        ok,
        warn,
        err,
        skip,
        total = results.len(),
        "Diagnostics complete"
    );

    Ok(results)
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

/// Check that a JSON/YAML/TOML config file is present.
async fn check_config_file() -> DiagnosticResult {
    let candidates = config_file_candidates();

    for path in &candidates {
        if tokio::fs::metadata(path).await.is_ok() {
            return DiagnosticResult {
                check_name: "config_file".into(),
                status: DiagnosticStatus::Ok,
                message: "Config file found".into(),
                details: Some(path.display().to_string()),
            };
        }
    }

    DiagnosticResult {
        check_name: "config_file".into(),
        status: DiagnosticStatus::Warning,
        message: "No config file found (using env vars / defaults)".into(),
        details: Some(format!("searched: {:?}", candidates)),
    }
}

/// Return the conventional config file paths to check.
fn config_file_candidates() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("mylobster.json"),
        PathBuf::from("mylobster.yaml"),
        PathBuf::from("mylobster.toml"),
        PathBuf::from("config.json"),
    ];

    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".mylobster/config.json"));
        paths.push(home.join(".config/mylobster/config.json"));
    }

    paths
}

/// Check ANTHROPIC_API_KEY.
async fn check_env_anthropic_key() -> DiagnosticResult {
    check_env_var("anthropic_api_key", "ANTHROPIC_API_KEY", true)
}

/// Check OPENAI_API_KEY.
async fn check_env_openai_key() -> DiagnosticResult {
    check_env_var("openai_api_key", "OPENAI_API_KEY", false)
}

/// Check TELEGRAM_BOT_TOKEN.
async fn check_channel_telegram() -> DiagnosticResult {
    check_env_var("channel_telegram", "TELEGRAM_BOT_TOKEN", false)
}

/// Check DISCORD_BOT_TOKEN.
async fn check_channel_discord() -> DiagnosticResult {
    check_env_var("channel_discord", "DISCORD_BOT_TOKEN", false)
}

/// Check SLACK_BOT_TOKEN.
async fn check_channel_slack() -> DiagnosticResult {
    check_env_var("channel_slack", "SLACK_BOT_TOKEN", false)
}

/// Generic env-var presence check.
fn check_env_var(check_name: &str, var: &str, required: bool) -> DiagnosticResult {
    match std::env::var(var) {
        Ok(val) if !val.is_empty() => DiagnosticResult {
            check_name: check_name.into(),
            status: DiagnosticStatus::Ok,
            message: format!("{} is set", var),
            details: Some(format!("{}...{}", &val[..3.min(val.len())], &val[val.len().saturating_sub(3)..])),
        },
        _ => DiagnosticResult {
            check_name: check_name.into(),
            status: if required {
                DiagnosticStatus::Error
            } else {
                DiagnosticStatus::Skipped
            },
            message: format!("{} is not set", var),
            details: None,
        },
    }
}

/// Check that the SQLite memory directory is writable.
async fn check_memory_sqlite() -> DiagnosticResult {
    let db_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mylobster");

    if tokio::fs::metadata(&db_dir).await.is_ok() {
        DiagnosticResult {
            check_name: "memory_sqlite".into(),
            status: DiagnosticStatus::Ok,
            message: "SQLite data directory exists".into(),
            details: Some(db_dir.display().to_string()),
        }
    } else {
        DiagnosticResult {
            check_name: "memory_sqlite".into(),
            status: DiagnosticStatus::Warning,
            message: "SQLite data directory does not exist (will be created on first use)".into(),
            details: Some(db_dir.display().to_string()),
        }
    }
}

/// Check DATABASE_URL for PostgreSQL connectivity.
async fn check_database_url() -> DiagnosticResult {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => DiagnosticResult {
            check_name: "database_url".into(),
            status: DiagnosticStatus::Ok,
            message: "DATABASE_URL is set".into(),
            details: Some(mask_connection_string(&url)),
        },
        _ => DiagnosticResult {
            check_name: "database_url".into(),
            status: DiagnosticStatus::Skipped,
            message: "DATABASE_URL not set (using SQLite)".into(),
            details: None,
        },
    }
}

/// Mask password in a connection string for safe display.
fn mask_connection_string(url: &str) -> String {
    // postgres://user:password@host/db -> postgres://user:***@host/db
    if let Some(at_pos) = url.find('@') {
        if let Some(colon_pos) = url[..at_pos].rfind(':') {
            let prefix = &url[..colon_pos + 1];
            let suffix = &url[at_pos..];
            return format!("{}***{}", prefix, suffix);
        }
    }
    url.to_string()
}

/// Check that a browser binary (chromium / chrome) is available.
async fn check_browser_binary() -> DiagnosticResult {
    let candidates = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];

    for bin in &candidates {
        if binary_exists(bin).await {
            return DiagnosticResult {
                check_name: "browser_binary".into(),
                status: DiagnosticStatus::Ok,
                message: "Browser binary found".into(),
                details: Some((*bin).to_string()),
            };
        }
    }

    DiagnosticResult {
        check_name: "browser_binary".into(),
        status: DiagnosticStatus::Warning,
        message: "No Chrome/Chromium binary found (browser tools will be unavailable)".into(),
        details: None,
    }
}

/// Check that ffmpeg is installed.
async fn check_ffmpeg() -> DiagnosticResult {
    if binary_exists("ffmpeg").await {
        DiagnosticResult {
            check_name: "ffmpeg".into(),
            status: DiagnosticStatus::Ok,
            message: "ffmpeg is available".into(),
            details: None,
        }
    } else {
        DiagnosticResult {
            check_name: "ffmpeg".into(),
            status: DiagnosticStatus::Warning,
            message: "ffmpeg not found (media processing will be limited)".into(),
            details: None,
        }
    }
}

/// Check available disk space on the current working directory's filesystem.
async fn check_disk_space() -> DiagnosticResult {
    let output = tokio::process::Command::new("df")
        .arg("-h")
        .arg(".")
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Parse df output: last line, 4th column is available space
            let lines: Vec<&str> = stdout.lines().collect();
            if lines.len() >= 2 {
                let fields: Vec<&str> = lines[1].split_whitespace().collect();
                let avail = fields.get(3).unwrap_or(&"unknown");
                let use_pct = fields.get(4).unwrap_or(&"?");

                // Warn if usage is above 90%
                let pct_num: u32 = use_pct.trim_end_matches('%').parse().unwrap_or(0);
                let status = if pct_num >= 95 {
                    DiagnosticStatus::Error
                } else if pct_num >= 90 {
                    DiagnosticStatus::Warning
                } else {
                    DiagnosticStatus::Ok
                };

                DiagnosticResult {
                    check_name: "disk_space".into(),
                    status,
                    message: format!("{} available, {} used", avail, use_pct),
                    details: None,
                }
            } else {
                DiagnosticResult {
                    check_name: "disk_space".into(),
                    status: DiagnosticStatus::Warning,
                    message: "Could not parse df output".into(),
                    details: Some(stdout.to_string()),
                }
            }
        }
        _ => DiagnosticResult {
            check_name: "disk_space".into(),
            status: DiagnosticStatus::Skipped,
            message: "Could not run `df`".into(),
            details: None,
        },
    }
}

/// Check network connectivity to api.anthropic.com.
async fn check_network_anthropic() -> DiagnosticResult {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            return DiagnosticResult {
                check_name: "network_anthropic".into(),
                status: DiagnosticStatus::Error,
                message: "Failed to create HTTP client".into(),
                details: Some(e.to_string()),
            };
        }
    };

    match client
        .get("https://api.anthropic.com/v1/messages")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            // Any response (even 401) means network connectivity is fine.
            DiagnosticResult {
                check_name: "network_anthropic".into(),
                status: DiagnosticStatus::Ok,
                message: format!("api.anthropic.com reachable (HTTP {})", resp.status().as_u16()),
                details: None,
            }
        }
        Err(e) => DiagnosticResult {
            check_name: "network_anthropic".into(),
            status: DiagnosticStatus::Error,
            message: "Cannot reach api.anthropic.com".into(),
            details: Some(e.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a binary exists on `$PATH`.
async fn binary_exists(name: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
