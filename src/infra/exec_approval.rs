//! system.run approval v2 with structured argv and environment binding (v2026.2.26).
//!
//! Ported from OpenClaw `src/infra/exec-approval.ts`. This module implements
//! the approval gate that decides whether a tool execution request should be
//! allowed, denied, or requires explicit user confirmation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Types
// ============================================================================

/// Structured exec approval request (v2).
///
/// Unlike v1 which used a flat command string, v2 uses structured argv
/// and supports environment variable binding for approval matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecApprovalV2 {
    /// Structured argument vector (e.g., `["git", "push", "origin", "main"]`).
    pub argv: Vec<String>,
    /// Environment variables bound to this approval.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Original flat command string (for backwards compat with v1 policies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Working directory for the execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Turn-origin context for exec approval auditing.
///
/// Records where the execution request originated from, enabling
/// per-channel and per-account approval policies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnOrigin {
    /// Source channel (e.g., "telegram", "discord", "websocket").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_channel: Option<String>,
    /// Recipient address within the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_to: Option<String>,
    /// Account ID within the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,
    /// Thread/topic ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_thread_id: Option<String>,
}

/// An exec approval policy rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPolicy {
    /// Unique identifier for this policy.
    pub id: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Action to take: "allow", "deny", "ask".
    pub action: ApprovalAction,
    /// Command patterns to match (glob-style).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Structured argv prefix patterns.
    #[serde(default)]
    pub argv_prefixes: Vec<Vec<String>>,
    /// Environment variable requirements.
    #[serde(default)]
    pub env_requirements: HashMap<String, String>,
    /// Restrict to specific source channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_channels: Option<Vec<String>>,
    /// Restrict to specific account IDs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_ids: Option<Vec<String>>,
}

/// Action taken by an approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalAction {
    /// Allow execution without prompting.
    Allow,
    /// Deny execution.
    Deny,
    /// Require explicit user approval.
    Ask,
}

/// Result of evaluating exec approval policies.
#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    /// Whether execution is approved.
    pub approved: bool,
    /// The matching policy, if any.
    pub matched_policy: Option<String>,
    /// The action taken.
    pub action: ApprovalAction,
    /// Reason for the decision.
    pub reason: String,
}

// ============================================================================
// Error types (v2026.2.26 categorized errors)
// ============================================================================

/// Categories of exec approval errors for structured diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecApprovalErrorKind {
    /// A guard policy explicitly denied execution.
    GuardError,
    /// No matching allow policy — requires user approval.
    ApprovalRequired,
    /// A policy matched but conditions didn't align (env mismatch, etc.).
    ApprovalMismatch,
}

/// Structured exec approval error with diagnostic context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecApprovalError {
    pub kind: ExecApprovalErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ============================================================================
// Error builders
// ============================================================================

/// Build a "guard error" response when a deny policy matches.
pub fn system_run_approval_guard_error(
    policy_id: &str,
    command: &str,
) -> ExecApprovalError {
    ExecApprovalError {
        kind: ExecApprovalErrorKind::GuardError,
        message: format!(
            "Execution denied by guard policy '{policy_id}': {command}"
        ),
        policy_id: Some(policy_id.to_string()),
        data: Some(serde_json::json!({
            "command": command,
            "deniedBy": policy_id,
        })),
    }
}

/// Build an "approval required" response when no allow policy matches.
pub fn system_run_approval_required(
    command: &str,
    turn_origin: &TurnOrigin,
) -> ExecApprovalError {
    ExecApprovalError {
        kind: ExecApprovalErrorKind::ApprovalRequired,
        message: format!("User approval required to execute: {command}"),
        policy_id: None,
        data: Some(serde_json::json!({
            "command": command,
            "sourceChannel": turn_origin.source_channel,
            "sourceAccountId": turn_origin.source_account_id,
        })),
    }
}

