//! `/dock-<channel>` route switching for direct chats.
//!
//! Ported from OpenClaw `src/auto-reply/reply/commands-dock.ts` (v2026.5.2):
//! docking rebinds a session's reply route (`lastChannel`/`lastTo`/
//! `lastAccountId`) to a linked peer on another channel. Key behaviors:
//!
//! - Dock commands are only honored **from direct chats** ("/dock-* route
//!   switches start from direct chats only").
//! - Docking to the current channel is a no-op ("Already docked").
//! - The target peer is resolved through `session.identityLinks`: an identity
//!   group must contain one of the source sender's identity candidates (raw
//!   or `<sourceChannel>:<peer>` scoped, matched case-insensitively) AND a
//!   `<targetChannel>:<peer>` entry (peer returned with original casing).
//! - Unauthorized senders are silently dropped; missing sender ids, missing
//!   identity links, and missing session entries produce explanatory replies.

use std::collections::HashMap;

/// Parsed dock command target channel.
///
/// Accepts `/dock-<channel>` optionally followed by `@BotName` (mirrors the
/// command-registry `dock:<target>` keys, category `docks`).
pub fn parse_dock_command(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix("/dock-")?;
    // Strip an optional trailing @BotName mention.
    let body = body.split_whitespace().next().unwrap_or("");
    let body = body.split('@').next().unwrap_or("");
    let target = body.trim().to_lowercase();
    if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(target)
}

/// Inputs for resolving a dock command.
#[derive(Debug, Clone, Default)]
pub struct DockRequest {
    /// Channel the dock command should switch replies to.
    pub target_channel: String,
    /// Channel the command was received on.
    pub source_channel: String,
    /// Chat type of the source conversation ("direct", "group", …).
    pub chat_type: String,
    /// Whether the sender passed command authorization.
    pub is_authorized_sender: bool,
    /// Source peer identity candidates (sender id, e164, username, from, …).
    pub sender_candidates: Vec<String>,
    /// `session.identityLinks` (link-name → identity ids).
    pub identity_links: Option<HashMap<String, Vec<String>>>,
    /// Whether an active session entry exists for the session key.
    pub has_session_entry: bool,
    /// Default account id of the target channel.
    pub target_default_account_id: String,
}

/// Route mutation produced by a successful dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockRouteMutation {
    pub last_channel: String,
    pub last_to: String,
    pub last_account_id: String,
}

/// Outcome of a dock command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockDecision {
    /// Sender not authorized: swallow silently (upstream returns no reply).
    Unauthorized,
    /// Already docked to the requested channel.
    AlreadyDocked { reply: String },
    /// Docking is only available from direct chats.
    NotDirectChat { reply: String },
    /// No usable sender identity candidate.
    MissingSenderId { reply: String },
    /// No identity link pairs the sender with a target-channel peer.
    NoLinkedIdentity { reply: String },
    /// No active session entry to mutate.
    NoSessionEntry { reply: String },
    /// Dock succeeded: apply the route mutation and confirm.
    Docked {
        mutation: DockRouteMutation,
        reply: String,
    },
}

/// Build the source identity candidate set (lowercased raw + channel-scoped).
fn source_identity_candidates(source_channel: &str, candidates: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let channel = source_channel.trim().to_lowercase();
    for peer in candidates {
        let raw = peer.trim().to_lowercase();
        if raw.is_empty() {
            continue;
        }
        if !out.contains(&raw) {
            out.push(raw.clone());
        }
        if !channel.is_empty() {
            let scoped = format!("{channel}:{raw}");
            if !out.contains(&scoped) {
                out.push(scoped);
            }
        }
    }
    out
}

/// Find a `<targetChannel>:<peer>` id in an identity-link group that also
/// contains one of the source candidates. Peer casing is preserved.
fn resolve_linked_dock_peer(
    identity_links: &HashMap<String, Vec<String>>,
    source_candidates: &[String],
    target_channel: &str,
) -> Option<String> {
    let target_prefix = format!("{}:", target_channel.trim().to_lowercase());
    for ids in identity_links.values() {
        let normalized: Vec<String> = ids
            .iter()
            .map(|id| id.trim().to_lowercase())
            .filter(|id| !id.is_empty())
            .collect();
        if !normalized.iter().any(|id| source_candidates.contains(id)) {
            continue;
        }
        for id in ids {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.to_lowercase().starts_with(&target_prefix) {
                let peer = trimmed[target_prefix.len()..].trim();
                if !peer.is_empty() {
                    return Some(peer.to_string());
                }
            }
        }
    }
    None
}

