//! `channels.stop` RPC (v2026.5.2 parity).
//!
//! Stops one channel (or all channels with `channel: "*"`) without a gateway
//! restart. The stopped set is tracked so `channels.status` consumers and the
//! health snapshot cache can observe the divergence.

use crate::gateway::protocol::{OcResponseFrame, RequestFrame};
use crate::gateway::server::GatewayState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelsStopParams {
    pub channel: String,
    pub account_id: Option<String>,
}

/// Parse and validate `channels.stop` params.
pub fn parse_channels_stop_params(
    params: Option<&serde_json::Value>,
) -> Result<ChannelsStopParams, String> {
    let p = params.ok_or("missing params")?;
    let channel = p
        .get("channel")
        .and_then(|v| v.as_str())
        .ok_or("missing 'channel' (string)")?
        .trim()
        .to_string();
    if channel.is_empty() {
        return Err("'channel' must be non-empty".to_string());
    }
    // Channel ids are lowercase identifiers or the "*" wildcard.
    if channel != "*"
        && !channel
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!("invalid channel id: {channel}"));
    }
    Ok(ChannelsStopParams {
        channel,
        account_id: p
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

pub async fn handle_channels_stop(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = match parse_channels_stop_params(request.params.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("Invalid channels.stop params: {e}"),
                Some(-32602),
            )
        }
    };

    let stopped: Vec<String> = if params.channel == "*" {
        match state.channels.stop_all().await {
            Ok(()) => {
                let mut set = state.rpc.stopped_channels.write();
                set.insert("*".to_string());
                vec!["*".to_string()]
            }
            Err(e) => {
                return OcResponseFrame::error(
                    request.id.clone(),
                    format!("failed to stop channels: {e}"),
                    Some(-32603),
                )
            }
        }
    } else {
        match state.channels.get_plugin(&params.channel).await {
            Some(plugin) => {
                if let Err(e) = plugin.stop_account().await {
                    return OcResponseFrame::error(
                        request.id.clone(),
                        format!("failed to stop channel '{}': {e}", params.channel),
                        Some(-32603),
                    );
                }
                state
                    .rpc
                    .stopped_channels
                    .write()
                    .insert(params.channel.clone());
                vec![params.channel.clone()]
            }
            None => {
                return OcResponseFrame::error(
                    request.id.clone(),
                    format!("channel not found or not running: {}", params.channel),
                    Some(-32600),
                )
            }
        }
    };

    // Channel runtime state diverged → force health snapshot refresh.
    state.rpc.health_cache.invalidate();

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "ok": true, "stopped": stopped }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_valid_channel() {
        let p = parse_channels_stop_params(Some(&json!({"channel": "telegram"}))).unwrap();
        assert_eq!(p.channel, "telegram");
        assert!(p.account_id.is_none());
    }

    #[test]
    fn parse_wildcard() {
        let p = parse_channels_stop_params(Some(&json!({"channel": "*"}))).unwrap();
        assert_eq!(p.channel, "*");
    }

    #[test]
    fn parse_account_scoped() {
        let p = parse_channels_stop_params(Some(
            &json!({"channel": "discord", "accountId": "work"}),
        ))
        .unwrap();
        assert_eq!(p.account_id.as_deref(), Some("work"));
    }

    #[test]
    fn parse_rejects_missing_or_invalid() {
        assert!(parse_channels_stop_params(None).is_err());
        assert!(parse_channels_stop_params(Some(&json!({}))).is_err());
        assert!(parse_channels_stop_params(Some(&json!({"channel": ""}))).is_err());
        assert!(parse_channels_stop_params(Some(&json!({"channel": "Bad Channel!"}))).is_err());
        assert!(parse_channels_stop_params(Some(&json!({"channel": "UPPER"}))).is_err());
    }
}