/// Build an "approval mismatch" response when a policy matches structurally
/// but env/channel conditions don't align.
pub fn to_system_run_approval_mismatch_error(
    policy_id: &str,
    command: &str,
    mismatch_reason: &str,
) -> ExecApprovalError {
    ExecApprovalError {
        kind: ExecApprovalErrorKind::ApprovalMismatch,
        message: format!(
            "Approval policy '{policy_id}' matched but conditions failed: {mismatch_reason}"
        ),
        policy_id: Some(policy_id.to_string()),
        data: Some(serde_json::json!({
            "command": command,
            "mismatchReason": mismatch_reason,
        })),
    }
}

// ============================================================================
// Policy evaluation
// ============================================================================

/// Evaluate an exec request against a set of approval policies.
///
/// Policies are evaluated in order. The first matching policy determines
/// the outcome:
/// - `Deny` → immediately rejected
/// - `Allow` → immediately approved
/// - `Ask` → requires user confirmation
///
/// If no policy matches, the default is `Ask` (fail-safe).
pub fn evaluate_exec_approval(
    request: &ExecApprovalV2,
    policies: &[ApprovalPolicy],
    turn_origin: &TurnOrigin,
) -> ApprovalDecision {
    let command_str = request
        .command
        .clone()
        .unwrap_or_else(|| request.argv.join(" "));

    for policy in policies {
        if !matches_policy(request, policy, turn_origin) {
            continue;
        }

        match policy.action {
            ApprovalAction::Deny => {
                return ApprovalDecision {
                    approved: false,
                    matched_policy: Some(policy.id.clone()),
                    action: ApprovalAction::Deny,
                    reason: format!("Denied by policy '{}'", policy.id),
                };
            }
            ApprovalAction::Allow => {
                // Check env requirements
                if let Some(mismatch) = check_env_requirements(request, policy) {
                    return ApprovalDecision {
                        approved: false,
                        matched_policy: Some(policy.id.clone()),
                        action: ApprovalAction::Ask,
                        reason: format!(
                            "Policy '{}' matched but env mismatch: {}",
                            policy.id, mismatch
                        ),
                    };
                }
                return ApprovalDecision {
                    approved: true,
                    matched_policy: Some(policy.id.clone()),
                    action: ApprovalAction::Allow,
                    reason: format!("Allowed by policy '{}'", policy.id),
                };
            }
            ApprovalAction::Ask => {
                return ApprovalDecision {
                    approved: false,
                    matched_policy: Some(policy.id.clone()),
                    action: ApprovalAction::Ask,
                    reason: format!("Approval required by policy '{}'", policy.id),
                };
            }
        }
    }

    // No policy matched — default to Ask (fail-safe).
    ApprovalDecision {
        approved: false,
        matched_policy: None,
        action: ApprovalAction::Ask,
        reason: format!("No matching policy for: {command_str}"),
    }
}

/// Check if an exec request matches a policy's command patterns.
fn matches_policy(
    request: &ExecApprovalV2,
    policy: &ApprovalPolicy,
    turn_origin: &TurnOrigin,
) -> bool {
    // Check source channel restriction
    if let Some(ref channels) = policy.source_channels {
        match &turn_origin.source_channel {
            Some(ch) if channels.contains(ch) => {}
            _ => return false,
        }
    }

    // Check account ID restriction
    if let Some(ref accounts) = policy.account_ids {
        match &turn_origin.source_account_id {
            Some(id) if accounts.contains(id) => {}
            _ => return false,
        }
    }

    // Check argv prefix patterns
    if !policy.argv_prefixes.is_empty() {
        let matches_any = policy.argv_prefixes.iter().any(|prefix| {
            if prefix.len() > request.argv.len() {
                return false;
            }
            prefix
                .iter()
                .zip(request.argv.iter())
                .all(|(pattern, arg)| {
                    pattern == "*" || pattern == arg
                })
        });
        if matches_any {
            return true;
        }
    }

    // Check flat command patterns (glob-style)
    if !policy.patterns.is_empty() {
        let command_str = request
            .command
            .as_deref()
            .unwrap_or_else(|| {
                // Can't return a reference to a local, so this is a best-effort
                // match against the first arg.
                request.argv.first().map(|s| s.as_str()).unwrap_or("")
            });

        return policy.patterns.iter().any(|pattern| {
            glob_match(pattern, command_str)
        });
    }

    // Policy has no patterns — matches everything.
    policy.argv_prefixes.is_empty() && policy.patterns.is_empty()
}

