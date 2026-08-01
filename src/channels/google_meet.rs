//! Google Meet channel (meeting bot: join, listen, captions, speak).
//!
//! Ported from OpenClaw `extensions/google-meet`. Scaffold — being fleshed
//! out by the channels cluster (accessType/entryPointAccess, end-active-
//! conference, test-listen, caption health).

use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

pub struct GoogleMeetChannel {
    enabled: bool,
}

impl GoogleMeetChannel {
    pub fn new(config: &Config) -> Self {
        let enabled = config
            .channels
            .extensions
            .get("googlemeet")
            .or_else(|| config.channels.extensions.get("google-meet"))
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for GoogleMeetChannel {
    fn id(&self) -> &str {
        "googlemeet"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Google Meet".to_string(),
            description: "Google Meet meeting-bot channel".to_string(),
            enabled: self.enabled,
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![ChannelCapability::Voice, ChannelCapability::ReceiveText]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if self.enabled {
            info!("Google Meet channel starting (scaffold)");
        }
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "GoogleMeet: send (scaffold, not implemented)");
        Ok(())
    }
}
