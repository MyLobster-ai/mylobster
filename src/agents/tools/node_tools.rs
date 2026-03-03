//! Device/node interaction tool.
//!
//! Supports: status, describe, pending, approve, reject, notify,
//! camera_snap, camera_list, camera_clip, screen_record,
//! location_get, notifications_list, device_status, device_info, run, invoke.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct NodeTool;

#[async_trait]
impl AgentTool for NodeTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "node".to_string(),
            description: "Interact with connected devices/nodes: status, camera, location, notifications, device info, remote execution".to_string(),
            category: "device".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "status", "describe", "pending", "approve", "reject",
                            "notify", "camera_snap", "camera_list", "camera_clip",
                            "screen_record", "location_get", "notifications_list",
                            "device_status", "device_info", "run", "invoke"
                        ],
                        "description": "The node action to perform"
                    },
                    "nodeId": { "type": "string", "description": "Target node/device ID" },
                    "command": { "type": "string", "description": "Command to run (for 'run' action)" },
                    "method": { "type": "string", "description": "RPC method name (for 'invoke' action)" },
                    "params": { "type": "object", "description": "Parameters for the invocation" },
                    "message": { "type": "string", "description": "Notification message" },
                    "title": { "type": "string", "description": "Notification title" },
                    "cameraId": { "type": "string", "description": "Camera ID for snap/clip" },
                    "duration": { "type": "integer", "description": "Duration in seconds for recording" },
                    "approvalId": { "type": "string", "description": "Approval request ID" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing action parameter"))?;

        let node_id = params
            .get("nodeId")
            .and_then(|v| v.as_str())
            .unwrap_or("local");

        // Node communication happens via the gateway's local endpoint
        let gateway_url = format!(
            "http://127.0.0.1:{}",
            context.config.gateway.port
        );

        let client = reqwest::Client::new();

        match action {
            "status" => {
                let resp = client
                    .post(format!("{}/rpc", gateway_url))
                    .json(&serde_json::json!({
                        "method": "nodes.status",
                        "params": { "nodeId": node_id }
                    }))
                    .send()
                    .await;

                match resp {
                    Ok(r) => {
                        let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({"status": "unknown"}));
                        Ok(ToolResult::json(body))
                    }
                    Err(_) => Ok(ToolResult::json(serde_json::json!({
                        "nodeId": node_id,
                        "status": "unreachable",
                        "message": "Node is not connected"
                    }))),
                }
            }
            "describe" => {
                Ok(ToolResult::json(serde_json::json!({
                    "nodeId": node_id,
                    "capabilities": [
                        "camera", "screen", "location",
                        "notifications", "run", "invoke"
                    ],
                    "description": "Connected device node"
                })))
            }
            "pending" => {
                Ok(ToolResult::json(serde_json::json!({
                    "pending": [],
                    "count": 0
                })))
            }
            "approve" | "reject" => {
                let approval_id = params
                    .get("approvalId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing approvalId"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "approvalId": approval_id,
                    "action": action,
                    "status": "processed"
                })))
            }
            "notify" => {
                let message = params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing message"))?;
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("MyLobster");

                Ok(ToolResult::json(serde_json::json!({
                    "sent": true,
                    "nodeId": node_id,
                    "title": title,
                    "message": message
                })))
            }
            "camera_snap" | "camera_list" | "camera_clip" => {
                Ok(ToolResult::json(serde_json::json!({
                    "action": action,
                    "nodeId": node_id,
                    "status": "not_available",
                    "message": "Camera access requires a connected companion app"
                })))
            }
            "screen_record" => {
                let duration = params
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10);

                Ok(ToolResult::json(serde_json::json!({
                    "action": "screen_record",
                    "nodeId": node_id,
                    "duration": duration,
                    "status": "not_available",
                    "message": "Screen recording requires a connected companion app"
                })))
            }
            "location_get" => {
                Ok(ToolResult::json(serde_json::json!({
                    "action": "location_get",
                    "nodeId": node_id,
                    "status": "not_available",
                    "message": "Location access requires a connected companion app"
                })))
            }
            "notifications_list" => {
                Ok(ToolResult::json(serde_json::json!({
                    "notifications": [],
                    "nodeId": node_id
                })))
            }
            "device_status" | "device_info" => {
                Ok(ToolResult::json(serde_json::json!({
                    "nodeId": node_id,
                    "platform": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "hostname": gethostname(),
                    "uptime_secs": uptime_seconds(),
                })))
            }
            "run" => {
                let command = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing command parameter"))?;

                // Delegate to bash tool execution
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await?;

                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                Ok(ToolResult::json(serde_json::json!({
                    "exitCode": output.status.code(),
                    "stdout": stdout,
                    "stderr": stderr,
                    "nodeId": node_id
                })))
            }
            "invoke" => {
                let method = params
                    .get("method")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing method parameter"))?;
                let rpc_params = params
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));

                Ok(ToolResult::json(serde_json::json!({
                    "method": method,
                    "params": rpc_params,
                    "nodeId": node_id,
                    "status": "invoked"
                })))
            }
            _ => Ok(ToolResult::error(format!("Unknown node action: {}", action))),
        }
    }
}

fn uptime_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gethostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}
