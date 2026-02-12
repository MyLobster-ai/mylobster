mod defaults;
mod io;
mod types;
mod validation;

pub use defaults::*;
pub use io::*;
pub use types::*;
pub use validation::*;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;

/// Top-level MyLobster configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub agent: AgentDefaultsConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub messages: MessagesConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub web: WebConfig,

    /// State directory for persistent data.
    #[serde(skip)]
    pub state_dir: PathBuf,
}

impl Config {
    /// Load configuration from file, environment, and defaults.
    pub fn load(path: Option<&str>) -> Result<Self> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| find_config_file())
            .unwrap_or_else(|| PathBuf::from("mylobster.json"));

        let mut config = if config_path.exists() {
            info!("Loading config from {}", config_path.display());
            load_config_file(&config_path)?
        } else {
            info!("No config file found, using defaults");
            Config::default()
        };

        // Apply environment variable overrides
        config.apply_env_overrides();

        // Resolve state directory
        config.state_dir = resolve_state_dir();

        Ok(config)
    }

    /// Write default configuration to a file.
    pub fn write_default(path: &str) -> Result<()> {
        let config = Config::default();
        let json = serde_json::to_string_pretty(&config)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Apply environment variable overrides to the configuration.
    fn apply_env_overrides(&mut self) {
        if let Ok(port) = std::env::var("MYLOBSTER_GATEWAY_PORT") {
            if let Ok(port) = port.parse() {
                self.gateway.port = port;
            }
        }

        if let Ok(bind) = std::env::var("MYLOBSTER_GATEWAY_BIND") {
            if let Ok(mode) = bind.parse() {
                self.gateway.bind = mode;
            }
        }

        if let Ok(token) = std::env::var("MYLOBSTER_GATEWAY_TOKEN") {
            self.gateway.auth.token = Some(token);
        }

        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            self.models.apply_anthropic_key(&key);
        }

        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            self.models.apply_openai_key(&key);
        }

        if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
            self.channels.discord.apply_token(&token);
        }

        if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
            self.channels.telegram.apply_token(&token);
        }

        if let Ok(token) = std::env::var("SLACK_BOT_TOKEN") {
            self.channels.slack.apply_bot_token(&token);
        }

        if let Ok(token) = std::env::var("SLACK_APP_TOKEN") {
            self.channels.slack.apply_app_token(&token);
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: AgentDefaultsConfig::default(),
            agents: AgentsConfig::default(),
            gateway: GatewayConfig::default(),
            channels: ChannelsConfig::default(),
            tools: ToolsConfig::default(),
            memory: MemoryConfig::default(),
            models: ModelsConfig::default(),
            plugins: PluginsConfig::default(),
            hooks: HooksConfig::default(),
            messages: MessagesConfig::default(),
            session: SessionConfig::default(),
            logging: LoggingConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            sandbox: SandboxConfig::default(),
            browser: BrowserConfig::default(),
            tts: TtsConfig::default(),
            cron: CronConfig::default(),
            web: WebConfig::default(),
            state_dir: resolve_state_dir(),
        }
    }
}

/// Find the configuration file in standard locations.
fn find_config_file() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("mylobster.json"),
        PathBuf::from("mylobster.yaml"),
        PathBuf::from("mylobster.yml"),
        PathBuf::from("mylobster.toml"),
    ];

    for path in &candidates {
        if path.exists() {
            return Some(path.clone());
        }
    }

    // Check home directory
    if let Some(home) = dirs::home_dir() {
        let home_config = home.join(".mylobster").join("config.json");
        if home_config.exists() {
            return Some(home_config);
        }
    }

    None
}

/// Resolve the state directory for persistent data.
fn resolve_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MYLOBSTER_STATE_DIR") {
        return PathBuf::from(dir);
    }

    dirs::home_dir()
        .map(|h| h.join(".mylobster"))
        .unwrap_or_else(|| PathBuf::from(".mylobster"))
}

/// Load configuration from a file path.
fn load_config_file(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;

    let config = match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => serde_yaml::from_str(&content)?,
        Some("toml") => toml::from_str(&content)?,
        Some("json") | Some("json5") | _ => {
            // Try JSON5 first, then regular JSON
            json5::from_str(&content).or_else(|_| {
                serde_json::from_str(&content).map_err(|e| json5::Error::Message {
                    msg: e.to_string(),
                    location: None,
                })
            })?
        }
    };

    Ok(config)
}
