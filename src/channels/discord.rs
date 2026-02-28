use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ============================================================================
// v2026.2.26: Slash Command Validation
// ============================================================================

/// A Discord slash command definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// Command name (1-32 chars, lowercase, no spaces).
    pub name: String,
    /// Command description (1-100 chars).
    pub description: String,
    /// Command options.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SlashCommandOption>,
}

/// A slash command option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandOption {
    /// Option name (1-32 chars, lowercase, no spaces).
    pub name: String,
    /// Option description (1-100 chars).
    pub description: String,
    /// Option type (3=string, 4=integer, 5=boolean, etc.).
    #[serde(rename = "type")]
    pub option_type: u8,
    /// Whether this option is required.
    #[serde(default)]
    pub required: bool,
}

/// Validation error for slash command definitions.
#[derive(Debug, Clone)]
pub struct SlashCommandValidationError {
    pub field: String,
    pub message: String,
}

/// Validate a slash command definition before registration.
///
/// v2026.2.26: Validates all fields to prevent Discord API errors during
/// registration. Invalid commands are logged and skipped rather than
/// causing the entire registration to fail.
pub fn validate_slash_command(cmd: &SlashCommand) -> Vec<SlashCommandValidationError> {
    let mut errors = Vec::new();

    // Name: 1-32 chars, lowercase, no spaces, matches ^[\w-]{1,32}$
    if cmd.name.is_empty() || cmd.name.len() > 32 {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: format!(
                "Command name must be 1-32 characters, got {}",
                cmd.name.len()
            ),
        });
    }

    if cmd.name != cmd.name.to_lowercase() {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must be lowercase".to_string(),
        });
    }

    if cmd.name.contains(' ') {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must not contain spaces".to_string(),
        });
    }

    if !cmd
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must only contain alphanumeric characters, hyphens, or underscores".to_string(),
        });
    }

    // Description: 1-100 chars
    if cmd.description.is_empty() || cmd.description.len() > 100 {
        errors.push(SlashCommandValidationError {
            field: "description".to_string(),
            message: format!(
                "Command description must be 1-100 characters, got {}",
                cmd.description.len()
            ),
        });
    }

    // Validate options
    for (i, opt) in cmd.options.iter().enumerate() {
        if opt.name.is_empty() || opt.name.len() > 32 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].name", i),
                message: format!(
                    "Option name must be 1-32 characters, got {}",
                    opt.name.len()
                ),
            });
        }

        if opt.name != opt.name.to_lowercase() {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].name", i),
                message: "Option name must be lowercase".to_string(),
            });
        }

        if opt.description.is_empty() || opt.description.len() > 100 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].description", i),
                message: format!(
                    "Option description must be 1-100 characters, got {}",
                    opt.description.len()
                ),
            });
        }

        // Valid option types: 1-11
        if opt.option_type == 0 || opt.option_type > 11 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].type", i),
                message: format!(
                    "Option type must be 1-11, got {}",
                    opt.option_type
                ),
            });
        }
    }

    errors
}

/// Filter and validate a list of slash commands, returning only valid ones.
///
/// Invalid commands are logged as warnings but do not prevent valid commands
/// from being registered.
pub fn filter_valid_commands(commands: Vec<SlashCommand>) -> Vec<SlashCommand> {
    commands
        .into_iter()
        .filter(|cmd| {
            let errors = validate_slash_command(cmd);
            if errors.is_empty() {
                true
            } else {
                for error in &errors {
                    warn!(
                        "Discord slash command '{}' validation error in {}: {}",
                        cmd.name, error.field, error.message
                    );
                }
                false
            }
        })
        .collect()
}

// ============================================================================
// Discord Channel Implementation
// ============================================================================

/// Discord channel implementation using serenity.
pub struct DiscordChannel {
    enabled: bool,
    bot_token: Option<String>,
}

impl DiscordChannel {
    pub fn new(config: &Config) -> Self {
        let dc = &config.channels.discord;
        let bot_token = dc.default_account.token.clone();
        let enabled = dc.default_account.enabled.unwrap_or(bot_token.is_some());

        Self { enabled, bot_token }
    }
}

#[async_trait]
impl ChannelPlugin for DiscordChannel {
    fn id(&self) -> &str {
        "discord"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Discord".to_string(),
            description: "Discord Bot channel via serenity gateway".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
            ChannelCapability::Stickers,
            ChannelCapability::Voice,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let token = match &self.bot_token {
            Some(t) => t,
            None => {
                warn!("Discord channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!(
            "Discord channel starting (token ends ...{})",
            &token[token.len().saturating_sub(4)..]
        );

        // TODO: Initialise a serenity::Client with a gateway handler.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Discord channel stopping");
            // TODO: Shut down the serenity client.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        let _token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Discord bot token not configured"))?;

        let _channel_id: u64 = to
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Discord channel_id: {to}"))?;

        info!(channel_id = to, "Discord: sending message");

        // TODO: Use serenity HTTP client to send a message to _channel_id.

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = DiscordChannel::new(config);
    channel.send_message(to, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_slash_command() {
        let cmd = SlashCommand {
            name: "help".to_string(),
            description: "Show help".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.is_empty());
    }

    #[test]
    fn slash_command_name_too_long() {
        let cmd = SlashCommand {
            name: "a".repeat(33),
            description: "Test".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.field == "name"));
    }

    #[test]
    fn slash_command_name_uppercase_rejected() {
        let cmd = SlashCommand {
            name: "Help".to_string(),
            description: "Show help".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("lowercase")));
    }

    #[test]
    fn slash_command_name_with_spaces_rejected() {
        let cmd = SlashCommand {
            name: "my command".to_string(),
            description: "Test".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("spaces")));
    }

    #[test]
    fn slash_command_empty_description_rejected() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.field == "description"));
    }

    #[test]
    fn slash_command_description_too_long_rejected() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "x".repeat(101),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.field == "description"));
    }

    #[test]
    fn slash_command_option_validation() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "Test command".to_string(),
            options: vec![SlashCommandOption {
                name: "INVALID".to_string(),
                description: "An option".to_string(),
                option_type: 3,
                required: false,
            }],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors
            .iter()
            .any(|e| e.field.starts_with("options[0]")));
    }

    #[test]
    fn slash_command_option_invalid_type() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "Test command".to_string(),
            options: vec![SlashCommandOption {
                name: "opt".to_string(),
                description: "An option".to_string(),
                option_type: 99,
                required: false,
            }],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("1-11")));
    }

    #[test]
    fn filter_valid_commands_keeps_valid() {
        let commands = vec![
            SlashCommand {
                name: "help".to_string(),
                description: "Show help".to_string(),
                options: vec![],
            },
            SlashCommand {
                name: "INVALID".to_string(),
                description: "Bad command".to_string(),
                options: vec![],
            },
            SlashCommand {
                name: "ask".to_string(),
                description: "Ask a question".to_string(),
                options: vec![SlashCommandOption {
                    name: "query".to_string(),
                    description: "Your question".to_string(),
                    option_type: 3,
                    required: true,
                }],
            },
        ];
        let valid = filter_valid_commands(commands);
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[0].name, "help");
        assert_eq!(valid[1].name, "ask");
    }

    #[test]
    fn valid_command_with_hyphen_and_underscore() {
        let cmd = SlashCommand {
            name: "my-cool_cmd".to_string(),
            description: "A cool command".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.is_empty());
    }
}
