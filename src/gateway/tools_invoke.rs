//! `tools.invoke` RPC (v2026.5.2 parity).
//!
//! SDK-facing tool invocation with the shared tool policy and typed
//! approval/refusal payloads. Exec-class tools require an explicit
//! `approved: true` flag (typed `approval_required` refusal otherwise);
//! unknown tools and policy-denied tools return typed refusals rather than
//! opaque errors.

use crate::agents::tools::{self, AgentTool, ToolContext};
use crate::gateway::protocol::{OcResponseFrame, RequestFrame};
use crate::gateway::server::GatewayState;

// ============================================================================
// Params
// ============================================================================

#[derive(Debug, Clone)]
pub struct ToolsInvokeParams {
    pub tool: String,
    pub args: serde_json::Value,
    pub session_key: String,
    pub dry_run: bool,
    pub approved: bool,
}

/// Parse and validate `tools.invoke` params.
pub fn parse_tools_invoke_params(
    params: Option<&serde_json::Value>,
) -> Result<ToolsInvokeParams, String> {
    let p = params.ok_or("missing params")?;
    let tool = p
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or("missing 'tool' (string)")?
        .trim()
        .to_string();
    if tool.is_empty() {
        return Err("'tool' must be non-empty".to_string());
    }
    let args = p.get("args").cloned().unwrap_or(serde_json::json!({}));
    if !args.is_object() {
        return Err("'args' must be an object".to_string());
    }
    Ok(ToolsInvokeParams {
        tool,
        args,
        session_key: p
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .unwrap_or("tools-invoke")
            .to_string(),
        dry_run: p.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(false),
        approved: p.get("approved").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

// ============================================================================
// Typed approval / refusal
// ============================================================================

/// Exec-class tools that require explicit approval on `tools.invoke`.
pub const APPROVAL_REQUIRED_TOOLS: &[&str] = &["system_run", "browser", "sessions_spawn"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeRefusal {
    UnknownTool(String),
    Denied { tool: String, reason: String },
    ApprovalRequired { tool: String },
}

impl InvokeRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            InvokeRefusal::UnknownTool(_) => "unknown_tool",
            InvokeRefusal::Denied { .. } => "denied",
            InvokeRefusal::ApprovalRequired { .. } => "approval_required",
        }
    }

    pub fn message(&self) -> String {
        match self {
            InvokeRefusal::UnknownTool(t) => format!("unknown tool: {t}"),
            InvokeRefusal::Denied { tool, reason } => {
                format!("tool '{tool}' denied by policy: {reason}")
            }
            InvokeRefusal::ApprovalRequired { tool } => format!(
                "tool '{tool}' requires approval; re-invoke with approved:true after operator confirmation"
            ),
        }
    }

    pub fn tool(&self) -> &str {
        match self {
            InvokeRefusal::UnknownTool(t) => t,
            InvokeRefusal::Denied { tool, .. } => tool,
            InvokeRefusal::ApprovalRequired { tool } => tool,
        }
    }

    /// Typed refusal payload (success frame with `status: "refused"` so SDK
    /// callers can distinguish refusals from transport errors).
    pub fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "status": "refused",
            "refusal": {
                "code": self.code(),
                "tool": self.tool(),
                "message": self.message(),
            }
        })
    }
}

/// Evaluate the shared invoke policy: deny-list wins, then allow-list (empty
/// allow-list = all allowed), then approval gating for exec-class tools.
pub fn evaluate_invoke_policy(
    tool: &str,
    allow: &[String],
    deny: &[String],
    approved: bool,
) -> Result<(), InvokeRefusal> {
    if deny.iter().any(|d| d == tool) {
        return Err(InvokeRefusal::Denied {
            tool: tool.to_string(),
            reason: "listed in deny list".to_string(),
        });
    }
    if !allow.is_empty() && !allow.iter().any(|a| a == tool) {
        return Err(InvokeRefusal::Denied {
            tool: tool.to_string(),
            reason: "not in allow list".to_string(),
        });
    }
    if APPROVAL_REQUIRED_TOOLS.contains(&tool) && !approved {
        return Err(InvokeRefusal::ApprovalRequired {
            tool: tool.to_string(),
        });
    }
    Ok(())
}

// ============================================================================
// Tool factory
// ============================================================================

/// Instantiate an invokable tool by name.
fn build_tool(name: &str) -> Option<Box<dyn AgentTool>> {
    match name {
        "web_fetch" => Some(Box::new(tools::web_fetch::WebFetchTool)),
        "web_search" => Some(Box::new(tools::web_search::WebSearchTool)),
        "memory_store" => Some(Box::new(tools::memory_tool::MemoryStoreTool)),
        "memory_search" => Some(Box::new(tools::memory_tool::MemorySearchTool)),
        "system_run" => Some(Box::new(tools::bash::BashTool)),
        _ => None,
    }
}

/// Whether a tool name can be invoked through `tools.invoke`.
pub fn is_invokable_tool(name: &str) -> bool {
    build_tool(name).is_some()
}

// ============================================================================
// Handler
// ============================================================================