/// Resolve a dock command into a decision + optional route mutation.
pub fn resolve_dock(req: &DockRequest) -> DockDecision {
    let target = req.target_channel.trim().to_lowercase();

    if !req.is_authorized_sender {
        return DockDecision::Unauthorized;
    }

    let source = req.source_channel.trim().to_lowercase();
    if source == target {
        return DockDecision::AlreadyDocked {
            reply: format!("Already docked to {target}."),
        };
    }

    // Direct-chat-only gate.
    if req.chat_type.trim().to_lowercase() != "direct" {
        return DockDecision::NotDirectChat {
            reply: format!(
                "Cannot dock to {target}: docking is only available from direct chats."
            ),
        };
    }

    let candidates = source_identity_candidates(&source, &req.sender_candidates);
    if candidates.is_empty() {
        return DockDecision::MissingSenderId {
            reply: format!("Cannot dock to {target}: sender id is unavailable."),
        };
    }

    let peer = req
        .identity_links
        .as_ref()
        .and_then(|links| resolve_linked_dock_peer(links, &candidates, &target));
    let Some(peer) = peer else {
        return DockDecision::NoLinkedIdentity {
            reply: format!(
                "Cannot dock to {target}: add this sender and a {target}:... peer to session.identityLinks."
            ),
        };
    };

    if !req.has_session_entry {
        return DockDecision::NoSessionEntry {
            reply: format!("Cannot dock to {target}: no active session entry was found."),
        };
    }

    let account_id = {
        let a = req.target_default_account_id.trim();
        if a.is_empty() {
            "default".to_string()
        } else {
            a.to_string()
        }
    };

    DockDecision::Docked {
        mutation: DockRouteMutation {
            last_channel: target.clone(),
            last_to: peer,
            last_account_id: account_id,
        },
        reply: format!("Docked replies to {target}."),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn links(groups: &[&[&str]]) -> HashMap<String, Vec<String>> {
        groups
            .iter()
            .enumerate()
            .map(|(i, ids)| {
                (
                    format!("g{i}"),
                    ids.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    fn base_request() -> DockRequest {
        DockRequest {
            target_channel: "telegram".into(),
            source_channel: "discord".into(),
            chat_type: "direct".into(),
            is_authorized_sender: true,
            sender_candidates: vec!["U123".into()],
            identity_links: Some(links(&[&["discord:u123", "telegram:555"]])),
            has_session_entry: true,
            target_default_account_id: "default".into(),
        }
    }

    #[test]
    fn parse_dock_commands() {
        assert_eq!(parse_dock_command("/dock-telegram"), Some("telegram".into()));
        assert_eq!(
            parse_dock_command("/dock-telegram@MyBot"),
            Some("telegram".into())
        );
        assert_eq!(parse_dock_command("/dock-"), None);
        assert_eq!(parse_dock_command("/dock"), None);
        assert_eq!(parse_dock_command("dock-telegram"), None);
        assert_eq!(parse_dock_command("/dock-tele gram"), Some("tele".into()));
    }

    #[test]
    fn successful_dock_mutates_route() {
        let req = base_request();
        match resolve_dock(&req) {
            DockDecision::Docked { mutation, reply } => {
                assert_eq!(mutation.last_channel, "telegram");
                assert_eq!(mutation.last_to, "555");
                assert_eq!(mutation.last_account_id, "default");
                assert!(reply.contains("Docked replies to telegram"));
            }
            other => panic!("expected Docked, got {other:?}"),
        }
    }

    #[test]
    fn dock_only_from_direct_chats() {
        let mut req = base_request();
        req.chat_type = "group".into();
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::NotDirectChat { .. }
        ));
    }

    #[test]
    fn unauthorized_sender_swallowed() {
        let mut req = base_request();
        req.is_authorized_sender = false;
        assert_eq!(resolve_dock(&req), DockDecision::Unauthorized);
    }

    #[test]
    fn already_docked_same_channel() {
        let mut req = base_request();
        req.source_channel = "telegram".into();
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::AlreadyDocked { .. }
        ));
    }

    #[test]
    fn missing_sender_and_links() {
        let mut req = base_request();
        req.sender_candidates = vec!["  ".into()];
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::MissingSenderId { .. }
        ));

        let mut req = base_request();
        req.identity_links = Some(links(&[&["discord:someoneelse", "telegram:555"]]));
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::NoLinkedIdentity { .. }
        ));

        let mut req = base_request();
        req.identity_links = None;
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::NoLinkedIdentity { .. }
        ));
    }

    #[test]
    fn scoped_and_raw_candidates_match_case_insensitively() {
        // Raw candidate matches an unscoped identity id.
        let mut req = base_request();
        req.identity_links = Some(links(&[&["u123", "TELEGRAM:AbC"]]));
        match resolve_dock(&req) {
            DockDecision::Docked { mutation, .. } => {
                // Peer casing preserved from the link entry.
                assert_eq!(mutation.last_to, "AbC");
            }
            other => panic!("expected Docked, got {other:?}"),
        }
    }

    #[test]
    fn no_session_entry() {
        let mut req = base_request();
        req.has_session_entry = false;
        assert!(matches!(
            resolve_dock(&req),
            DockDecision::NoSessionEntry { .. }
        ));
    }

    #[test]
    fn empty_account_defaults() {
        let mut req = base_request();
        req.target_default_account_id = " ".into();
        match resolve_dock(&req) {
            DockDecision::Docked { mutation, .. } => {
                assert_eq!(mutation.last_account_id, "default");
            }
            other => panic!("expected Docked, got {other:?}"),
        }
    }
}
