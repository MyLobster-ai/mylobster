//! Bundled Policy checks (channel conformance / ingress / data handling).
//!
//! Compact behavior port of OpenClaw's bundled Policy plugin
//! (`extensions/policy`, v2026.5.x–6.2): a set of static conformance checks
//! over the loaded config that produce doctor-style lint findings. The
//! upstream plugin also offers opt-in workspace repair; this port surfaces
//! findings only (repair belongs to `doctor --fix`, CLI cluster — HANDOFF:
//! `infra/doctor.rs` can render [`run_policy_checks`] findings directly).

use crate::config::Config;

/// Severity of a policy finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySeverity {
    Info,
    Warning,
    Error,
}

/// One policy lint finding.
#[derive(Debug, Clone)]
pub struct PolicyFinding {
    pub check: &'static str,
    pub severity: PolicySeverity,
    pub message: String,
}

/// Run the bundled policy checks over a loaded config.
pub fn run_policy_checks(config: &Config) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    check_channel_conformance(config, &mut findings);
    check_ingress_posture(config, &mut findings);
    check_data_handling(config, &mut findings);
    findings
}

/// Channel-conformance: channels with open DM policies and no allowlist.
fn check_channel_conformance(config: &Config, findings: &mut Vec<PolicyFinding>) {
    // Extension-map channels: `{"enabled": true}` with no allowFrom and an
    // open dmPolicy is flagged.
    for (channel, entry) in &config.channels.extensions {
        let Some(obj) = entry.as_object() else { continue };
        if matches!(obj.get("enabled"), Some(serde_json::Value::Bool(true))) {
            let dm_policy = obj.get("dmPolicy").and_then(|v| v.as_str()).unwrap_or("");
            let has_allowlist = obj
                .get("allowFrom")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if dm_policy == "open" && !has_allowlist {
                findings.push(PolicyFinding {
                    check: "channel-conformance",
                    severity: PolicySeverity::Warning,
                    message: format!(
                        "channels.{channel}: dmPolicy \"open\" without an allowFrom list — \
                         any sender can start DMs"
                    ),
                });
            }
        }
    }
}

/// Ingress posture: unresolved access-group references fail closed but are
/// surfaced so operators can fix the config.
fn check_ingress_posture(config: &Config, findings: &mut Vec<PolicyFinding>) {
    let configured: Vec<&str> = config
        .access_groups
        .as_ref()
        .map(|g| g.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    for (channel, entry) in &config.channels.extensions {
        let Some(list) = entry.get("allowFrom").and_then(|v| v.as_array()) else {
            continue;
        };
        for value in list {
            let Some(s) = value.as_str() else { continue };
            if let Some(name) = super::access_groups::parse_access_group_entry(s) {
                if !configured.contains(&name) {
                    findings.push(PolicyFinding {
                        check: "ingress",
                        severity: PolicySeverity::Warning,
                        message: format!(
                            "channels.{channel}.allowFrom references unknown access group \
                             \"{name}\" (entry matches nothing; configure accessGroups.{name})"
                        ),
                    });
                }
            }
        }
    }
}

/// Data handling: plaintext secrets in extension-channel config values.
fn check_data_handling(config: &Config, findings: &mut Vec<PolicyFinding>) {
    const SECRET_KEYS: &[&str] = &["token", "botToken", "appToken", "apiKey", "password"];
    for (channel, entry) in &config.channels.extensions {
        let Some(obj) = entry.as_object() else { continue };
        for key in SECRET_KEYS {
            if let Some(serde_json::Value::String(v)) = obj.get(*key) {
                if !v.is_empty() && !v.starts_with("${") && !v.starts_with("secretRef:") {
                    findings.push(PolicyFinding {
                        check: "data-handling",
                        severity: PolicySeverity::Info,
                        message: format!(
                            "channels.{channel}.{key} holds a plaintext secret; consider a \
                             SecretRef or environment reference"
                        ),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_with_extension(channel: &str, value: serde_json::Value) -> Config {
        let mut config = Config::default();
        config
            .channels
            .extensions
            .insert(channel.to_string(), value);
        config
    }

    #[test]
    fn open_dm_without_allowlist_flagged() {
        let config = config_with_extension("mattermost", json!({"enabled": true, "dmPolicy": "open"}));
        let findings = run_policy_checks(&config);
        assert!(findings
            .iter()
            .any(|f| f.check == "channel-conformance" && f.message.contains("mattermost")));
    }

    #[test]
    fn unknown_access_group_reference_flagged() {
        let config = config_with_extension(
            "matrix",
            json!({"enabled": true, "dmPolicy": "allowlist", "allowFrom": ["accessGroup:ghost"]}),
        );
        let findings = run_policy_checks(&config);
        assert!(findings
            .iter()
            .any(|f| f.check == "ingress" && f.message.contains("ghost")));
    }

    #[test]
    fn plaintext_secret_flagged_and_refs_ignored() {
        let config = config_with_extension(
            "line",
            json!({"enabled": true, "token": "plain-secret", "apiKey": "secretRef:line-key"}),
        );
        let findings = run_policy_checks(&config);
        let data: Vec<_> = findings
            .iter()
            .filter(|f| f.check == "data-handling")
            .collect();
        assert_eq!(data.len(), 1);
        assert!(data[0].message.contains("token"));
    }

    #[test]
    fn clean_config_no_findings() {
        let config = Config::default();
        assert!(run_policy_checks(&config).is_empty());
    }
}