pub async fn handle_tools_invoke(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = match parse_tools_invoke_params(request.params.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            return OcResponseFrame::error(
                request.id.clone(),
                format!("Invalid tools.invoke params: {e}"),
                Some(-32602),
            )
        }
    };

    if !is_invokable_tool(&params.tool) {
        return OcResponseFrame::success(
            request.id.clone(),
            InvokeRefusal::UnknownTool(params.tool.clone()).to_payload(),
        );
    }

    // Shared policy (deny/allow currently sourced from gateway node command
    // policy; empty lists = allow all non-exec tools).
    let config = state.config.read().await;
    let (allow, deny): (Vec<String>, Vec<String>) = config
        .gateway
        .nodes
        .as_ref()
        .map(|n| (n.allow_commands.clone(), n.deny_commands.clone()))
        .unwrap_or_default();

    if let Err(refusal) =
        evaluate_invoke_policy(&params.tool, &allow, &deny, params.approved)
    {
        return OcResponseFrame::success(request.id.clone(), refusal.to_payload());
    }

    if params.dry_run {
        return OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({
                "status": "ok",
                "dryRun": true,
                "tool": params.tool,
            }),
        );
    }

    let tool = build_tool(&params.tool).expect("checked invokable");
    let ctx = ToolContext {
        session_key: params.session_key.clone(),
        agent_id: "default".to_string(),
        config: config.clone(),
    };
    drop(config);

    match tool.execute(params.args, &ctx).await {
        Ok(result) => {
            let is_error = result.is_error;
            OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({
                    "status": if is_error { "error" } else { "ok" },
                    "tool": params.tool,
                    "result": result,
                }),
            )
        }
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("tools.invoke '{}' failed: {e}", params.tool),
            Some(-32603),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- param parsing ----

    #[test]
    fn parse_valid_params() {
        let raw = json!({"tool": "web_search", "args": {"query": "rust"}, "dryRun": true});
        let p = parse_tools_invoke_params(Some(&raw)).unwrap();
        assert_eq!(p.tool, "web_search");
        assert!(p.dry_run);
        assert!(!p.approved);
        assert_eq!(p.session_key, "tools-invoke");
    }

    #[test]
    fn parse_missing_tool_rejected() {
        assert!(parse_tools_invoke_params(Some(&json!({"args": {}}))).is_err());
        assert!(parse_tools_invoke_params(None).is_err());
        assert!(parse_tools_invoke_params(Some(&json!({"tool": "  "}))).is_err());
    }

    #[test]
    fn parse_non_object_args_rejected() {
        let raw = json!({"tool": "web_search", "args": "not-an-object"});
        assert!(parse_tools_invoke_params(Some(&raw)).is_err());
    }

    #[test]
    fn parse_defaults_args_to_empty_object() {
        let p = parse_tools_invoke_params(Some(&json!({"tool": "web_search"}))).unwrap();
        assert_eq!(p.args, json!({}));
    }

    // ---- policy ----

    #[test]
    fn policy_deny_wins() {
        let err = evaluate_invoke_policy(
            "web_fetch",
            &["web_fetch".into()],
            &["web_fetch".into()],
            false,
        )
        .unwrap_err();
        assert_eq!(err.code(), "denied");
    }

    #[test]
    fn policy_allowlist_restricts() {
        assert!(evaluate_invoke_policy("web_fetch", &["web_fetch".into()], &[], false).is_ok());
        let err =
            evaluate_invoke_policy("web_search", &["web_fetch".into()], &[], false).unwrap_err();
        assert_eq!(err.code(), "denied");
    }

    #[test]
    fn policy_empty_allow_permits_all_non_exec() {
        assert!(evaluate_invoke_policy("web_fetch", &[], &[], false).is_ok());
        assert!(evaluate_invoke_policy("memory_search", &[], &[], false).is_ok());
    }

    #[test]
    fn policy_exec_requires_approval() {
        let err = evaluate_invoke_policy("system_run", &[], &[], false).unwrap_err();
        assert_eq!(err.code(), "approval_required");
        assert!(evaluate_invoke_policy("system_run", &[], &[], true).is_ok());
    }

    // ---- refusal payload shape ----

    #[test]
    fn refusal_payload_is_typed() {
        let payload = InvokeRefusal::ApprovalRequired {
            tool: "system_run".into(),
        }
        .to_payload();
        assert_eq!(payload["status"], "refused");
        assert_eq!(payload["refusal"]["code"], "approval_required");
        assert_eq!(payload["refusal"]["tool"], "system_run");
        assert!(payload["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("approved:true"));
    }

    #[test]
    fn unknown_tool_refusal() {
        let r = InvokeRefusal::UnknownTool("nope".into());
        assert_eq!(r.code(), "unknown_tool");
        assert_eq!(r.to_payload()["refusal"]["tool"], "nope");
    }

    // ---- factory ----

    #[test]
    fn invokable_tools_cover_core_set() {
        for t in ["web_fetch", "web_search", "memory_store", "memory_search", "system_run"] {
            assert!(is_invokable_tool(t), "{t}");
        }
        assert!(!is_invokable_tool("does_not_exist"));
    }
}
