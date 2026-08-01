//! Tencent Yuanbao channel.
//!
//! Ported from OpenClaw's Yuanbao plugin (`docs/channels/yuanbao.md`).
//! Scaffold — being fleshed out by the channels cluster.

use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

pub struct YuanbaoChannel {
    enabled: bool,
}

impl YuanbaoChannel {
    pub fn new(config: &Config) -> Self {
        let enabled = config
            .channels
            .extensions
            .get("yuanbao")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for YuanbaoChannel {
    fn id(&self) -> &str {
        "yuanbao"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Yuanbao".to_string(),
            description: "Tencent Yuanbao channel".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![ChannelCapability::SendText, ChannelCapability::ReceiveText]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if self.enabled {
            info!("Yuanbao channel starting (scaffold)");
        }
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "Yuanbao: send (scaffold, not implemented)");
        Ok(())
    }
}