/// Simple glob matching (supports `*` as wildcard).
fn glob_match(pattern: &str, input: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix('*') {
        return input.ends_with(suffix);
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        return input.starts_with(prefix);
    }

    if pattern.contains('*') {
        // Split on first * and match prefix + suffix
        let parts: Vec<&str> = pattern.splitn(2, '*').collect();
        if parts.len() == 2 {
            return input.starts_with(parts[0]) && input.ends_with(parts[1]);
        }
    }

    pattern == input
}

/// Check environment variable requirements in a policy.
/// Returns `Some(reason)` if there's a mismatch, `None` if all requirements met.
fn check_env_requirements(
    request: &ExecApprovalV2,
    policy: &ApprovalPolicy,
) -> Option<String> {
    for (key, expected_value) in &policy.env_requirements {
        match request.env.get(key) {
            Some(actual) if actual == expected_value => continue,
            Some(actual) => {
                return Some(format!(
                    "env.{key}: expected '{expected_value}', got '{actual}'"
                ));
            }
            None => {
                return Some(format!("env.{key} not set (expected '{expected_value}')"));
            }
        }
    }
    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(argv: &[&str]) -> ExecApprovalV2 {
        ExecApprovalV2 {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: HashMap::new(),
            command: Some(argv.join(" ")),
            cwd: None,
            timeout_ms: None,
        }
    }

    fn make_origin() -> TurnOrigin {
        TurnOrigin::default()
    }

    // ====================================================================
    // evaluate_exec_approval
    // ====================================================================

    #[test]
    fn no_policies_defaults_to_ask() {
        let req = make_request(&["ls", "-la"]);
        let decision = evaluate_exec_approval(&req, &[], &make_origin());
        assert!(!decision.approved);
        assert_eq!(decision.action, ApprovalAction::Ask);
    }

    #[test]
    fn deny_policy_rejects() {
        let policies = vec![ApprovalPolicy {
            id: "deny-rm".to_string(),
            description: None,
            action: ApprovalAction::Deny,
            patterns: vec!["rm *".to_string()],
            argv_prefixes: vec![],
            env_requirements: HashMap::new(),
            source_channels: None,
            account_ids: None,
        }];
        let req = make_request(&["rm", "-rf", "/"]);
        let decision = evaluate_exec_approval(&req, &policies, &make_origin());
        assert!(!decision.approved);
        assert_eq!(decision.action, ApprovalAction::Deny);
        assert_eq!(decision.matched_policy.as_deref(), Some("deny-rm"));
    }

    #[test]
    fn allow_policy_approves() {
        let policies = vec![ApprovalPolicy {
            id: "allow-git".to_string(),
            description: None,
            action: ApprovalAction::Allow,
            patterns: vec![],
            argv_prefixes: vec![vec!["git".to_string()]],
            env_requirements: HashMap::new(),
            source_channels: None,
            account_ids: None,
        }];
        let req = make_request(&["git", "status"]);
        let decision = evaluate_exec_approval(&req, &policies, &make_origin());
        assert!(decision.approved);
        assert_eq!(decision.action, ApprovalAction::Allow);
    }

    #[test]
    fn argv_prefix_matching() {
        let policies = vec![ApprovalPolicy {
            id: "allow-cargo".to_string(),
            description: None,
            action: ApprovalAction::Allow,
            patterns: vec![],
            argv_prefixes: vec![vec!["cargo".to_string(), "build".to_string()]],
            env_requirements: HashMap::new(),
            source_channels: None,
            account_ids: None,
        }];

        let req_build = make_request(&["cargo", "build", "--release"]);
        assert!(evaluate_exec_approval(&req_build, &policies, &make_origin()).approved);

        let req_test = make_request(&["cargo", "test"]);
        assert!(!evaluate_exec_approval(&req_test, &policies, &make_origin()).approved);
    }

    #[test]
    fn env_requirement_mismatch() {
        let mut env_req = HashMap::new();
        env_req.insert("NODE_ENV".to_string(), "production".to_string());

        let policies = vec![ApprovalPolicy {
            id: "allow-deploy".to_string(),
            description: None,
            action: ApprovalAction::Allow,
            patterns: vec!["deploy*".to_string()],
            argv_prefixes: vec![],
            env_requirements: env_req,
            source_channels: None,
            account_ids: None,
        }];

        let mut req = make_request(&["deploy"]);
        req.env
            .insert("NODE_ENV".to_string(), "development".to_string());
        let decision = evaluate_exec_approval(&req, &policies, &make_origin());
        assert!(!decision.approved);
    }

    #[test]
    fn source_channel_restriction() {
        let policies = vec![ApprovalPolicy {
            id: "ws-only".to_string(),
            description: None,
            action: ApprovalAction::Allow,
            patterns: vec!["*".to_string()],
            argv_prefixes: vec![],
            env_requirements: HashMap::new(),
            source_channels: Some(vec!["websocket".to_string()]),
            account_ids: None,
        }];

        let req = make_request(&["ls"]);
        let tg_origin = TurnOrigin {
            source_channel: Some("telegram".to_string()),
            ..Default::default()
        };
        assert!(!evaluate_exec_approval(&req, &policies, &tg_origin).approved);

        let ws_origin = TurnOrigin {
            source_channel: Some("websocket".to_string()),
            ..Default::default()
        };
        assert!(evaluate_exec_approval(&req, &policies, &ws_origin).approved);
    }

    // ====================================================================
    // Error builders
    // ====================================================================

    #[test]
    fn guard_error_format() {
        let err = system_run_approval_guard_error("no-rm", "rm -rf /");
        assert_eq!(err.kind, ExecApprovalErrorKind::GuardError);
        assert!(err.message.contains("no-rm"));
        assert_eq!(err.policy_id.as_deref(), Some("no-rm"));
    }

    #[test]
    fn approval_required_format() {
        let origin = TurnOrigin {
            source_channel: Some("telegram".to_string()),
            ..Default::default()
        };
        let err = system_run_approval_required("curl example.com", &origin);
        assert_eq!(err.kind, ExecApprovalErrorKind::ApprovalRequired);
        assert!(err.message.contains("curl example.com"));
    }

    #[test]
    fn mismatch_error_format() {
        let err = to_system_run_approval_mismatch_error(
            "deploy-prod",
            "deploy app",
            "env.NODE_ENV mismatch",
        );
        assert_eq!(err.kind, ExecApprovalErrorKind::ApprovalMismatch);
        assert!(err.message.contains("deploy-prod"));
    }

    // ====================================================================
    // glob_match
    // ====================================================================

    #[test]
    fn glob_wildcard_matches_all() {
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn glob_prefix_match() {
        assert!(glob_match("git *", "git push"));
        assert!(!glob_match("git *", "cargo build"));
    }

    #[test]
    fn glob_suffix_match() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.ts"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("ls", "lsof"));
    }

    // ====================================================================
    // ExecApprovalV2 serialization
    // ====================================================================

    #[test]
    fn exec_approval_v2_camel_case() {
        let req = ExecApprovalV2 {
            argv: vec!["git".into(), "push".into()],
            env: HashMap::new(),
            command: None,
            cwd: Some("/home/user".into()),
            timeout_ms: Some(30000),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("argv").is_some());
        assert!(v.get("timeoutMs").is_some());
        assert!(v.get("command").is_none()); // skip_serializing_if
    }

    #[test]
    fn turn_origin_serialization() {
        let origin = TurnOrigin {
            source_channel: Some("telegram".into()),
            source_to: Some("12345".into()),
            source_account_id: None,
            source_thread_id: None,
        };
        let v = serde_json::to_value(&origin).unwrap();
        assert_eq!(v["sourceChannel"], "telegram");
        assert_eq!(v["sourceTo"], "12345");
        assert!(v.get("sourceAccountId").is_none());
    }
}
